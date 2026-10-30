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

struct SafeTool(cua_driver_core::tool::ToolDef);
#[async_trait::async_trait]
impl cua_driver_core::tool::Tool for SafeTool {
    fn def(&self) -> &cua_driver_core::tool::ToolDef {
        &self.0
    }
    async fn invoke(&self, _: serde_json::Value) -> cua_driver_core::protocol::ToolResult {
        cua_driver_core::protocol::ToolResult::text("ok")
    }
}
fn safe_tool(name: &str) -> SafeTool {
    let mut def = FakeTool::new().0;
    def.name = name.into();
    def.read_only = name == "get_window_state";
    SafeTool(def)
}

#[tokio::test(start_paused = true)]
async fn restoration_outcomes_are_delivered_once_to_next_input_or_observation() {
    let fence = Box::leak(Box::new(Fence::default()));
    let input = safe_tool("click");
    let observe = safe_tool("get_window_state");
    let args = serde_json::json!({"pid": 2});
    let epoch = fence.admit().await;
    spawn_restore_with(
        fence,
        2,
        epoch,
        Some(1),
        || RestoreCheck::Abandon("user_input"),
        || async { panic!("restored") },
    );
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(50)).await;
    tokio::task::yield_now().await;
    let unrelated = invoke_with_outcome(fence, &safe_tool("unrelated_write"), args.clone()).await;
    assert!(unrelated.structured_content.is_none());
    let first = invoke_with_outcome(fence, &input, args.clone()).await;
    assert_eq!(
        first.structured_content.unwrap()["focus_restoration"],
        serde_json::json!({"status":"abandoned", "reason":"user_input", "prior_pid":1})
    );
    let epoch = fence.admit().await;
    spawn_restore_with(
        fence,
        2,
        epoch,
        Some(1),
        || RestoreCheck::Valid,
        || async { true },
    );
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(50)).await;
    tokio::task::yield_now().await;
    let second = invoke_with_outcome(fence, &observe, args.clone()).await;
    assert_eq!(
        second.structured_content.unwrap()["focus_restoration"],
        serde_json::json!({"status":"restored", "prior_pid":1})
    );
    let third = invoke_with_outcome(fence, &observe, args).await;
    assert!(third.structured_content.is_none());
}

#[tokio::test(start_paused = true)]
async fn refused_admission_does_not_consume_outcome() {
    let fence = Fence::default();
    fence.record_outcome(2, Some(1), "abandoned", Some("user_input"));
    let lock = fence.commit.lock().await;
    let refused =
        invoke_with_outcome(&fence, &safe_tool("click"), serde_json::json!({"pid":2})).await;
    assert_eq!(
        refused.structured_content.unwrap()["code"],
        "pixel_restore_pending"
    );
    drop(lock);
    let next = invoke_with_outcome(
        &fence,
        &safe_tool("get_window_state"),
        serde_json::json!({"pid":2}),
    )
    .await;
    assert_eq!(
        next.structured_content.unwrap()["focus_restoration"]["reason"],
        "user_input"
    );
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
        tokio::time::timeout(std::time::Duration::from_millis(1), fence.await_pending(42)).await;
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

#[test]
fn reactivation_does_not_override_physical_input_or_third_app() {
    let baseline = RestoreBaseline {
        idle: Some(1000),
        started: std::time::Instant::now(),
        epoch: 7,
    };
    let epoch = baseline.restore_epoch(8, true);
    assert!(!baseline.valid_sample(Some(10), 50, Some(1), Some(1), 2, epoch));
    assert!(!baseline.valid_sample(Some(1050), 50, Some(3), Some(1), 2, epoch));
}

#[tokio::test(start_paused = true)]
async fn admission_during_reactivation_does_not_skip_window_focus() {
    let fence = Arc::new(Fence::default());
    let epoch = fence.admit().await;
    let baseline = RestoreBaseline {
        idle: Some(1000),
        started: std::time::Instant::now(),
        epoch,
    };
    let focused = Arc::new(AtomicUsize::new(0));
    let (activated_tx, activated_rx) = oneshot::channel();
    let (resume_tx, resume_rx) = oneshot::channel();
    let worker = fence.clone();
    let focused_worker = focused.clone();
    let restore = tokio::spawn(async move {
        worker
            .commit(
                epoch,
                || true,
                || async {
                    activated_tx.send(()).unwrap();
                    resume_rx.await.unwrap();
                    if baseline.valid_sample(
                        Some(1050),
                        50,
                        Some(1),
                        Some(1),
                        2,
                        baseline.restore_epoch(worker.epoch(), true),
                    ) {
                        focused_worker.store(1, Ordering::SeqCst);
                    }
                },
            )
            .await;
    });
    activated_rx.await.unwrap();
    let worker = fence.clone();
    let admission = tokio::spawn(async move { worker.admit().await });
    while fence.epoch() == epoch {
        tokio::task::yield_now().await;
    }
    resume_tx.send(()).unwrap();
    restore.await.unwrap();
    admission.await.unwrap();
    assert_eq!(
        focused.load(Ordering::SeqCst),
        1,
        "window focus must complete"
    );
}

#[tokio::test(start_paused = true)]
async fn original_front_is_restored_after_intervening_action_and_second_click() {
    let fence = Box::leak(Box::new(Fence::default()));
    let front = Arc::new(AtomicUsize::new(1));
    let restores = Arc::new(AtomicUsize::new(0));
    let first_click = fence.admit().await;
    front.store(2, Ordering::SeqCst);
    let fake_front = front.clone();
    let fake_restores = restores.clone();
    spawn_restore_with(
        fence,
        2,
        first_click,
        Some(1),
        || RestoreCheck::Valid,
        move || async move {
            fake_front.store(1, Ordering::SeqCst);
            fake_restores.fetch_add(1, Ordering::SeqCst);
            true
        },
    );
    tokio::task::yield_now().await;
    let (_, action) = fence.admit_action().await;
    drop(action);
    let (_, click) = fence.admit_action().await;
    drop(click);
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(50)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        front.load(Ordering::SeqCst),
        1,
        "original app must be restored"
    );
    assert_eq!(restores.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn older_click_obligation_wins_even_if_registered_later() {
    let fence = Box::leak(Box::new(Fence::default()));
    let (old, old_action) = fence.admit_action().await;
    let (new, new_action) = fence.admit_action().await;
    let restored = Arc::new(AtomicUsize::new(0));
    let newer = restored.clone();
    spawn_restore_with(
        fence,
        2,
        new,
        Some(3),
        || RestoreCheck::Valid,
        move || async move {
            newer.store(3, Ordering::SeqCst);
            true
        },
    );
    let older = restored.clone();
    spawn_restore_with(
        fence,
        2,
        old,
        Some(1),
        || RestoreCheck::Valid,
        move || async move {
            older.store(1, Ordering::SeqCst);
            true
        },
    );
    assert_eq!(
        fence.obligations.lock().unwrap().get(&2),
        Some(&(old, Some(1)))
    );
    drop(new_action);
    drop(old_action);
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(50)).await;
    tokio::task::yield_now().await;
    assert_eq!(restored.load(Ordering::SeqCst), 1);
    assert_eq!(fence.take_outcome(2).unwrap()["prior_pid"], 1);
}

#[tokio::test(start_paused = true)]
async fn debounce_reports_user_input_and_third_app_without_restoring() {
    for reason in ["user_input", "third_app"] {
        let fence = Box::leak(Box::new(Fence::default()));
        let epoch = fence.admit().await;
        let interrupted = Arc::new(AtomicUsize::new(0));
        let sample = interrupted.clone();
        let baseline = RestoreBaseline {
            idle: Some(1000),
            started: std::time::Instant::now(),
            epoch,
        };
        spawn_restore_with(
            fence,
            2,
            epoch,
            Some(1),
            move || {
                let changed = sample.load(Ordering::SeqCst) != 0;
                let front = if changed && reason == "third_app" {
                    Some(3)
                } else {
                    Some(2)
                };
                let idle = if changed && reason == "user_input" {
                    Some(5)
                } else {
                    Some(1050)
                };
                baseline.abandon_sample(Some(1), 2, front, idle, 50)
            },
            || async { panic!("restored") },
        );
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_millis(20)).await;
        interrupted.store(1, Ordering::SeqCst);
        tokio::time::advance(std::time::Duration::from_millis(30)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            fence.take_outcome(2).unwrap(),
            serde_json::json!({"status":"abandoned", "reason":reason, "prior_pid":1})
        );
    }
}

#[tokio::test(start_paused = true)]
async fn unconfirmed_restore_keeps_obligation_until_timeout() {
    let fence = Box::leak(Box::new(Fence::default()));
    let epoch = fence.admit().await;
    let posts = Arc::new(AtomicUsize::new(0));
    let sent = posts.clone();
    spawn_restore_with(
        fence,
        2,
        epoch,
        Some(1),
        || RestoreCheck::Valid,
        move || async move {
            sent.fetch_add(1, Ordering::SeqCst);
            false
        },
    );
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(50)).await;
    tokio::task::yield_now().await;
    assert_eq!(posts.load(Ordering::SeqCst), 1);
    assert!(fence.take_outcome(2).is_none());
    tokio::time::advance(std::time::Duration::from_millis(1950)).await;
    tokio::task::yield_now().await;
    assert_eq!(posts.load(Ordering::SeqCst), 1);
    assert_eq!(fence.take_outcome(2).unwrap()["reason"], "timeout");
}

#[tokio::test(start_paused = true)]
async fn debounce_waits_for_inflight_and_times_out_at_two_seconds() {
    let fence = Box::leak(Box::new(Fence::default()));
    let (epoch, action) = fence.admit_action().await;
    spawn_restore_with(
        fence,
        2,
        epoch,
        Some(1),
        || RestoreCheck::Valid,
        || async { panic!("restored while action active") },
    );
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        fence.take_outcome(2).unwrap(),
        serde_json::json!({"status":"abandoned", "reason":"timeout", "prior_pid":1})
    );
    drop(action);
}

#[tokio::test(start_paused = true)]
async fn unreadable_front_or_idle_defers_until_timeout() {
    let baseline = RestoreBaseline {
        idle: Some(1000),
        started: std::time::Instant::now(),
        epoch: 1,
    };
    assert_eq!(
        baseline.abandon_sample(Some(1), 2, None, Some(1050), 50),
        RestoreCheck::Retry
    );
    assert_eq!(
        baseline.abandon_sample(Some(1), 2, Some(2), None, 50),
        RestoreCheck::Retry
    );
    assert_eq!(
        baseline.abandon_sample(Some(1), 2, Some(3), None, 50),
        RestoreCheck::Abandon("third_app")
    );
    let fence = Box::leak(Box::new(Fence::default()));
    let epoch = fence.admit().await;
    spawn_restore_with(
        fence,
        2,
        epoch,
        Some(1),
        || RestoreCheck::Retry,
        || async { panic!("unknown sample restored") },
    );
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    assert_eq!(fence.take_outcome(2).unwrap()["reason"], "timeout");
}

#[tokio::test(start_paused = true)]
async fn debounce_does_not_commit_until_inflight_action_finishes() {
    let fence = Box::leak(Box::new(Fence::default()));
    let (epoch, action) = fence.admit_action().await;
    let restored = Arc::new(AtomicUsize::new(0));
    let result = restored.clone();
    spawn_restore_with(
        fence,
        2,
        epoch,
        Some(1),
        || RestoreCheck::Valid,
        move || async move {
            result.fetch_add(1, Ordering::SeqCst);
            true
        },
    );
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(100)).await;
    tokio::task::yield_now().await;
    assert_eq!(restored.load(Ordering::SeqCst), 0);
    drop(action);
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(50)).await;
    tokio::task::yield_now().await;
    assert_eq!(restored.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelled_admission_releases_inflight_guard() {
    let fence = Arc::new(Fence::default());
    let worker = fence.clone();
    let (ready, received) = oneshot::channel();
    let task = tokio::spawn(async move {
        let (_, _guard) = worker.admit_action().await;
        ready.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    received.await.unwrap();
    assert_eq!(fence.in_flight.load(Ordering::SeqCst), 1);
    task.abort();
    let _ = task.await;
    assert_eq!(fence.in_flight.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn second_click_is_not_blocked_by_debounced_restore() {
    let fence = Box::leak(Box::new(Fence::default()));
    let epoch = fence.admit().await;
    spawn_restore_with(
        fence,
        2,
        epoch,
        Some(1),
        || RestoreCheck::Valid,
        || async { true },
    );
    let outcome = tokio::time::timeout(
        std::time::Duration::from_millis(1),
        invoke_with_outcome(fence, &safe_tool("click"), serde_json::json!({"pid":2})),
    )
    .await;
    assert!(outcome.is_ok());
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
