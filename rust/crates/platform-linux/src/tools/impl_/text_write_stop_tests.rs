use super::*;
use crate::atspi::native::{
    gated_route, text_write_stop, text_write_timeout_error, TextWriteDispatch,
};
use crate::atspi::snapshot::build_snapshot;
use crate::atspi::{AtspiIdentity, AtspiNode};
use crate::token_action_policy::TextWriteStop;
use cua_driver_core::element_token::token_for;
use std::sync::atomic::{AtomicUsize, Ordering};

// Linux pid_max is bounded well below this value.
const PID: u32 = 2_000_000_001;
const INDEX: usize = 4;

fn published_token(state: &ToolState) -> String {
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
            bus_name: ":1.43".into(),
            path: "/text".into(),
            frame_bus_name: ":1.43".into(),
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
    token_for(snapshot, INDEX)
}

async fn set_value_with(outcome: SetValueTestOutcome) -> (ToolResult, usize) {
    let state = ToolState::new();
    let token = published_token(&state);
    let hooks = Arc::new(SetValueTestHooks {
        ax: outcome,
        keyboard_fallbacks: AtomicUsize::new(0),
    });
    let tool = SetValueTool {
        state,
        test_hooks: Some(hooks.clone()),
    };
    let result = tool
        .invoke(json!({ "pid": PID, "element_token": token, "value": "hello" }))
        .await;
    (result, hooks.keyboard_fallbacks.load(Ordering::SeqCst))
}

#[tokio::test]
async fn set_value_stopped_write_never_reaches_keyboard_fallback() {
    for (outcome, code) in [
        (
            SetValueTestOutcome::Uncertain,
            "element_write_unacknowledged",
        ),
        (SetValueTestOutcome::Partial, "element_write_partial"),
        (
            SetValueTestOutcome::UncertainAfterTimeout,
            "element_write_unacknowledged",
        ),
    ] {
        let (result, fallbacks) = set_value_with(outcome).await;
        assert_eq!(fallbacks, 0, "{outcome:?}: keyboard fallback wrote again");
        assert_eq!(result.is_error, Some(true), "{outcome:?}");
        let fields = result.structured_content.expect("structured result");
        assert_eq!(fields["code"], code, "{outcome:?}");
        assert_eq!(fields["effect"], "unverifiable", "{outcome:?}");
        assert_eq!(fields["element_index"], INDEX, "{outcome:?}");
        assert!(
            !fields.to_string().contains("hello"),
            "{outcome:?}: value leaked"
        );
    }
}

#[tokio::test]
async fn set_value_without_a_value_route_reaches_keyboard_fallback() {
    let (result, fallbacks) = set_value_with(SetValueTestOutcome::NoRoute).await;
    assert_eq!(fallbacks, 1);
    assert_eq!(
        result.structured_content.expect("structured result")["test_fallthrough"],
        true
    );
}

#[test]
fn stop_classification_survives_context() {
    use crate::text_write_policy::{Step, WriteStep};
    let step = || WriteStep {
        step: Step::InsertText,
        bytes: 5,
        units: Some(5),
    };
    let uncertain = anyhow::Error::from(crate::atspi::native::WriteUncertain { step: step() })
        .context("type_text did not complete");
    assert_eq!(text_write_stop(&uncertain), Some(TextWriteStop::Uncertain));
    let partial = anyhow::Error::from(crate::atspi::native::WritePartial { step: step() })
        .context("set_value task failed");
    assert_eq!(text_write_stop(&partial), Some(TextWriteStop::Partial));
    assert_eq!(text_write_stop(&anyhow::anyhow!("no editable")), None);

    let closed = TextWriteDispatch::default();
    assert!(text_write_timeout_error(&closed, 5).is_none());
    let mut ran = false;
    let refused = gated_route(&closed, || {
        ran = true;
        Ok(())
    })
    .unwrap_err();
    assert!(!ran);
    assert_eq!(text_write_stop(&refused), Some(TextWriteStop::Closed));
}

#[tokio::test]
async fn deadline_before_any_write_reports_none_and_blocks_late_writes() {
    let dispatch = Arc::new(TextWriteDispatch::default());
    let worker_dispatch = dispatch.clone();
    let writes = Arc::new(AtomicUsize::new(0));
    let worker_writes = writes.clone();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let (refused_tx, refused_rx) = tokio::sync::oneshot::channel::<bool>();
    let (deadline_tx, deadline_rx) = tokio::sync::oneshot::channel::<()>();
    deadline_tx.send(()).unwrap();
    let result = spawn_text_write_until(
        "type_text",
        std::time::Duration::from_secs(12),
        async {
            let _ = deadline_rx.await;
        },
        &[dispatch],
        5,
        move || {
            released.recv().unwrap();
            let late = gated_route(&worker_dispatch, || {
                worker_writes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });
            let _ = refused_tx.send(late.is_err());
            late
        },
    )
    .await
    .expect("not a join error");
    let error = result.unwrap_err();
    assert_eq!(text_write_stop(&error), None);
    assert!(error
        .to_string()
        .starts_with(crate::input::foreground::CODE_TIMEOUT));
    release.send(()).unwrap();
    assert!(refused_rx.await.unwrap(), "late write was not refused");
    assert_eq!(writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn deadline_during_a_write_is_unverifiable() {
    let dispatch = Arc::new(TextWriteDispatch::default());
    let worker_dispatch = dispatch.clone();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let (deadline_tx, deadline_rx) = tokio::sync::oneshot::channel::<()>();
    let dispatches = [Arc::new(TextWriteDispatch::default()), dispatch];
    let call = spawn_text_write_until(
        "type_text",
        std::time::Duration::from_secs(12),
        async {
            let _ = deadline_rx.await;
        },
        &dispatches,
        5,
        move || {
            gated_route(&worker_dispatch, || {
                entered_tx.send(()).unwrap();
                released.recv().unwrap();
                Ok(())
            })
        },
    );
    let driver = async {
        entered_rx.await.unwrap();
        deadline_tx.send(()).unwrap();
    };
    let (result, ()) = tokio::join!(call, driver);
    let error = result.expect("not a join error").unwrap_err();
    assert_eq!(text_write_stop(&error), Some(TextWriteStop::Uncertain));
    let stopped = text_write_stop_result("type_text", None, &error).expect("stopped write");
    let fields = stopped.structured_content.expect("structured result");
    assert_eq!(fields["code"], "element_write_unacknowledged");
    assert_eq!(fields["effect"], "unverifiable");
    release.send(()).unwrap();
}

#[tokio::test]
async fn panicked_writer_is_unverifiable() {
    let dispatch = Arc::new(TextWriteDispatch::default());
    let worker_dispatch = dispatch.clone();
    let result = spawn_text_write_until(
        "type_text",
        std::time::Duration::from_secs(12),
        std::future::pending::<()>(),
        &[dispatch],
        5,
        move || -> anyhow::Result<()> {
            gated_route(&worker_dispatch, || -> anyhow::Result<()> {
                panic!("test writer panic")
            })
        },
    )
    .await
    .expect("entered write is not a bare join error");
    assert_eq!(
        text_write_stop(&result.unwrap_err()),
        Some(TextWriteStop::Uncertain)
    );
}

#[tokio::test]
async fn panicked_worker_before_any_write_stays_a_join_error() {
    let result = spawn_text_write_until(
        "type_text",
        std::time::Duration::from_secs(12),
        std::future::pending::<()>(),
        &[Arc::new(TextWriteDispatch::default())],
        5,
        || -> anyhow::Result<()> { panic!("test worker panic") },
    )
    .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn completed_write_passes_through() {
    let result = spawn_text_write_until(
        "type_text",
        std::time::Duration::from_secs(12),
        std::future::pending::<()>(),
        &[Arc::new(TextWriteDispatch::default())],
        5,
        || Ok(7),
    )
    .await;
    assert_eq!(result.unwrap().unwrap(), 7);
}

#[test]
fn gated_steps_stop_once_the_caller_closed_the_gate() {
    let gate = TextWriteDispatch::default();
    let ran = std::cell::RefCell::new(Vec::new());
    let result = gated_steps(
        &gate,
        vec![
            Box::new(|| {
                ran.borrow_mut().push("click");
                // The caller times out while the click is in flight.
                assert!(text_write_timeout_error(&gate, 5).is_some());
                Ok(())
            }),
            Box::new(|| {
                ran.borrow_mut().push("select_all");
                Ok(())
            }),
            Box::new(|| {
                ran.borrow_mut().push("type");
                Ok(())
            }),
            Box::new(|| {
                ran.borrow_mut().push("tab");
                Ok(())
            }),
        ],
    );
    assert_eq!(*ran.borrow(), vec!["click"]);
    assert_eq!(
        text_write_stop(&result.unwrap_err()),
        Some(TextWriteStop::Closed)
    );
}

#[test]
fn gated_steps_run_every_step_while_open() {
    let gate = TextWriteDispatch::default();
    let ran = std::cell::RefCell::new(0);
    let step = || -> anyhow::Result<()> {
        *ran.borrow_mut() += 1;
        Ok(())
    };
    gated_steps(&gate, vec![Box::new(step), Box::new(step), Box::new(step)]).unwrap();
    assert_eq!(*ran.borrow(), 3);
}
