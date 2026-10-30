//! Admission and detached pixel-focus restoration coordination.
use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Mutex, OnceLock,
    },
};
use tokio::sync::{watch, Mutex as AsyncMutex};

pub(super) struct Fence {
    epoch: AtomicU64,
    in_flight: AtomicUsize,
    commit: AsyncMutex<()>,
    pending: Mutex<HashMap<(i32, u64), usize>>,
    obligations: Mutex<HashMap<i32, (u64, Option<i32>)>>,
    superseded: Mutex<HashSet<(i32, u64)>>,
    outcomes: Mutex<HashMap<i32, VecDeque<serde_json::Value>>>,
    changes: watch::Sender<u64>,
    epochs: watch::Sender<u64>,
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
            changes: watch::channel(0).0,
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
    pub(super) fn bump(&self) -> u64 {
        let epoch = self.bump_epoch();
        self.notify_epoch_change();
        epoch
    }
    #[cfg(test)]
    pub(super) async fn admit(&self) -> u64 {
        let epoch = self.bump_epoch();
        let lock = self.commit.lock();
        tokio::pin!(lock);
        let mut notified = false;
        // Try the commit boundary immediately after the bump. If an older
        // restore already owns it, wake stale pending restores while this
        // admission waits; otherwise unlock before notifying them.
        let guard = std::future::poll_fn(|cx| {
            let result = lock.as_mut().poll(cx);
            if result.is_pending() && !notified {
                self.notify_epoch_change();
                notified = true;
            }
            result
        })
        .await;
        drop(guard);
        if !notified {
            self.notify_epoch_change();
        }
        epoch
    }
    pub(super) async fn admit_action(&self) -> (u64, InFlight<'_>) {
        let epoch = self.bump_epoch();
        self.notify_epoch_change();
        let guard = self.commit.lock().await;
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        drop(guard);
        (epoch, InFlight(self))
    }
    fn record_outcome(&self, pid: i32, prior: Option<i32>, status: &str, reason: Option<&str>) {
        let mut note = serde_json::json!({"status": status, "prior_pid": prior});
        if let Some(reason) = reason {
            note["reason"] = reason.into();
        }
        self.outcomes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(pid)
            .or_default()
            .push_back(note);
    }
    fn take_outcome(&self, pid: i32) -> Option<serde_json::Value> {
        let mut outcomes = self.outcomes.lock().unwrap_or_else(|e| e.into_inner());
        let queue = outcomes.get_mut(&pid)?;
        let note = queue.pop_front();
        if queue.is_empty() {
            outcomes.remove(&pid);
        }
        note
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
    #[cfg(test)]
    async fn commit<F, W, Fut>(&self, epoch: u64, front_unchanged: F, work: W)
    where
        F: FnOnce() -> bool,
        W: FnOnce() -> Fut,
        Fut: Future<Output = ()>,
    {
        let _guard = self.commit.lock().await;
        if self.epoch() != epoch || !front_unchanged() {
            tracing::debug!(epoch, "dropping stale pixel focus restoration");
            return;
        }
        work().await;
    }
    #[cfg(test)]
    fn register(&self, pid: i32, epoch: u64) -> Pending<'_> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        *pending.entry((pid, epoch)).or_default() += 1;
        Pending {
            fence: self,
            pid,
            epoch,
        }
    }
    #[cfg(test)]
    fn is_pending(&self, pid: i32) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .any(|(pending_pid, _)| *pending_pid == pid)
    }
    #[cfg(test)]
    async fn await_pending(&self, pid: i32) -> bool {
        self.await_matching_pending(Some(pid)).await
    }
    #[cfg(test)]
    async fn await_matching_pending(&self, pid: Option<i32>) -> bool {
        let mut changed = self.changes.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while match pid {
                Some(pid) => self.is_pending(pid),
                None => !self
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_empty(),
            } {
                if changed.changed().await.is_err() {
                    return false;
                }
            }
            true
        })
        .await
        .unwrap_or(false)
    }
}
pub(super) struct InFlight<'a>(&'a Fence);
impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.0.notify_epoch_change();
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
            .is_some_and(|(epoch, _)| *epoch == self.epoch)
        {
            obligations.remove(&self.pid);
        }
        self.fence
            .changes
            .send_modify(|version| *version = version.wrapping_add(1));
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
pub(super) fn user_takeover() {
    global().bump();
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
    let Ok((epoch, _in_flight)) =
        tokio::time::timeout(std::time::Duration::from_secs(2), fence.admit_action()).await
    else {
        return super::pre_dispatch_refusal(
            "pixel_restore_pending",
            "A previous pixel focus restoration is still pending; no input was sent.",
        );
    };
    let name = tool.def().name.clone();
    ACTION_EPOCH.scope((name, epoch), tool.invoke(args)).await
}

#[cfg(test)]
async fn wait_for_settle_or_stale(
    fence: &Fence,
    epoch: u64,
    mut epochs: watch::Receiver<u64>,
) -> bool {
    let settle = tokio::time::sleep(std::time::Duration::from_millis(50));
    tokio::pin!(settle);
    loop {
        if fence.epoch() != epoch {
            tracing::debug!(
                epoch,
                "dropping stale pixel focus restoration before commit"
            );
            return false;
        }
        tokio::select! {
            _ = &mut settle => break,
            changed = epochs.changed() => {
                if changed.is_err() { return false; }
            }
        }
    }
    if fence.epoch() != epoch {
        tracing::debug!(
            epoch,
            "dropping stale pixel focus restoration before commit"
        );
        return false;
    }
    true
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
    epoch: u64,
}

impl RestoreBaseline {
    pub(super) fn capture(epoch: u64) -> Self {
        Self {
            idle: super::physical_input_idle_ms(),
            started: std::time::Instant::now(),
            epoch,
        }
    }

    fn valid_sample(
        &self,
        idle: Option<u64>,
        elapsed: u64,
        front: Option<i32>,
        expected: Option<i32>,
        target: i32,
        epoch: u64,
    ) -> bool {
        if self.epoch != epoch || (front != expected && front != Some(target)) {
            return false;
        }
        match (self.idle, idle) {
            (Some(before), Some(after)) => {
                after.saturating_add(2) >= before.saturating_add(elapsed)
            }
            _ => true,
        }
    }

    fn abandon_reason(&self, expected: Option<i32>, target: i32) -> RestoreCheck {
        let elapsed = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.abandon_sample(
            expected,
            target,
            crate::apps::frontmost_pid(),
            super::physical_input_idle_ms(),
            elapsed,
        )
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
        if !self.valid_sample(idle, elapsed, front, expected, target, self.epoch) {
            return RestoreCheck::Abandon("user_input");
        }
        RestoreCheck::Valid
    }

    pub(super) fn valid_after_reactivation(&self, expected: Option<i32>, target: i32) -> bool {
        self.valid_during_restore(expected, target, true)
    }

    fn restore_epoch(&self, observed: u64, reactivated: bool) -> u64 {
        // The commit lock still excludes admission after reactivation. An
        // admission may bump the epoch while waiting, but cannot dispatch.
        if reactivated {
            self.epoch
        } else {
            observed
        }
    }

    fn valid_during_restore(&self, expected: Option<i32>, target: i32, reactivated: bool) -> bool {
        let elapsed = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let idle = super::physical_input_idle_ms();
        if self.idle.is_none() || idle.is_none() {
            return false;
        }
        let valid = self.valid_sample(
            idle,
            elapsed,
            crate::apps::frontmost_pid(),
            expected,
            target,
            self.restore_epoch(global().epoch(), reactivated),
        );
        if !valid && global().epoch() == self.epoch {
            global().bump();
        }
        valid
    }
}

pub(super) fn spawn_restore<F, Fut>(
    pid: i32,
    epoch: u64,
    expected_front: Option<i32>,
    baseline: RestoreBaseline,
    work: F,
) where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = bool> + Send + 'static,
{
    spawn_restore_with(
        global(),
        pid,
        epoch,
        expected_front,
        move || baseline.abandon_reason(expected_front, pid),
        work,
    );
}

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
    let pending = {
        let mut entries = fence.pending.lock().unwrap_or_else(|e| e.into_inner());
        let mut obligations = fence.obligations.lock().unwrap_or_else(|e| e.into_inner());
        if obligations
            .get(&pid)
            .is_some_and(|(existing_epoch, _)| *existing_epoch <= epoch)
        {
            return;
        }
        obligations.insert(pid, (epoch, prior));
        let mut superseded = fence.superseded.lock().unwrap_or_else(|e| e.into_inner());
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
                    _ = &mut deadline => { tracing::debug!(pid, "pixel focus restoration timed out"); if !fence.superseded(pid, epoch) { fence.record_outcome(pid, prior, "abandoned", Some(valid().timeout_reason())); } return; }
                    changed = epochs.changed() => if changed.is_err() { return; },
                }
                continue;
            }
            tokio::select! {
                _ = &mut deadline => { tracing::debug!(pid, "pixel focus restoration timed out"); if !fence.superseded(pid, epoch) { fence.record_outcome(pid, prior, "abandoned", Some(valid().timeout_reason())); } return; }
                changed = epochs.changed() => { if changed.is_err() { return; } continue; }
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
            }
            let guard = tokio::select! {
                _ = &mut deadline => { tracing::debug!(pid, "pixel focus restoration timed out"); if !fence.superseded(pid, epoch) { fence.record_outcome(pid, prior, "abandoned", Some(valid().timeout_reason())); } return; }
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
                    fence.record_outcome(pid, prior, "abandoned", Some(reason))
                }
                RestoreCheck::Valid => {
                    if work.take().expect("restore called once")().await {
                        fence.record_outcome(pid, prior, "restored", None);
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
                                fence.record_outcome(pid, prior, "abandoned", Some(reason));
                                return;
                            }
                            tokio::select! {
                                _ = &mut deadline => {
                                    fence.record_outcome(pid, prior, "abandoned", Some(valid().timeout_reason()));
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
