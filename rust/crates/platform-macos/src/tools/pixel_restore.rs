//! Admission and detached pixel-focus restoration coordination.
use std::{
    collections::HashMap,
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
};
use tokio::sync::{watch, Mutex as AsyncMutex};

pub(super) struct Fence {
    epoch: AtomicU64,
    commit: AsyncMutex<()>,
    pending: Mutex<HashMap<(i32, u64), usize>>,
    changes: watch::Sender<u64>,
    epochs: watch::Sender<u64>,
}

impl Default for Fence {
    fn default() -> Self {
        Self {
            epoch: AtomicU64::new(0),
            commit: AsyncMutex::new(()),
            pending: Mutex::new(HashMap::new()),
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
    fn notify_epoch_change(&self) {
        self.epochs
            .send_modify(|version| *version = version.wrapping_add(1));
    }
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
    fn register(&self, pid: i32, epoch: u64) -> Pending<'_> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        *pending.entry((pid, epoch)).or_default() += 1;
        Pending {
            fence: self,
            pid,
            epoch,
        }
    }
    fn is_pending(&self, pid: i32) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .any(|(pending_pid, _)| *pending_pid == pid)
    }
    async fn await_pending(&self, pid: i32) -> bool {
        self.await_matching_pending(Some(pid)).await
    }
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
        if self.0.def().name == "page"
            && matches!(
                args.get("action").and_then(serde_json::Value::as_str),
                Some("get_text" | "query_dom")
            )
        {
            return self.0.invoke(args).await;
        }
        invoke(self.0.as_ref(), args).await
    }
}

pub(crate) fn wrap(
    tool: Box<dyn cua_driver_core::tool::Tool>,
) -> Box<dyn cua_driver_core::tool::Tool> {
    if requires_admission(&tool.def().name) {
        Box::new(AdmittedTool(tool))
    } else {
        tool
    }
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
    let pid = args
        .get("pid")
        .and_then(serde_json::Value::as_i64)
        .and_then(|p| i32::try_from(p).ok());
    let admission = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let epoch = fence.admit().await;
        let ready = match pid {
            Some(pid) => fence.await_pending(pid).await,
            None => fence.await_matching_pending(None).await,
        };
        ready.then_some(epoch)
    })
    .await
    .ok()
    .flatten();
    let Some(epoch) = admission else {
        return super::pre_dispatch_refusal(
            "pixel_restore_pending",
            "A previous pixel focus restoration is still pending; no input was sent.",
        );
    };
    let name = tool.def().name.clone();
    ACTION_EPOCH.scope((name, epoch), tool.invoke(args)).await
}

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

    pub(super) fn valid(&self, expected: Option<i32>, target: i32) -> bool {
        let elapsed = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let valid = self.valid_sample(
            super::physical_input_idle_ms(),
            elapsed,
            crate::apps::frontmost_pid(),
            expected,
            target,
            global().epoch(),
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
    Fut: Future<Output = ()> + Send + 'static,
{
    let fence = global();
    let epochs = fence.epochs.subscribe();
    let pending = fence.register(pid, epoch);
    tokio::spawn(async move {
        let _pending = pending;
        if !wait_for_settle_or_stale(fence, epoch, epochs).await {
            return;
        }
        fence
            .commit(epoch, || baseline.valid(expected_front, pid), work)
            .await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::sync::oneshot;

    struct FakeTool(cua_driver_core::tool::ToolDef);
    impl FakeTool {
        fn new() -> Self {
            Self(cua_driver_core::tool::ToolDef {
                name: "pixel_fake".into(),
                description: String::new(),
                input_schema: serde_json::json!({}),
                read_only: false,
                destructive: false,
                idempotent: false,
                open_world: false,
            })
        }
    }
    #[async_trait::async_trait]
    impl cua_driver_core::tool::Tool for FakeTool {
        fn def(&self) -> &cua_driver_core::tool::ToolDef {
            &self.0
        }
        async fn invoke(&self, _: serde_json::Value) -> cua_driver_core::protocol::ToolResult {
            panic!("input dispatched")
        }
    }

    #[tokio::test(start_paused = true)]
    async fn pixel_admission_lock_has_deadline() {
        let fence = Fence::default();
        let lock = fence.commit.lock().await;
        let tool = FakeTool::new();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_millis(2100),
            invoke_with_fence(&fence, &tool, serde_json::json!({"pid": -2000000010})),
        )
        .await;
        let result = outcome.expect("admission exceeded its two-second deadline");
        let data = result
            .structured_content
            .expect("structured timeout refusal");
        assert_eq!(data["code"], "pixel_restore_pending");
        assert_eq!(data["effect"], "refused");
        assert_eq!(data["dispatched"], false);
        assert!(
            fence.commit.try_lock().is_err(),
            "restoration retains its lock after refusal"
        );
        drop(lock);
        assert!(
            fence.commit.try_lock().is_ok(),
            "timed out admission leaves no lock held"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pixel_timeout_is_structured_refusal() {
        let fence = Fence::default();
        let _pending = fence.register(-2000000011, fence.epoch());
        let result = invoke_with_fence(
            &fence,
            &FakeTool::new(),
            serde_json::json!({"pid": -2000000011}),
        )
        .await;
        let data = result
            .structured_content
            .expect("structured timeout refusal");
        assert_eq!(data["code"], "pixel_restore_pending");
        assert_eq!(data["effect"], "refused");
        assert_eq!(data["dispatched"], false);
    }

    #[test]
    fn pixel_target_front_without_physical_input_allows_restore() {
        let baseline = RestoreBaseline {
            idle: Some(1000),
            started: std::time::Instant::now(),
            epoch: 7,
        };
        assert!(
            baseline.valid_sample(Some(1050), 50, Some(2), Some(1), 2, 7),
            "driver-caused target activation must allow restoration"
        );
    }

    #[test]
    fn pixel_pre_dispatch_baseline_detects_takeover_and_front_change() {
        let baseline = RestoreBaseline {
            idle: Some(1000),
            started: std::time::Instant::now(),
            epoch: 7,
        };
        assert!(baseline.valid_sample(Some(1050), 50, Some(1), Some(1), 2, 7));
        // Input during dispatch reset the idle clock even if the target is now frontmost.
        assert!(!baseline.valid_sample(Some(20), 50, Some(2), Some(1), 2, 7));
        assert!(!baseline.valid_sample(Some(1050), 50, Some(3), Some(1), 2, 7));
        assert!(!baseline.valid_sample(Some(1050), 50, Some(1), Some(1), 2, 8));
        // A second focus post must sample again after the first post or an await.
        assert!(!baseline.valid_sample(Some(1), 70, Some(1), Some(1), 2, 7));
    }

    #[tokio::test(start_paused = true)]
    async fn stale_restore_clears_pending_before_settle_deadline() {
        let fence = global();
        let pid = -2_000_000_000;
        let epoch = fence.admit().await;
        spawn_restore(
            pid,
            epoch,
            None,
            RestoreBaseline::capture(epoch),
            || async { panic!("stale restoration ran") },
        );
        assert!(fence.is_pending(pid));
        fence.admit().await;
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(1),
            fence.await_pending(pid),
        )
        .await;
        assert!(
            matches!(result, Ok(true)),
            "stale pending entry must clear without waiting 50 ms: {result:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn detected_takeover_clears_pending_before_settle_deadline() {
        let fence = global();
        let pid = -1_999_999_999;
        let epoch = fence.admit().await;
        spawn_restore(
            pid,
            epoch,
            None,
            RestoreBaseline::capture(epoch),
            || async { panic!("stale restoration ran") },
        );
        user_takeover();
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(1),
            fence.await_pending(pid),
        )
        .await;
        assert!(
            matches!(result, Ok(true)),
            "takeover must release the pending entry: {result:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stale_pending_clears_while_new_admission_waits_for_commit_lock() {
        let fence = Arc::new(Fence::default());
        let epoch = fence.admit().await;
        let (registered_tx, registered_rx) = oneshot::channel();
        let waiting_fence = fence.clone();
        let restore = tokio::spawn(async move {
            let epochs = waiting_fence.epochs.subscribe();
            let _pending = waiting_fence.register(42, epoch);
            registered_tx.send(()).unwrap();
            assert!(!wait_for_settle_or_stale(&waiting_fence, epoch, epochs).await);
        });
        registered_rx.await.unwrap();
        let (held_tx, held_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let holding_fence = fence.clone();
        let holder = tokio::spawn(async move {
            holding_fence
                .commit(
                    epoch,
                    || true,
                    || async move {
                        held_tx.send(()).unwrap();
                        release_rx.await.unwrap();
                    },
                )
                .await;
        });
        held_rx.await.unwrap();
        let admitting_fence = fence.clone();
        let admission = tokio::spawn(async move { admitting_fence.admit().await });
        while fence.epoch() == epoch {
            tokio::task::yield_now().await;
        }
        assert!(!admission.is_finished());
        let cleared =
            tokio::time::timeout(std::time::Duration::from_millis(1), fence.await_pending(42))
                .await;
        assert!(
            matches!(cleared, Ok(true)),
            "stale entry must clear while admission is blocked"
        );
        release_tx.send(()).unwrap();
        holder.await.unwrap();
        admission.await.unwrap();
        restore.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn uncontested_restoration_runs_once_after_settle() {
        use std::task::Poll;
        let fence = Fence::default();
        let epoch = fence.admit().await;
        let mut wait = Box::pin(wait_for_settle_or_stale(
            &fence,
            epoch,
            fence.epochs.subscribe(),
        ));
        std::future::poll_fn(|cx| {
            assert!(wait.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        tokio::time::advance(std::time::Duration::from_millis(49)).await;
        std::future::poll_fn(|cx| {
            assert!(wait.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        assert!(wait.await);
        let called = AtomicUsize::new(0);
        fence
            .commit(
                epoch,
                || true,
                || async {
                    called.fetch_add(1, Ordering::SeqCst);
                },
            )
            .await;
        assert_eq!(called.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn action_admitted_before_click_returns_aborts_old_restore() {
        let fence = Fence::default();
        let click_epoch = fence.admit().await;
        let _other_epoch = fence.admit().await;
        let called = AtomicUsize::new(0);
        fence
            .commit(
                click_epoch,
                || true,
                || async {
                    called.fetch_add(1, Ordering::SeqCst);
                },
            )
            .await;
        assert_eq!(called.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn uncontested_restore_runs_once() {
        let fence = Fence::default();
        let click_epoch = fence.admit().await;
        let called = AtomicUsize::new(0);
        fence
            .commit(
                click_epoch,
                || true,
                || async {
                    called.fetch_add(1, Ordering::SeqCst);
                },
            )
            .await;
        assert_eq!(called.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn changed_frontmost_drops_restore() {
        let fence = Fence::default();
        let epoch = fence.admit().await;
        let called = AtomicUsize::new(0);
        fence
            .commit(
                epoch,
                || false,
                || async {
                    called.fetch_add(1, Ordering::SeqCst);
                },
            )
            .await;
        assert_eq!(called.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn admission_while_restore_waits_aborts_before_lock() {
        let fence = Arc::new(Fence::default());
        let epoch = fence.admit().await;
        let (tick_tx, tick_rx) = oneshot::channel();
        let called = Arc::new(AtomicUsize::new(0));
        let f = fence.clone();
        let output = called.clone();
        let task = tokio::spawn(async move {
            tick_rx.await.unwrap();
            f.commit(
                epoch,
                || true,
                || async {
                    output.fetch_add(1, Ordering::SeqCst);
                },
            )
            .await;
        });
        fence.admit().await;
        tick_tx.send(()).unwrap();
        task.await.unwrap();
        assert_eq!(called.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn admission_after_restore_check_waits_for_restore_commit() {
        let fence = Arc::new(Fence::default());
        let click_epoch = fence.admit().await;
        let _click_pending = fence.register(42, click_epoch);
        assert!(!fence.is_pending(99), "the next action targets another pid");
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let worker = fence.clone();
        let restore = tokio::spawn(async move {
            worker
                .commit(
                    click_epoch,
                    || true,
                    || async move {
                        started_tx.send(()).unwrap();
                        release_rx.await.unwrap();
                    },
                )
                .await;
        });
        started_rx.await.unwrap();
        let (done_tx, mut done_rx) = oneshot::channel();
        let worker = fence.clone();
        let admit = tokio::spawn(async move {
            worker.admit().await;
            assert!(worker.await_pending(99).await);
            done_tx.send(()).unwrap();
        });
        while fence.epoch() == click_epoch {
            tokio::task::yield_now().await;
        }
        assert!(done_rx.try_recv().is_err());
        release_tx.send(()).unwrap();
        restore.await.unwrap();
        admit.await.unwrap();
        assert!(done_rx.try_recv().is_ok());
    }
    #[test]
    fn pending_registration_is_cleared_on_drop() {
        let fence = Fence::default();
        let guard = fence.register(42, 7);
        assert!(fence.is_pending(42));
        drop(guard);
        assert!(!fence.is_pending(42));
    }

    #[tokio::test]
    async fn same_pid_admission_waits_until_pending_guard_drops() {
        use std::task::Poll;
        let fence = Fence::default();
        let guard = fence.register(42, 7);
        let mut waiter = Box::pin(fence.await_pending(42));
        std::future::poll_fn(|cx| {
            assert!(waiter.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(guard);
        assert!(waiter.await);
    }

    #[tokio::test]
    async fn cancellation_clears_pending_registration() {
        let fence = Arc::new(Fence::default());
        let worker = fence.clone();
        let (ready_tx, ready_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _pending = worker.register(42, 7);
            ready_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        ready_rx.await.unwrap();
        assert!(fence.is_pending(42));
        task.abort();
        let _ = task.await;
        assert!(!fence.is_pending(42));
    }
}
