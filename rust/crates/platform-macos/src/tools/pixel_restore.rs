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
}

impl Default for Fence {
    fn default() -> Self {
        Self {
            epoch: AtomicU64::new(0),
            commit: AsyncMutex::new(()),
            pending: Mutex::new(HashMap::new()),
            changes: watch::channel(0).0,
        }
    }
}

impl Fence {
    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }
    pub(super) fn bump(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::SeqCst).wrapping_add(1)
    }
    pub(super) async fn admit(&self) -> u64 {
        let epoch = self.bump();
        drop(self.commit.lock().await);
        epoch
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
    if matches!(
        tool.def().name.as_str(),
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
    ) {
        Box::new(AdmittedTool(tool))
    } else {
        tool
    }
}

pub(super) async fn invoke(
    tool: &dyn cua_driver_core::tool::Tool,
    args: serde_json::Value,
) -> cua_driver_core::protocol::ToolResult {
    let epoch = global().admit().await;
    let pid = args
        .get("pid")
        .and_then(serde_json::Value::as_i64)
        .and_then(|p| i32::try_from(p).ok());
    let ready = match pid {
        Some(pid) => global().await_pending(pid).await,
        None => global().await_matching_pending(None).await,
    };
    if !ready {
        return cua_driver_core::protocol::ToolResult::error(
            "A previous pixel focus restoration is still pending; no input was sent.",
        );
    }
    let name = tool.def().name.clone();
    ACTION_EPOCH.scope((name, epoch), tool.invoke(args)).await
}

pub(super) fn spawn_restore<F, Fut>(pid: i32, epoch: u64, expected_front: Option<i32>, work: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let fence = global();
    let pending = fence.register(pid, epoch);
    let idle_before = super::physical_input_idle_ms();
    let started = std::time::Instant::now();
    tokio::spawn(async move {
        let _pending = pending;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        fence
            .commit(
                epoch,
                || {
                    if let (Some(before), Some(after)) =
                        (idle_before, super::physical_input_idle_ms())
                    {
                        let elapsed =
                            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                        if after.saturating_add(2) < before.saturating_add(elapsed) {
                            fence.bump();
                            return false;
                        }
                    }
                    crate::apps::frontmost_pid() == expected_front
                },
                work,
            )
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
