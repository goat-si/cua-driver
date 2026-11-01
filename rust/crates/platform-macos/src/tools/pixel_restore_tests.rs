use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
pub(super) fn fake_identity(pid: i32) -> Option<ProcessStamp> {
    (pid != 0).then_some((pid as u64, 1))
}
pub(super) fn test_fence() -> Fence {
    Fence {
        identity: fake_identity,
        ..Fence::default()
    }
}

pub(super) struct FakeTool(pub(super) cua_driver_core::tool::ToolDef);
impl FakeTool {
    pub(super) fn new() -> Self {
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

pub(super) struct SafeTool(cua_driver_core::tool::ToolDef);
#[async_trait::async_trait]
impl cua_driver_core::tool::Tool for SafeTool {
    fn def(&self) -> &cua_driver_core::tool::ToolDef {
        &self.0
    }
    async fn invoke(&self, _: serde_json::Value) -> cua_driver_core::protocol::ToolResult {
        cua_driver_core::protocol::ToolResult::text("ok")
    }
}
pub(super) fn safe_tool(name: &str) -> SafeTool {
    let mut def = FakeTool::new().0;
    def.name = name.into();
    def.read_only = name == "get_window_state";
    SafeTool(def)
}

struct RecordedClick {
    def: cua_driver_core::tool::ToolDef,
}

#[async_trait::async_trait]
impl cua_driver_core::tool::Tool for RecordedClick {
    fn def(&self) -> &cua_driver_core::tool::ToolDef {
        &self.def
    }
    async fn invoke(&self, _: serde_json::Value) -> cua_driver_core::protocol::ToolResult {
        use cua_driver_core::action_record::{
            ActionEffect, ActionExecutionRecord, ActionTransport, ActualDelivery, RequestedDelivery,
        };
        cua_driver_core::protocol::ToolResult::text("Clicked.")
            .with_structured(serde_json::json!({"effect":"unverifiable"}))
            .with_action_record(
                ActionExecutionRecord::builder(
                    ActionEffect::Unverifiable,
                    ActionTransport::MacosCgEventPid,
                    RequestedDelivery::Background,
                )
                .actual_delivery(ActualDelivery::Background)
                .build()
                .unwrap(),
            )
    }
}

struct ClickWithRestoration {
    fence: Fence,
    inner: RecordedClick,
}

#[async_trait::async_trait]
impl cua_driver_core::tool::Tool for ClickWithRestoration {
    fn def(&self) -> &cua_driver_core::tool::ToolDef {
        &self.inner.def
    }
    async fn invoke(&self, args: serde_json::Value) -> cua_driver_core::protocol::ToolResult {
        invoke_with_outcome(&self.fence, &self.inner, args).await
    }
}

#[tokio::test]
async fn successful_click_publishes_attached_restoration_outcome() {
    use cua_driver_core::authorization::PermissionMode;
    use cua_driver_core::session_authorization::{
        SessionAuthorizationRegistry, SessionModeCeiling,
    };
    use std::time::Duration;

    for (status, reason) in [("abandoned", Some("third_app")), ("restored", None)] {
        let fence = test_fence();
        fence.record_outcome(42, Some((42, 1)), Some(1), status, reason);
        let mut def = FakeTool::new().0;
        def.name = "click".into();
        def.input_schema = serde_json::json!({"type":"object"});
        let mut registry =
            cua_driver_core::tool::ToolRegistry::new_with_protected_consent_provider(None);
        registry.register(Box::new(ClickWithRestoration {
            fence,
            inner: RecordedClick { def },
        }));
        let ceiling = SessionModeCeiling::for_trusted_sessions(
            [PermissionMode::Standard],
            false,
            Duration::from_secs(60),
            Duration::from_secs(30),
        )
        .unwrap();
        let context = SessionAuthorizationRegistry::with_ceiling(ceiling)
            .compatibility_context(PermissionMode::Standard, None)
            .unwrap();
        registry
            .protected_resource_ownership()
            .mark_driver_owned_pid(&context.runtime_session_key("restoration-test"), 42);
        let result = registry.invoke_with_context("click", serde_json::json!({"pid":42,"window_id":7,"session":"restoration-test","x":10,"y":20,"post_action_wait":"none"}), context).await;
        assert_ne!(result.is_error, Some(true), "{result:?}");
        let data = result.structured_content.unwrap();
        let mut expected = serde_json::json!({"status":status,"prior_pid":1});
        if let Some(reason) = reason {
            expected["reason"] = reason.into();
        }
        assert_eq!(data["focus_restoration"], expected);
        assert_eq!(data["effect"], "unverifiable");
        assert_eq!(data["summary"], "Clicked.");
        assert!(result.content.iter().any(|block| matches!(block, cua_driver_core::protocol::Content::Text { text, .. } if text.starts_with("focus_restoration: "))));
    }
}

#[tokio::test(start_paused = true)]
async fn restoration_outcomes_are_delivered_once_to_next_input_or_observation() {
    let fence = Box::leak(Box::new(test_fence()));
    let input = safe_tool("click");
    let observe = safe_tool("get_window_state");
    let args = serde_json::json!({"pid": 2});
    let (epoch, admission) = fence.admit_action(true).await;
    drop(admission);
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
    let fence = test_fence();
    fence.record_outcome(2, Some((2, 1)), Some(1), "abandoned", Some("user_input"));
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

#[test]
fn reused_pid_never_receives_an_old_restoration_note() {
    let fence = test_fence();
    fence.record_outcome(2, Some((10, 1)), Some(1), "abandoned", Some("user_input"));
    assert!(fence.take_outcome_for(2, Some((11, 1))).is_none());
    assert!(fence.take_outcome_for(2, Some((10, 1))).is_none());
}

#[tokio::test(start_paused = true)]
async fn restoration_panic_is_reported_to_the_next_result() {
    let fence: &'static Fence = Box::leak(Box::new(test_fence()));
    let (epoch, admission) = fence.admit_action(true).await;
    drop(admission);
    spawn_restore_with(
        fence,
        2,
        epoch,
        Some(1),
        || RestoreCheck::Valid,
        || async { panic!("fake restoration panic") },
    );
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(50)).await;
    tokio::task::yield_now().await;
    let result = invoke_with_outcome(
        fence,
        &safe_tool("get_window_state"),
        serde_json::json!({"pid":2}),
    )
    .await;
    assert_eq!(
        result.structured_content.unwrap()["focus_restoration"]["reason"],
        "panic"
    );
}

#[tokio::test(start_paused = true)]
async fn browser_state_refusal_reports_operation_not_dispatched() {
    let fence = test_fence();
    let guard = fence.commit.lock().await;
    let result = invoke_with_outcome(
        &fence,
        &safe_tool("get_browser_state"),
        serde_json::json!({"pid": 2}),
    )
    .await;
    assert_eq!(
        result.structured_content.as_ref().unwrap()["code"],
        "pixel_restore_pending"
    );
    let text = serde_json::to_string(&result.content).unwrap();
    assert!(text.contains("operation was not dispatched"));
    assert!(!text.contains("no input was sent"));
    drop(guard);
}

#[tokio::test(start_paused = true)]
async fn pixel_admission_lock_has_deadline() {
    let fence = test_fence();
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
fn driver_foreground_hid_does_not_become_user_takeover() {
    let baseline = RestoreBaseline {
        idle: Some(1000),
        started: std::time::Instant::now(),
    };
    assert!(baseline.valid_sample_with_driver_hid(Some(10), 50, Some(2), Some(1), 2, true));
    assert!(!baseline.valid_sample_with_driver_hid(Some(10), 50, Some(2), Some(1), 2, false));
}

#[test]
fn pixel_target_front_without_physical_input_allows_restore() {
    let baseline = RestoreBaseline {
        idle: Some(1000),
        started: std::time::Instant::now(),
    };
    assert!(
        baseline.valid_sample(Some(1050), 50, Some(2), Some(1), 2),
        "driver-caused target activation must allow restoration"
    );
}

#[test]
fn pixel_pre_dispatch_baseline_detects_takeover_and_front_change() {
    let baseline = RestoreBaseline {
        idle: Some(1000),
        started: std::time::Instant::now(),
    };
    assert!(baseline.valid_sample(Some(1050), 50, Some(1), Some(1), 2));
    // Input during dispatch reset the idle clock even if the target is now frontmost.
    assert!(!baseline.valid_sample(Some(20), 50, Some(2), Some(1), 2));
    assert!(!baseline.valid_sample(Some(1050), 50, Some(3), Some(1), 2));
    // A second focus post must sample again after the first post or an await.
    assert!(!baseline.valid_sample(Some(1), 70, Some(1), Some(1), 2));
}

#[test]
fn reactivation_does_not_override_physical_input_or_third_app() {
    let baseline = RestoreBaseline {
        idle: Some(1000),
        started: std::time::Instant::now(),
    };
    assert!(!baseline.valid_sample(Some(10), 50, Some(1), Some(1), 2));
    assert!(!baseline.valid_sample(Some(1050), 50, Some(3), Some(1), 2));
}

#[tokio::test(start_paused = true)]
async fn original_front_is_restored_after_intervening_action_and_second_click() {
    let fence = Box::leak(Box::new(test_fence()));
    let front = Arc::new(AtomicUsize::new(1));
    let restores = Arc::new(AtomicUsize::new(0));
    let (first_click, admission) = fence.admit_action(true).await;
    drop(admission);
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
    let (_, action) = fence.admit_action(true).await;
    drop(action);
    let (_, click) = fence.admit_action(true).await;
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

struct FakeDesktop {
    front: AtomicUsize,
    key: AtomicUsize,
    window: AtomicUsize,
    posts: AtomicUsize,
}
impl RestoreProbe for FakeDesktop {
    fn front(&self) -> Option<i32> {
        Some(self.front.load(Ordering::SeqCst) as i32)
    }
    fn check(
        &self,
        _: RestoreBaseline,
        expected: Option<i32>,
        target: i32,
        front: Option<i32>,
    ) -> RestoreCheck {
        if expected == Some(4) {
            return RestoreCheck::Retry;
        }
        if front == expected || front == Some(target) {
            RestoreCheck::Valid
        } else {
            RestoreCheck::Abandon("third_app")
        }
    }
}
#[async_trait::async_trait]
impl super::super::pixel_focus::PixelRestoreOps for FakeDesktop {
    fn front(&self) -> Option<i32> {
        RestoreProbe::front(self)
    }
    fn key_focus(&self) -> Option<i32> {
        Some(self.key.load(Ordering::SeqCst) as i32)
    }
    fn key_window(&self, _: i32) -> Option<u32> {
        Some(self.window.load(Ordering::SeqCst) as u32)
    }
    fn allowed(&self, _: Option<i32>) -> bool {
        true
    }
    async fn activate(&self, pid: i32) {
        self.front.store(pid as usize, Ordering::SeqCst);
    }
    async fn focus(&self, prior: i32, wid: Option<u32>, _: i32, _: u32) {
        self.key.store(prior as usize, Ordering::SeqCst);
        if let Some(wid) = wid {
            self.window.store(wid as usize, Ordering::SeqCst);
        }
        self.posts.fetch_add(1, Ordering::SeqCst);
    }
}
fn fake_baseline() -> RestoreBaseline {
    RestoreBaseline {
        idle: Some(1000),
        started: std::time::Instant::now(),
    }
}

#[tokio::test(start_paused = true)]
async fn cross_pid_click_chain_restores_original_app_once() {
    for reverse in [false, true] {
        let fence: &'static Fence = Box::leak(Box::new(test_fence()));
        let (a_epoch, a_guard) = fence.admit_action(true).await;
        let (c_epoch, c_guard) = fence.admit_action(true).await;
        let desktop = Arc::new(FakeDesktop {
            front: 3.into(),
            key: 3.into(),
            window: 7.into(),
            posts: 0.into(),
        });
        let register = |target: i32, epoch: u64, prior: i32| {
            let ops = desktop.clone();
            spawn_restore(
                fence,
                SpawnRestore {
                    pid: target,
                    epoch,
                    expected_front: Some(prior),
                    baseline: fake_baseline(),
                    window_id: Some(9),
                    target_stamp: fake_identity(target),
                },
                desktop.clone(),
                move |pid, wid| async move {
                    super::super::pixel_focus::restore_user_focus_with(
                        ops.as_ref(),
                        pid,
                        Some(prior),
                        Some(7),
                        wid,
                        true,
                        Some(prior),
                    )
                    .await
                },
            );
        };
        if reverse {
            register(3, c_epoch, 2);
            register(2, a_epoch, 1);
        } else {
            register(2, a_epoch, 1);
            register(3, c_epoch, 2);
        }
        drop(a_guard);
        drop(c_guard);
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_millis(50)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            desktop.front.load(Ordering::SeqCst),
            1,
            "original front, reverse={reverse}"
        );
        assert_eq!(desktop.posts.load(Ordering::SeqCst), 1);
        assert_eq!(
            fence.take_outcome(3).unwrap(),
            serde_json::json!({"status":"restored", "prior_pid":1})
        );
        assert!(fence.take_outcome(3).is_none());
        assert!(fence.take_outcome(2).is_none());
    }
}

#[tokio::test(start_paused = true)]
async fn chain_targeting_original_app_reports_restored() {
    let fence: &'static Fence = Box::leak(Box::new(test_fence()));
    let (a, a_guard) = fence.admit_action(true).await;
    let (b, b_guard) = fence.admit_action(true).await;
    let desktop = Arc::new(FakeDesktop {
        front: 1.into(),
        key: 1.into(),
        window: 7.into(),
        posts: 0.into(),
    });
    let ops = desktop.clone();
    spawn_restore(
        fence,
        SpawnRestore {
            pid: 2,
            epoch: a,
            expected_front: Some(1),
            baseline: fake_baseline(),
            window_id: Some(9),
            target_stamp: fake_identity(2),
        },
        desktop.clone(),
        move |pid, wid| async move {
            super::super::pixel_focus::restore_user_focus_with(
                ops.as_ref(),
                pid,
                Some(1),
                Some(7),
                wid,
                true,
                Some(1),
            )
            .await
        },
    );
    spawn_restore(
        fence,
        SpawnRestore {
            pid: 1,
            epoch: b,
            expected_front: Some(2),
            baseline: fake_baseline(),
            window_id: Some(7),
            target_stamp: fake_identity(1),
        },
        desktop.clone(),
        |_, _| async { panic!("chained duplicate") },
    );
    drop(a_guard);
    drop(b_guard);
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(50)).await;
    tokio::task::yield_now().await;
    assert_eq!(desktop.posts.load(Ordering::SeqCst), 0);
    assert_eq!(
        fence.take_outcome(1).unwrap(),
        serde_json::json!({"status":"restored", "prior_pid":1})
    );
}

#[tokio::test]
async fn original_app_target_restores_its_previous_key_window() {
    let ops = FakeDesktop {
        front: 1.into(),
        key: 1.into(),
        window: 20.into(),
        posts: 0.into(),
    };
    let restored = super::super::pixel_focus::restore_user_focus_with(
        &ops,
        1,
        Some(1),
        Some(7),
        Some(20),
        true,
        Some(1),
    )
    .await;
    assert!(restored);
    assert_eq!(ops.window.load(Ordering::SeqCst), 7);
    assert_eq!(ops.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn real_spawn_retries_when_another_pending_target_is_frontmost() {
    let fence: &'static Fence = Box::leak(Box::new(test_fence()));
    let (a, a_guard) = fence.admit_action(true).await;
    let (b, b_guard) = fence.admit_action(true).await;
    let desktop = Arc::new(FakeDesktop {
        front: 3.into(),
        key: 3.into(),
        window: 7.into(),
        posts: 0.into(),
    });
    let ops = desktop.clone();
    spawn_restore(
        fence,
        SpawnRestore {
            pid: 2,
            epoch: a,
            expected_front: Some(1),
            baseline: fake_baseline(),
            window_id: Some(9),
            target_stamp: fake_identity(2),
        },
        desktop.clone(),
        move |pid, wid| async move {
            super::super::pixel_focus::restore_user_focus_with(
                ops.as_ref(),
                pid,
                Some(1),
                Some(7),
                wid,
                true,
                Some(1),
            )
            .await
        },
    );
    spawn_restore(
        fence,
        SpawnRestore {
            pid: 3,
            epoch: b,
            expected_front: Some(4),
            baseline: fake_baseline(),
            window_id: Some(10),
            target_stamp: fake_identity(3),
        },
        desktop.clone(),
        |_, _| async { panic!("other pending restore ran") },
    );
    drop(a_guard);
    drop(b_guard);
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(50)).await;
    tokio::task::yield_now().await;
    assert_eq!(desktop.posts.load(Ordering::SeqCst), 0);
    assert!(
        fence.obligations.lock().unwrap().contains_key(&2),
        "pending target must not abandon original"
    );
    assert!(fence.take_outcome(2).is_none());
    desktop.front.store(2, Ordering::SeqCst);
    tokio::time::advance(std::time::Duration::from_millis(50)).await;
    tokio::task::yield_now().await;
    assert_eq!(desktop.front.load(Ordering::SeqCst), 1);
    assert_eq!(desktop.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn chain_target_uses_latest_window_for_repeated_pid() {
    let fence: &'static Fence = Box::leak(Box::new(test_fence()));
    let (a, a_guard) = fence.admit_action(true).await;
    let (b, b_guard) = fence.admit_action(true).await;
    let (c, c_guard) = fence.admit_action(true).await;
    for (pid, epoch, prior, wid) in [(2, a, 1, 10), (3, b, 2, 30), (2, c, 3, 20)] {
        spawn_restore_with_window(
            fence,
            RestoreTarget {
                pid,
                window_id: Some(wid),
                stamp: fake_identity(pid),
            },
            epoch,
            Some(prior),
            || RestoreCheck::Valid,
            || async { true },
        );
    }
    assert_eq!(fence.chain_target(2, a, Some(2)), (2, Some(20)));
    drop(a_guard);
    drop(b_guard);
    drop(c_guard);
}

#[tokio::test(start_paused = true)]
async fn same_pid_click_updates_window_without_new_obligation() {
    let fence: &'static Fence = Box::leak(Box::new(test_fence()));
    let (a, first) = fence.admit_action(true).await;
    spawn_restore_with_window(
        fence,
        RestoreTarget {
            pid: 2,
            window_id: Some(10),
            stamp: fake_identity(2),
        },
        a,
        Some(1),
        || RestoreCheck::Valid,
        || async { true },
    );
    let (b, second) = fence.admit_action(true).await;
    fence.update_existing_target(2, Some(20), fake_identity(2), b);
    fence.update_existing_target(2, Some(99), fake_identity(2), a);
    assert_eq!(fence.chain_target(2, a, Some(2)), (2, Some(20)));
    assert_eq!(fence.obligations.lock().unwrap().len(), 1);
    drop(first);
    drop(second);
}

#[tokio::test(start_paused = true)]
async fn same_pid_window_updates_when_process_stamp_is_unreadable() {
    let fence: &'static Fence = Box::leak(Box::new(test_fence()));
    let (a, first) = fence.admit_action(true).await;
    spawn_restore_with_window(
        fence,
        RestoreTarget {
            pid: 2,
            window_id: Some(10),
            stamp: fake_identity(2),
        },
        a,
        Some(1),
        || RestoreCheck::Valid,
        || async { true },
    );
    let (b, second) = fence.admit_action(true).await;
    fence.update_existing_target(2, Some(20), None, b);
    assert_eq!(fence.chain_target(2, a, Some(2)), (2, Some(20)));
    drop(first);
    drop(second);
}

#[tokio::test(start_paused = true)]
async fn older_click_obligation_wins_even_if_registered_later() {
    let fence = Box::leak(Box::new(test_fence()));
    let (old, old_action) = fence.admit_action(true).await;
    let (new, new_action) = fence.admit_action(true).await;
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
        fence
            .obligations
            .lock()
            .unwrap()
            .get(&2)
            .map(|value| (value.epoch, value.prior)),
        Some((old, Some(1)))
    );
    drop(new_action);
    drop(old_action);
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(50)).await;
    tokio::task::yield_now().await;
    assert_eq!(restored.load(Ordering::SeqCst), 1);
    assert_eq!(fence.take_outcome(2).unwrap()["prior_pid"], 1);
}
