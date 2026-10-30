use super::tests::{fake_identity, safe_tool, test_fence, FakeTool};
use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::oneshot;

#[tokio::test(start_paused = true)]
async fn debounce_reports_user_input_and_third_app_without_restoring() {
    for reason in ["user_input", "third_app"] {
        let fence = Box::leak(Box::new(test_fence()));
        let (epoch, admission) = fence.admit_action(true).await;
        drop(admission);
        let interrupted = Arc::new(AtomicUsize::new(0));
        let sample = interrupted.clone();
        let baseline = RestoreBaseline {
            idle: Some(1000),
            started: std::time::Instant::now(),
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
async fn chained_click_after_unconfirmed_restore_reports_outcome() {
    let fence: &'static Fence = Box::leak(Box::new(test_fence()));
    let (a, action) = fence.admit_action(true).await;
    spawn_restore_with(
        fence,
        2,
        a,
        Some(1),
        || RestoreCheck::Valid,
        || async { false },
    );
    drop(action);
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(50)).await;
    tokio::task::yield_now().await;
    let (b, action_b) = fence.admit_action(true).await;
    spawn_restore_with(
        fence,
        3,
        b,
        Some(2),
        || RestoreCheck::Valid,
        || async { panic!("unconfirmed chain ran") },
    );
    drop(action_b);
    assert_eq!(
        fence.take_outcome(3).unwrap(),
        serde_json::json!({"status":"abandoned", "reason":"unconfirmed", "prior_pid":1})
    );
}

#[tokio::test(start_paused = true)]
async fn unconfirmed_target_receives_only_one_note_after_timeout() {
    let fence: &'static Fence = Box::leak(Box::new(test_fence()));
    let (a, first) = fence.admit_action(true).await;
    spawn_restore_with(
        fence,
        2,
        a,
        Some(1),
        || RestoreCheck::Valid,
        || async { false },
    );
    drop(first);
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(50)).await;
    tokio::task::yield_now().await;
    let (b, second) = fence.admit_action(true).await;
    fence.update_existing_target(2, Some(20), fake_identity(2), b);
    drop(second);
    let first = fence.take_outcome(2).unwrap();
    assert_eq!(first["reason"], "unconfirmed");
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(1950)).await;
    tokio::task::yield_now().await;
    assert!(
        fence.take_outcome(2).is_none(),
        "target received a second note"
    );
}

#[tokio::test(start_paused = true)]
async fn same_pid_click_after_unconfirmed_restore_reports_outcome() {
    let fence: &'static Fence = Box::leak(Box::new(test_fence()));
    let (a, action) = fence.admit_action(true).await;
    spawn_restore_with(
        fence,
        2,
        a,
        Some(1),
        || RestoreCheck::Valid,
        || async { false },
    );
    drop(action);
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(50)).await;
    tokio::task::yield_now().await;
    let (b, action_b) = fence.admit_action(true).await;
    fence.update_existing_target(2, Some(20), fake_identity(2), b);
    drop(action_b);
    assert_eq!(
        fence.take_outcome(2).unwrap(),
        serde_json::json!({"status":"abandoned", "reason":"unconfirmed", "prior_pid":1})
    );
}

#[tokio::test(start_paused = true)]
async fn unconfirmed_restore_keeps_obligation_until_timeout() {
    let fence = Box::leak(Box::new(test_fence()));
    let (epoch, admission) = fence.admit_action(true).await;
    drop(admission);
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
    let fence = Box::leak(Box::new(test_fence()));
    let (epoch, action) = fence.admit_action(true).await;
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
    let fence = Box::leak(Box::new(test_fence()));
    let (epoch, admission) = fence.admit_action(true).await;
    drop(admission);
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

#[tokio::test]
async fn page_connection_actions_are_tracked_without_tracking_plain_reads() {
    struct Probe {
        def: cua_driver_core::tool::ToolDef,
        fence: &'static Fence,
        seen: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl cua_driver_core::tool::Tool for Probe {
        fn def(&self) -> &cua_driver_core::tool::ToolDef {
            &self.def
        }
        async fn invoke(&self, _: serde_json::Value) -> cua_driver_core::protocol::ToolResult {
            self.seen.store(
                self.fence.in_flight.load(Ordering::SeqCst),
                Ordering::SeqCst,
            );
            cua_driver_core::protocol::ToolResult::text("ok")
        }
    }
    for (action, extra, expected) in [
        ("insert_text", serde_json::json!({}), 1),
        ("type_keystrokes", serde_json::json!({}), 1),
        (
            "execute_javascript",
            serde_json::json!({"cdp_port":9222}),
            1,
        ),
        (
            "execute_javascript",
            serde_json::json!({"target_url_contains":"x"}),
            1,
        ),
        ("execute_javascript", serde_json::json!({}), 0),
        ("click_element", serde_json::json!({}), 0),
        ("get_text", serde_json::json!({}), 0),
    ] {
        let fence: &'static Fence = Box::leak(Box::new(test_fence()));
        let seen = Arc::new(AtomicUsize::new(99));
        let mut def = FakeTool::new().0;
        def.name = "page".into();
        let probe = Probe {
            def,
            fence,
            seen: seen.clone(),
        };
        let mut args = extra;
        args["action"] = action.into();
        invoke_with_outcome(fence, &probe, args).await;
        assert_eq!(seen.load(Ordering::SeqCst), expected, "{action} tracking");
    }
}

#[tokio::test]
async fn focus_capable_browser_and_page_actions_are_tracked() {
    struct Probe {
        def: cua_driver_core::tool::ToolDef,
        fence: &'static Fence,
        seen: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl cua_driver_core::tool::Tool for Probe {
        fn def(&self) -> &cua_driver_core::tool::ToolDef {
            &self.def
        }
        async fn invoke(&self, _: serde_json::Value) -> cua_driver_core::protocol::ToolResult {
            self.seen.store(
                self.fence.in_flight.load(Ordering::SeqCst),
                Ordering::SeqCst,
            );
            cua_driver_core::protocol::ToolResult::text("ok")
        }
    }
    for (name, action) in [
        ("page", "enable_javascript_apple_events"),
        ("browser_navigate", ""),
        ("browser_click", ""),
        ("browser_download", ""),
        ("browser_prepare", ""),
        ("get_browser_state", ""),
    ] {
        let fence: &'static Fence = Box::leak(Box::new(test_fence()));
        let seen = Arc::new(AtomicUsize::new(0));
        let mut def = FakeTool::new().0;
        def.name = name.into();
        let probe = Probe {
            def,
            fence,
            seen: seen.clone(),
        };
        invoke_with_fence(fence, &probe, serde_json::json!({"action":action})).await;
        assert_eq!(
            seen.load(Ordering::SeqCst),
            1,
            "{name} {action} must be tracked"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn ordinary_page_action_does_not_starve_focus_restoration() {
    struct BlockingTool {
        def: cua_driver_core::tool::ToolDef,
        ready: Mutex<Option<oneshot::Sender<()>>>,
        release: tokio::sync::Mutex<Option<oneshot::Receiver<()>>>,
    }
    #[async_trait::async_trait]
    impl cua_driver_core::tool::Tool for BlockingTool {
        fn def(&self) -> &cua_driver_core::tool::ToolDef {
            &self.def
        }
        async fn invoke(&self, _: serde_json::Value) -> cua_driver_core::protocol::ToolResult {
            self.ready.lock().unwrap().take().unwrap().send(()).unwrap();
            self.release.lock().await.take().unwrap().await.unwrap();
            cua_driver_core::protocol::ToolResult::text("ok")
        }
    }
    let fence: &'static Fence = Box::leak(Box::new(test_fence()));
    let (epoch, admission) = fence.admit_action(true).await;
    drop(admission);
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
    let (ready, receiving) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let mut def = FakeTool::new().0;
    def.name = "page".into();
    let tool = BlockingTool {
        def,
        ready: Mutex::new(Some(ready)),
        release: tokio::sync::Mutex::new(Some(released)),
    };
    let action = tokio::spawn(async move {
        invoke_with_fence(
            fence,
            &tool,
            serde_json::json!({"pid": 99, "action": "click_element"}),
        )
        .await
    });
    receiving.await.unwrap();
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(50)).await;
    tokio::task::yield_now().await;
    let during_action = restored.load(Ordering::SeqCst);
    release.send(()).unwrap();
    action.await.unwrap();
    assert_eq!(
        during_action, 1,
        "ordinary page action delayed focus restoration"
    );
}

#[tokio::test(start_paused = true)]
async fn debounce_does_not_commit_until_inflight_action_finishes() {
    let fence = Box::leak(Box::new(test_fence()));
    let (epoch, action) = fence.admit_action(true).await;
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
    let fence = Arc::new(test_fence());
    let worker = fence.clone();
    let (ready, received) = oneshot::channel();
    let task = tokio::spawn(async move {
        let (_, _guard) = worker.admit_action(true).await;
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
    let fence = Box::leak(Box::new(test_fence()));
    let (epoch, admission) = fence.admit_action(true).await;
    drop(admission);
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

#[tokio::test(start_paused = true)]
async fn admission_waits_for_spawned_restoration_to_finish_focus() {
    let fence: &'static Fence = Box::leak(Box::new(test_fence()));
    let (epoch, admission) = fence.admit_action(true).await;
    drop(admission);
    let baseline = RestoreBaseline {
        idle: Some(1000),
        started: std::time::Instant::now(),
    };
    let (activated, activated_rx) = oneshot::channel();
    let (resume, resume_rx) = oneshot::channel();
    let focused = Arc::new(AtomicUsize::new(0));
    let focused_worker = focused.clone();
    spawn_restore_with(
        fence,
        2,
        epoch,
        Some(1),
        || RestoreCheck::Valid,
        move || async move {
            activated.send(()).unwrap();
            resume_rx.await.unwrap();
            if baseline.valid_sample(Some(1050), 50, Some(1), Some(1), 2) {
                focused_worker.store(1, Ordering::SeqCst);
            }
            true
        },
    );
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(50)).await;
    activated_rx.await.unwrap();
    let action = tokio::spawn(async move { fence.admit_action(true).await });
    while fence.epoch() == epoch {
        tokio::task::yield_now().await;
    }
    assert!(!action.is_finished());
    resume.send(()).unwrap();
    let (_, guard) = action.await.unwrap();
    drop(guard);
    assert_eq!(focused.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn double_click_nested_click_counts_and_releases_both_actions() {
    struct Inner {
        def: cua_driver_core::tool::ToolDef,
        fence: &'static Fence,
        observed: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl cua_driver_core::tool::Tool for Inner {
        fn def(&self) -> &cua_driver_core::tool::ToolDef {
            &self.def
        }
        async fn invoke(&self, _: serde_json::Value) -> cua_driver_core::protocol::ToolResult {
            assert!(current_is("click"));
            self.observed.store(
                self.fence.in_flight.load(Ordering::SeqCst),
                Ordering::SeqCst,
            );
            cua_driver_core::protocol::ToolResult::text("ok")
        }
    }
    struct Outer {
        def: cua_driver_core::tool::ToolDef,
        fence: &'static Fence,
        inner: Inner,
    }
    #[async_trait::async_trait]
    impl cua_driver_core::tool::Tool for Outer {
        fn def(&self) -> &cua_driver_core::tool::ToolDef {
            &self.def
        }
        async fn invoke(&self, _: serde_json::Value) -> cua_driver_core::protocol::ToolResult {
            assert!(current_is("double_click"));
            invoke_with_fence(self.fence, &self.inner, serde_json::json!({})).await
        }
    }
    let fence: &'static Fence = Box::leak(Box::new(test_fence()));
    let observed = Arc::new(AtomicUsize::new(0));
    let mut inner_def = FakeTool::new().0;
    inner_def.name = "click".into();
    let mut outer_def = FakeTool::new().0;
    outer_def.name = "double_click".into();
    let tool = Outer {
        def: outer_def,
        fence,
        inner: Inner {
            def: inner_def,
            fence,
            observed: observed.clone(),
        },
    };
    invoke_with_fence(fence, &tool, serde_json::json!({})).await;
    assert_eq!(observed.load(Ordering::SeqCst), 2);
    assert_eq!(fence.in_flight.load(Ordering::SeqCst), 0);
}
