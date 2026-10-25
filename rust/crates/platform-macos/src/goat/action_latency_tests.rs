use super::*;
use serde_json::json;

struct Probe {
    def: ToolDef,
    rendezvous: Option<Arc<tokio::sync::Barrier>>,
}

impl Probe {
    fn new() -> Self {
        Self {
            def: ToolDef {
                name: "click".into(),
                description: "test action".into(),
                input_schema: json!({}),
                read_only: false,
                destructive: false,
                idempotent: false,
                open_world: true,
            },
            rendezvous: None,
        }
    }
}

#[async_trait]
impl Tool for Probe {
    fn def(&self) -> &ToolDef {
        &self.def
    }
    fn has_independent_input_lane(&self, _: &Value) -> bool {
        true
    }
    async fn protected_resource_ownership(&self, _: &str, _: &Value) -> ProtectedResourceOwnership {
        ProtectedResourceOwnership::DriverOwned
    }
    async fn protected_resource_scope(&self, _: &str, _: &Value) -> Result<Option<Value>, String> {
        Ok(Some(json!({"test": 1})))
    }
    async fn validate_protected_resource_scope(
        &self,
        _: &str,
        _: &Value,
        _: &Value,
    ) -> Result<(), String> {
        Err("test refusal".into())
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        if let Some(barrier) = &self.rendezvous {
            barrier.wait().await;
        }
        let id = current().map(|t| t.id);
        let blocking_id = spawn_blocking(|| {
            let _guard = span(Stage::Protect);
            enumerate(|| current().map(|t| t.id))
        })
        .await
        .unwrap();
        assert_eq!(id, blocking_id);
        timed(Stage::Action, async { resolve(|| 7) }).await;
        detector_exit("skipped");
        ToolResult::error(format!("private response: {args}"))
    }
}

fn value<'a>(line: &'a str, key: &str) -> &'a str {
    line.split_whitespace()
        .find_map(|part| part.strip_prefix(&format!("{key}=")))
        .unwrap()
}

#[tokio::test]
async fn completion_emits_one_content_free_line_and_preserves_the_result() {
    let probe = Probe::new();
    let args =
        json!({"text":"SECRET_TEXT", "element_token":"SECRET_TOKEN", "title":"SECRET_TITLE"});
    let direct = probe.invoke(args.clone()).await;
    let mut lines = Vec::new();
    let traced = invoke_traced(&probe, "click", args, |line| lines.push(line)).await;
    assert_eq!(
        serde_json::to_value(direct).unwrap(),
        serde_json::to_value(traced).unwrap()
    );
    assert_eq!(lines.len(), 1);
    let line = &lines[0];
    assert!(!line.contains("SECRET"));
    assert_eq!(value(line, "outcome"), "error");
    assert_eq!(value(line, "detector_exit"), "skipped");
    assert_eq!(value(line, "detector_enumerations"), "1");
    let number = |key| value(line, key).parse::<u128>().unwrap();
    assert_eq!(
        number("total_us"),
        number("measured_us") + number("other_us")
    );
    assert!(current().is_none());
    assert!(spawn_blocking(|| current().is_none()).await.unwrap());
}

#[tokio::test]
async fn concurrent_actions_have_distinct_ids_across_blocking_work() {
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let run = |barrier| async move {
        let mut probe = Probe::new();
        probe.rendezvous = Some(barrier);
        let mut lines = Vec::new();
        invoke_traced(&probe, "click", json!({}), |line| lines.push(line)).await;
        lines.pop().unwrap()
    };
    let (one, two) = tokio::join!(run(barrier.clone()), run(barrier));
    assert_ne!(value(&one, "request_id"), value(&two, "request_id"));
    assert_eq!(value(&one, "detector_enumerations"), "1");
    assert_eq!(value(&two, "detector_enumerations"), "1");
}

#[tokio::test]
async fn nested_action_uses_the_outer_context_without_a_second_line() {
    let trace = Trace::new("click");
    CURRENT
        .scope(trace, async {
            invoke_traced(&Probe::new(), "click", json!({}), |_| panic!("nested line")).await;
        })
        .await;
}

#[tokio::test]
async fn decorator_preserves_metadata_and_protected_resource_decisions() {
    let tool = TracedTool {
        inner: Box::new(Probe::new()),
        name: "click",
    };
    assert_eq!(tool.def().name, "click");
    assert!(tool.has_independent_input_lane(&json!({})));
    assert!(matches!(
        tool.protected_resource_ownership("test", &json!({})).await,
        ProtectedResourceOwnership::DriverOwned
    ));
    assert_eq!(
        tool.protected_resource_scope("test", &json!({}))
            .await
            .unwrap(),
        Some(json!({"test":1}))
    );
    assert_eq!(
        tool.validate_protected_resource_scope("test", &json!({}), &json!({}))
            .await,
        Err("test refusal".into())
    );
}

#[test]
fn nested_and_overlapping_intervals_are_counted_once() {
    let ms = Duration::from_millis;
    let mut intervals = [
        (ms(3), ms(7)),
        (ms(1), ms(4)),
        (ms(9), ms(12)),
        (ms(2), ms(3)),
    ];
    assert_eq!(interval_union(&mut intervals, ms(10)), ms(7));
}

#[tokio::test]
async fn cancellation_emits_one_partial_record() {
    struct Pending;
    #[async_trait]
    impl Tool for Pending {
        fn def(&self) -> &ToolDef {
            unreachable!()
        }
        async fn invoke(&self, _: Value) -> ToolResult {
            let _span = span(Stage::Action);
            std::future::pending().await
        }
    }
    let mut lines = Vec::new();
    let mut future = Box::pin(invoke_traced(&Pending, "click", json!({}), |line| {
        lines.push(line)
    }));
    // Poll to pending without timers, sleeps, or a live macOS action.
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(future.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    drop(future);
    assert_eq!(lines.len(), 1);
    assert_eq!(value(&lines[0], "outcome"), "interrupted");
    assert!(current().is_none());
}

#[tokio::test]
async fn blocking_context_is_restored_after_panic() {
    let trace = Trace::new("click");
    CURRENT
        .scope(trace, async {
            assert!(spawn_blocking(|| panic!("test panic")).await.is_err());
        })
        .await;
    assert!(BLOCKING.with(|slot| slot.borrow().is_none()));
    assert!(spawn_blocking(|| current().is_none()).await.unwrap());
}

#[tokio::test]
async fn untraced_work_has_no_context_or_timer_and_keeps_its_result() {
    assert!(current().is_none());
    let timer = span(Stage::Snapshot);
    assert!(timer.0.is_none());
    let result = Probe::new().invoke(json!({"text":"PRIVATE"})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(current().is_none());
}
