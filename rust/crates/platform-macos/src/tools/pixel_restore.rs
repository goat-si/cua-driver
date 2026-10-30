//! Admission and detached pixel-focus restoration coordination.
use futures_util::FutureExt;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Mutex, OnceLock,
    },
};
use tokio::sync::{watch, Mutex as AsyncMutex};

type ProcessStamp = crate::ax::enablement::ProcessStartStamp;

pub(super) struct Fence {
    epoch: AtomicU64,
    in_flight: AtomicUsize,
    commit: AsyncMutex<()>,
    pending: Mutex<HashMap<(i32, u64), usize>>,
    obligations: Mutex<HashMap<i32, Obligation>>,
    superseded: Mutex<HashSet<(i32, u64)>>,
    outcomes: Mutex<HashMap<i32, VecDeque<(ProcessStamp, serde_json::Value)>>>,
    identity: fn(i32) -> Option<ProcessStamp>,
    epochs: watch::Sender<u64>,
}

#[derive(Clone, Debug)]
struct Obligation {
    epoch: u64,
    prior: Option<i32>,
    targets: Vec<(i32, Option<u32>, Option<ProcessStamp>)>,
}
struct RestoreTarget {
    pid: i32,
    window_id: Option<u32>,
    stamp: Option<ProcessStamp>,
}

impl Default for Fence {
    fn default() -> Self {
        Self {
            epoch: AtomicU64::new(0),
            in_flight: AtomicUsize::new(0),
            commit: AsyncMutex::new(()),
            pending: Mutex::new(HashMap::new()),
            obligations: Mutex::new(HashMap::new()),
            superseded: Mutex::new(HashSet::new()),
            outcomes: Mutex::new(HashMap::new()),
            identity: crate::ax::enablement::process_start_stamp,
            epochs: watch::channel(0).0,
        }
    }
}

impl Fence {
    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }
    fn bump_epoch(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::SeqCst).wrapping_add(1)
    }
    pub(super) async fn admit_action(&self, tracked: bool) -> (u64, InFlight<'_>) {
        let epoch = self.bump_epoch();
        self.notify_epoch_change();
        let guard = self.commit.lock().await;
        if tracked {
            self.in_flight.fetch_add(1, Ordering::SeqCst);
        }
        drop(guard);
        (
            epoch,
            InFlight {
                fence: self,
                tracked,
            },
        )
    }
    fn record_outcome(
        &self,
        pid: i32,
        stamp: Option<ProcessStamp>,
        prior: Option<i32>,
        status: &str,
        reason: Option<&str>,
    ) {
        let Some(stamp) = stamp else {
            tracing::debug!(pid, "cannot bind restoration outcome to process");
            return;
        };
        let mut note = serde_json::json!({"status": status, "prior_pid": prior});
        if let Some(reason) = reason {
            note["reason"] = reason.into();
        }
        self.outcomes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(pid)
            .or_default()
            .push_back((stamp, note));
    }
    fn take_outcome(&self, pid: i32) -> Option<serde_json::Value> {
        if !self
            .outcomes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&pid)
        {
            return None;
        }
        self.take_outcome_for(pid, (self.identity)(pid))
    }
    fn take_outcome_for(
        &self,
        pid: i32,
        current: Option<ProcessStamp>,
    ) -> Option<serde_json::Value> {
        let mut outcomes = self.outcomes.lock().unwrap_or_else(|e| e.into_inner());
        let queue = outcomes.get_mut(&pid)?;
        queue.retain(|(stamp, _)| Some(*stamp) == current && current.is_some());
        let note = queue.pop_front().map(|(_, note)| note);
        if queue.is_empty() {
            outcomes.remove(&pid);
        }
        note
    }
    fn chain_target(&self, root: i32, epoch: u64, front: Option<i32>) -> (i32, Option<u32>) {
        let obligations = self.obligations.lock().unwrap_or_else(|e| e.into_inner());
        let Some(chain) = obligations.get(&root).filter(|chain| chain.epoch == epoch) else {
            return (root, None);
        };
        chain
            .targets
            .iter()
            .copied()
            .find(|(pid, _, _)| Some(*pid) == front)
            .or_else(|| chain.targets.last().copied())
            .map(|(pid, wid, _)| (pid, wid))
            .unwrap_or((root, None))
    }
    fn outcome_identity(&self, root: i32, epoch: u64) -> (i32, Option<ProcessStamp>) {
        let obligations = self.obligations.lock().unwrap_or_else(|e| e.into_inner());
        obligations
            .get(&root)
            .filter(|chain| chain.epoch == epoch)
            .and_then(|chain| chain.targets.last())
            .map_or((root, None), |(pid, _, stamp)| (*pid, *stamp))
    }
    fn record_chain_outcome(
        &self,
        root: i32,
        epoch: u64,
        prior: Option<i32>,
        status: &str,
        reason: Option<&str>,
    ) {
        let (pid, stamp) = self.outcome_identity(root, epoch);
        self.record_outcome(pid, stamp, prior, status, reason);
    }
    fn is_pending_target(&self, pid: i32) -> bool {
        self.obligations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|chain| chain.targets.iter().any(|(target, _, _)| *target == pid))
    }
    fn superseded(&self, pid: i32, epoch: u64) -> bool {
        self.superseded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&(pid, epoch))
    }
    fn notify_epoch_change(&self) {
        self.epochs
            .send_modify(|version| *version = version.wrapping_add(1));
    }
}
pub(super) struct InFlight<'a> {
    fence: &'a Fence,
    tracked: bool,
}
impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if self.tracked {
            self.fence.in_flight.fetch_sub(1, Ordering::SeqCst);
            self.fence.notify_epoch_change();
        }
    }
}

struct Pending<'a> {
    fence: &'a Fence,
    pid: i32,
    epoch: u64,
}
impl Drop for Pending<'_> {
    fn drop(&mut self) {
        let mut pending = self.fence.pending.lock().unwrap_or_else(|e| e.into_inner());
        let key = (self.pid, self.epoch);
        if let Some(count) = pending.get_mut(&key) {
            *count -= 1;
            if *count == 0 {
                pending.remove(&key);
            }
        }
        self.fence
            .superseded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
        let mut obligations = self
            .fence
            .obligations
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if obligations
            .get(&self.pid)
            .is_some_and(|obligation| obligation.epoch == self.epoch)
        {
            obligations.remove(&self.pid);
        }
    }
}
static FENCE: OnceLock<Fence> = OnceLock::new();
fn global() -> &'static Fence {
    FENCE.get_or_init(Fence::default)
}
tokio::task_local! { static ACTION_EPOCH: (String, u64); }
pub(super) fn current_epoch() -> Option<u64> {
    ACTION_EPOCH.try_with(|(_, epoch)| *epoch).ok()
}
pub(super) fn current_is(tool: &str) -> bool {
    ACTION_EPOCH
        .try_with(|(name, _)| name == tool)
        .unwrap_or(false)
}

struct AdmittedTool(Box<dyn cua_driver_core::tool::Tool>);

#[async_trait::async_trait]
impl cua_driver_core::tool::Tool for AdmittedTool {
    fn def(&self) -> &cua_driver_core::tool::ToolDef {
        self.0.def()
    }
    fn has_independent_input_lane(&self, args: &serde_json::Value) -> bool {
        self.0.has_independent_input_lane(args)
    }
    async fn protected_resource_ownership(
        &self,
        adapter: &str,
        args: &serde_json::Value,
    ) -> cua_driver_core::tool::ProtectedResourceOwnership {
        self.0.protected_resource_ownership(adapter, args).await
    }
    async fn protected_resource_scope(
        &self,
        adapter: &str,
        args: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>, String> {
        self.0.protected_resource_scope(adapter, args).await
    }
    async fn validate_protected_resource_scope(
        &self,
        adapter: &str,
        args: &serde_json::Value,
        approved: &serde_json::Value,
    ) -> Result<(), String> {
        self.0
            .validate_protected_resource_scope(adapter, args, approved)
            .await
    }
    async fn invoke(&self, args: serde_json::Value) -> cua_driver_core::protocol::ToolResult {
        invoke_with_outcome(global(), self.0.as_ref(), args).await
    }
}

async fn invoke_with_outcome(
    fence: &Fence,
    tool: &dyn cua_driver_core::tool::Tool,
    args: serde_json::Value,
) -> cua_driver_core::protocol::ToolResult {
    let pid = args
        .get("pid")
        .and_then(serde_json::Value::as_i64)
        .and_then(|p| i32::try_from(p).ok());
    let page_observation = tool.def().name == "page"
        && matches!(
            args.get("action").and_then(serde_json::Value::as_str),
            Some("get_text" | "query_dom")
        );
    let admitted = requires_admission(&tool.def().name) && !page_observation;
    let eligible = admitted || page_observation || tool.def().read_only;
    let mut result = if !admitted {
        tool.invoke(args).await
    } else {
        invoke_with_fence(fence, tool, args).await
    };
    let admission_refused = admitted
        && result
            .structured_content
            .as_ref()
            .and_then(|value| value.get("code"))
            .and_then(serde_json::Value::as_str)
            == Some("pixel_restore_pending");
    if let Some(note) = (eligible && !admission_refused)
        .then(|| pid.and_then(|pid| fence.take_outcome(pid)))
        .flatten()
    {
        if result.structured_content.is_none() {
            result.structured_content = Some(serde_json::json!({}));
        }
        if let Some(object) = result
            .structured_content
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
        {
            object.insert("focus_restoration".into(), note.clone());
        }
        result
            .content
            .push(cua_driver_core::protocol::Content::text(format!(
                "focus_restoration: {note}"
            )));
    }
    result
}

pub(crate) fn wrap(
    tool: Box<dyn cua_driver_core::tool::Tool>,
) -> Box<dyn cua_driver_core::tool::Tool> {
    Box::new(AdmittedTool(tool))
}

pub(super) fn requires_admission(name: &str) -> bool {
    matches!(
        name,
        "click"
            | "double_click"
            | "right_click"
            | "drag"
            | "scroll"
            | "type_text"
            | "press_key"
            | "hotkey"
            | "set_value"
            | "select_text"
            | "invoke_menu"
            | "menu_bar_extras"
            | "move_cursor"
            | "page"
            | "browser_click"
            | "browser_type"
            | "browser_select"
            | "browser_dialog"
            | "browser_pointer"
            | "browser_set_input_files"
            | "browser_download"
            | "browser_prepare"
            | "browser_navigate"
            | "bring_to_front"
            | "launch_app"
    )
}

pub(super) async fn invoke(
    tool: &dyn cua_driver_core::tool::Tool,
    args: serde_json::Value,
) -> cua_driver_core::protocol::ToolResult {
    invoke_with_fence(global(), tool, args).await
}

async fn invoke_with_fence(
    fence: &Fence,
    tool: &dyn cua_driver_core::tool::Tool,
    args: serde_json::Value,
) -> cua_driver_core::protocol::ToolResult {
    let name = tool.def().name.clone();
    // CDP commands do not directly manipulate the native frontmost app.
    // Preparation may launch a process, so it remains tracked.
    let tracked = !(name == "page" || (name.starts_with("browser_") && name != "browser_prepare"));
    let Ok((epoch, _in_flight)) = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        fence.admit_action(tracked),
    )
    .await
    else {
        return super::pre_dispatch_refusal(
            "pixel_restore_pending",
            "A previous pixel focus restoration is still pending; no input was sent.",
        );
    };
    ACTION_EPOCH.scope((name, epoch), tool.invoke(args)).await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RestoreCheck {
    Valid,
    Retry,
    Abandon(&'static str),
}
impl RestoreCheck {
    fn timeout_reason(self) -> &'static str {
        match self {
            Self::Abandon(reason) => reason,
            Self::Valid | Self::Retry => "timeout",
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct RestoreBaseline {
    idle: Option<u64>,
    started: std::time::Instant,
}

impl RestoreBaseline {
    pub(super) fn capture() -> Self {
        Self {
            idle: super::physical_input_idle_ms(),
            started: std::time::Instant::now(),
        }
    }

    fn valid_sample(
        &self,
        idle: Option<u64>,
        elapsed: u64,
        front: Option<i32>,
        expected: Option<i32>,
        target: i32,
    ) -> bool {
        if front != expected && front != Some(target) {
            return false;
        }
        match (self.idle, idle) {
            (Some(before), Some(after)) => {
                after.saturating_add(2) >= before.saturating_add(elapsed)
            }
            _ => true,
        }
    }

    fn valid_sample_with_driver_hid(
        &self,
        idle: Option<u64>,
        elapsed: u64,
        front: Option<i32>,
        expected: Option<i32>,
        target: i32,
        own_hid: bool,
    ) -> bool {
        if own_hid {
            front == expected || front == Some(target)
        } else {
            self.valid_sample(idle, elapsed, front, expected, target)
        }
    }

    fn abandon_reason(&self, expected: Option<i32>, target: i32) -> RestoreCheck {
        let front = crate::apps::frontmost_pid();
        let idle = super::physical_input_idle_ms();
        let now = std::time::Instant::now();
        let elapsed =
            u64::try_from(now.duration_since(self.started).as_millis()).unwrap_or(u64::MAX);
        let sample = self.abandon_sample(expected, target, front, idle, elapsed);
        if matches!(
            sample,
            RestoreCheck::Retry | RestoreCheck::Abandon("third_app")
        ) {
            return sample;
        }
        match crate::input::hid_history::quiet_since(self.started, self.idle, now, idle) {
            Some(true) => RestoreCheck::Valid,
            Some(false) => RestoreCheck::Abandon("user_input"),
            None => RestoreCheck::Retry,
        }
    }

    fn abandon_sample(
        &self,
        expected: Option<i32>,
        target: i32,
        front: Option<i32>,
        idle: Option<u64>,
        elapsed: u64,
    ) -> RestoreCheck {
        if expected.is_none() || front.is_none() {
            return RestoreCheck::Retry;
        }
        if front != expected && front != Some(target) {
            return RestoreCheck::Abandon("third_app");
        }
        if self.idle.is_none() || idle.is_none() {
            return RestoreCheck::Retry;
        }
        if !self.valid_sample_with_driver_hid(idle, elapsed, front, expected, target, false) {
            return RestoreCheck::Abandon("user_input");
        }
        RestoreCheck::Valid
    }

    pub(super) fn valid_after_reactivation(&self, expected: Option<i32>, target: i32) -> bool {
        let idle = super::physical_input_idle_ms();
        let front = crate::apps::frontmost_pid();
        let now = std::time::Instant::now();
        let elapsed =
            u64::try_from(now.duration_since(self.started).as_millis()).unwrap_or(u64::MAX);
        crate::input::hid_history::quiet_since(self.started, self.idle, now, idle) == Some(true)
            && self.valid_sample_with_driver_hid(idle, elapsed, front, expected, target, true)
    }
}

pub(super) fn spawn_restore<F, Fut>(
    pid: i32,
    epoch: u64,
    expected_front: Option<i32>,
    baseline: RestoreBaseline,
    window_id: Option<u32>,
    target_stamp: Option<ProcessStamp>,
    work: F,
) where
    F: FnOnce(i32, Option<u32>) -> Fut + Send + 'static,
    Fut: Future<Output = bool> + Send + 'static,
{
    spawn_restore_with_window(
        global(),
        RestoreTarget {
            pid,
            window_id,
            stamp: target_stamp,
        },
        epoch,
        expected_front,
        move || {
            let front = crate::apps::frontmost_pid();
            let (target, _) = global().chain_target(pid, epoch, front);
            if front.is_some_and(|front| {
                Some(front) != expected_front
                    && front != target
                    && global().is_pending_target(front)
            }) {
                RestoreCheck::Retry
            } else {
                baseline.abandon_reason(expected_front, target)
            }
        },
        move || {
            let (target, wid) = global().chain_target(pid, epoch, crate::apps::frontmost_pid());
            work(target, wid)
        },
    );
}

#[cfg(test)]
pub(super) fn spawn_restore_with<F, Fut, V>(
    fence: &'static Fence,
    pid: i32,
    epoch: u64,
    prior: Option<i32>,
    valid: V,
    work: F,
) where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = bool> + Send + 'static,
    V: Fn() -> RestoreCheck + Send + 'static,
{
    spawn_restore_with_window(
        fence,
        RestoreTarget {
            pid,
            window_id: None,
            stamp: (fence.identity)(pid),
        },
        epoch,
        prior,
        valid,
        work,
    );
}

fn spawn_restore_with_window<F, Fut, V>(
    fence: &'static Fence,
    target: RestoreTarget,
    epoch: u64,
    prior: Option<i32>,
    valid: V,
    work: F,
) where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = bool> + Send + 'static,
    V: Fn() -> RestoreCheck + Send + 'static,
{
    let RestoreTarget {
        pid,
        window_id,
        stamp: target_stamp,
    } = target;
    let pending = {
        let mut entries = fence.pending.lock().unwrap_or_else(|e| e.into_inner());
        let mut obligations = fence.obligations.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(parent) = prior.and_then(|previous| {
            obligations.values_mut().find(|root| {
                root.epoch < epoch
                    && root
                        .targets
                        .iter()
                        .any(|(target, _, _)| *target == previous)
            })
        }) {
            parent.targets.push((pid, window_id, target_stamp));
            fence.notify_epoch_change();
            return;
        }
        if obligations
            .get(&pid)
            .is_some_and(|existing| existing.epoch <= epoch)
        {
            return;
        }
        let mut superseded = fence.superseded.lock().unwrap_or_else(|e| e.into_inner());
        let mut targets = vec![(pid, window_id, target_stamp)];
        let descendants: Vec<i32> = obligations
            .iter()
            .filter_map(|(root, child)| {
                (child.epoch > epoch && child.prior == Some(pid)).then_some(*root)
            })
            .collect();
        for descendant in descendants {
            if let Some(child) = obligations.remove(&descendant) {
                superseded.insert((descendant, child.epoch));
                targets.extend(child.targets);
            }
        }
        obligations.insert(
            pid,
            Obligation {
                epoch,
                prior,
                targets,
            },
        );
        superseded.extend(
            entries
                .keys()
                .filter(|(existing_pid, existing_epoch)| {
                    *existing_pid == pid && *existing_epoch > epoch
                })
                .copied(),
        );
        entries.insert((pid, epoch), 1);
        Pending { fence, pid, epoch }
    };
    let mut epochs = fence.epochs.subscribe();
    fence.notify_epoch_change();
    let deadline_at = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    tokio::spawn(async move {
        let _pending = pending;
        let deadline = tokio::time::sleep_until(deadline_at);
        tokio::pin!(deadline);
        let mut work = Some(work);
        loop {
            let observed = fence.epoch();
            if fence.in_flight.load(Ordering::SeqCst) != 0 {
                tokio::select! {
                    _ = &mut deadline => { tracing::debug!(pid, "pixel focus restoration timed out"); if !fence.superseded(pid, epoch) { fence.record_chain_outcome(pid, epoch, prior, "abandoned", Some(valid().timeout_reason())); } return; }
                    changed = epochs.changed() => if changed.is_err() { return; },
                }
                continue;
            }
            tokio::select! {
                _ = &mut deadline => { tracing::debug!(pid, "pixel focus restoration timed out"); if !fence.superseded(pid, epoch) { fence.record_chain_outcome(pid, epoch, prior, "abandoned", Some(valid().timeout_reason())); } return; }
                changed = epochs.changed() => { if changed.is_err() { return; } continue; }
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
            }
            let guard = tokio::select! {
                _ = &mut deadline => { tracing::debug!(pid, "pixel focus restoration timed out"); if !fence.superseded(pid, epoch) { fence.record_chain_outcome(pid, epoch, prior, "abandoned", Some(valid().timeout_reason())); } return; }
                guard = fence.commit.lock() => guard,
            };
            if fence.epoch() != observed || fence.in_flight.load(Ordering::SeqCst) != 0 {
                drop(guard);
                continue;
            }
            if fence.superseded(pid, epoch) {
                return;
            }
            match valid() {
                RestoreCheck::Retry => {
                    drop(guard);
                    continue;
                }
                RestoreCheck::Abandon(reason) => {
                    fence.record_chain_outcome(pid, epoch, prior, "abandoned", Some(reason))
                }
                RestoreCheck::Valid => {
                    let result = std::panic::AssertUnwindSafe(async {
                        work.take().expect("restore called once")().await
                    })
                    .catch_unwind()
                    .await;
                    if result.is_err() {
                        tracing::error!(pid, "pixel focus restoration panicked");
                        fence.record_chain_outcome(pid, epoch, prior, "abandoned", Some("panic"));
                    } else if matches!(result, Ok(true)) {
                        fence.record_chain_outcome(pid, epoch, prior, "restored", None);
                    } else {
                        // The hand-back could not be confirmed. Keep the
                        // obligation until a takeover or its original deadline;
                        // never post the focus operation a second time.
                        drop(guard);
                        loop {
                            if fence.superseded(pid, epoch) {
                                return;
                            }
                            if let RestoreCheck::Abandon(reason) = valid() {
                                fence.record_chain_outcome(
                                    pid,
                                    epoch,
                                    prior,
                                    "abandoned",
                                    Some(reason),
                                );
                                return;
                            }
                            tokio::select! {
                                _ = &mut deadline => {
                                    fence.record_chain_outcome(pid, epoch, prior, "abandoned", Some(valid().timeout_reason()));
                                    return;
                                }
                                changed = epochs.changed() => if changed.is_err() { return; },
                                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
                            }
                        }
                    }
                }
            }
            return;
        }
    });
}

#[cfg(test)]
#[path = "pixel_restore_tests.rs"]
mod tests;
