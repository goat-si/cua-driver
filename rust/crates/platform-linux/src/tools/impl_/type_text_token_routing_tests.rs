use super::*;
use crate::atspi::snapshot::build_snapshot;
use crate::atspi::{AtspiIdentity, AtspiNode};
use cua_driver_core::element_token::token_for;

// Linux pid_max is bounded well below this value, so process classification
// cannot accidentally find an unrelated Chromium/WebKit process.
const PID: u32 = 2_000_000_000;
const INDEX: usize = 3;

async fn exercise(branch: TypeTextBranch, outcome: TypeTextTestOutcome) {
    let state = ToolState::new();
    let node = AtspiNode {
        element_index: Some(INDEX),
        role: "text".into(),
        name: None,
        value: None,
        checked: None,
        enabled: Some(true),
        selected: None,
        toggle_state: None,
        expanded: None,
        description: None,
        actions: Vec::new(),
        element_key: 0,
        identity: Some(AtspiIdentity {
            bus_name: ":1.42".into(),
            path: "/text".into(),
            frame_bus_name: ":1.42".into(),
            frame_path: "/frame".into(),
        }),
        depth: 0,
        parent_element_index: None,
        in_web_content: false,
        object_ref: None,
    };
    let snapshot = state.snapshots.publish(
        PID as i32,
        0,
        build_snapshot(&[node], &[(INDEX, 10, 20, 30, 40)]),
    );
    let hooks = Arc::new(TypeTextTestHooks {
        hyprland_foreground: branch == TypeTextBranch::HyprlandForeground,
        inject_mode: branch == TypeTextBranch::Inject,
        chromium_or_webkit: false,
        outcome,
        fallthroughs: std::sync::Mutex::new(Vec::new()),
    });
    let tool = TypeTextTool {
        state,
        test_hooks: Some(hooks.clone()),
    };
    let result = tool
        .invoke(json!({
            "pid": PID,
            "element_token": token_for(snapshot, INDEX),
            "text": "hello",
            "delivery_mode": "foreground",
        }))
        .await;
    let calls = hooks.fallthroughs.lock().unwrap().clone();
    let fields = result.structured_content.expect("structured result");
    match outcome {
        TypeTextTestOutcome::NoRoute => {
            assert_eq!(calls, vec![branch]);
            assert_eq!(fields["test_fallthrough"], true);
        }
        TypeTextTestOutcome::Ok => {
            assert!(calls.is_empty(), "unexpected fallback: {calls:?}");
            assert_ne!(result.is_error, Some(true));
            assert_eq!(fields["path"], "ax");
            assert_eq!(fields["effect"], "unverifiable");
        }
        failure => {
            assert!(calls.is_empty(), "unexpected fallback: {calls:?}");
            assert_eq!(result.is_error, Some(true));
            let (code, effect) = match failure {
                TypeTextTestOutcome::Unavailable => ("token_route_unavailable", "none"),
                TypeTextTestOutcome::Gone => ("stale_element_token", "none"),
                TypeTextTestOutcome::Unknown | TypeTextTestOutcome::Panic => {
                    ("element_write_unacknowledged", "unverifiable")
                }
                _ => unreachable!(),
            };
            assert_eq!(fields["code"], code);
            assert_eq!(fields["effect"], effect);
        }
    }
}

macro_rules! cases {
    ($branch:expr, $unavailable:ident, $gone:ident, $unknown:ident, $panic:ident, $ok:ident, $no_route:ident) => {
        #[tokio::test]
        async fn $unavailable() {
            exercise($branch, TypeTextTestOutcome::Unavailable).await;
        }
        #[tokio::test]
        async fn $gone() {
            exercise($branch, TypeTextTestOutcome::Gone).await;
        }
        #[tokio::test]
        async fn $unknown() {
            exercise($branch, TypeTextTestOutcome::Unknown).await;
        }
        #[tokio::test]
        async fn $panic() {
            exercise($branch, TypeTextTestOutcome::Panic).await;
        }
        #[tokio::test]
        async fn $ok() {
            exercise($branch, TypeTextTestOutcome::Ok).await;
        }
        #[tokio::test]
        async fn $no_route() {
            exercise($branch, TypeTextTestOutcome::NoRoute).await;
        }
    };
}
cases!(
    TypeTextBranch::HyprlandForeground,
    hyprland_unavailable,
    hyprland_gone,
    hyprland_unknown,
    hyprland_panic,
    hyprland_ok,
    hyprland_no_route
);
cases!(
    TypeTextBranch::Inject,
    inject_unavailable,
    inject_gone,
    inject_unknown,
    inject_panic,
    inject_ok,
    inject_no_route
);
cases!(
    TypeTextBranch::Targeted,
    targeted_unavailable,
    targeted_gone,
    targeted_unknown,
    targeted_panic,
    targeted_ok,
    targeted_no_route
);
