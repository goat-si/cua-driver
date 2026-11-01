//! Evidence assembly and dispatch-independent decisions for macOS actionability.
use crate::ax::actionability::{observe, Reader};
use cua_driver_core::actionability::{
    self, ActionabilityReport, Affordance, Clock, CoordinateOutcome, ElementFacts, Evidence, Hit,
    Intent, Point, Policy, StabilitySampler, Verdict,
};
use cua_driver_core::protocol::ToolResult;
use std::{future::Future, time::Duration};

pub(super) mod runner;
pub(super) mod scope;
pub(super) mod semantic;
#[cfg(test)]
#[path = "actionability/order_tests.rs"]
mod u31_pointer_order_tests;
#[cfg(test)]
#[path = "actionability/pointer_tests.rs"]
mod u31_pointer_tests;
#[cfg(test)]
#[path = "actionability/scope_tests.rs"]
mod u31_scope_tests;
#[cfg(test)]
#[path = "actionability/target_tests.rs"]
mod u31_semantic_target_tests;

pub(crate) fn enabled<R: Reader>(reader: &mut R, element: R::Element) -> Evidence<bool> {
    observe(reader.boolean(element, "AXEnabled"))
}

pub(crate) fn advertised_actions<R: Reader>(
    reader: &mut R,
    element: R::Element,
) -> Evidence<Vec<String>> {
    observe(reader.actions(element))
}
pub(crate) fn selectable_text<R: Reader>(reader: &mut R, element: R::Element) -> Evidence<bool> {
    observe(reader.settable(element, "AXSelectedTextRange"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    Foreground,
    WindowInternal,
}

/// All observations use the same element and delivery scope. No implementation
/// may turn a failed hit test into a negative result or read occluder names.
pub(crate) trait ScopeReader<E> {
    fn visible(&mut self, target: E, window: E, scope: Scope) -> Evidence<bool>;
    fn offscreen(&mut self, target: E, window: E, scope: Scope) -> Evidence<bool>;
    fn hit(&mut self, target: E, window: E, scope: Scope, point: Point) -> Hit;
}

pub(crate) struct Adapter<E> {
    target: E,
    window: E,
    scope: Scope,
    stability: StabilitySampler,
}
impl<E: Copy> Adapter<E> {
    pub(crate) fn new(target: E, window: E, scope: Scope) -> Self {
        Self {
            target,
            window,
            scope,
            stability: StabilitySampler::default(),
        }
    }
    pub(crate) fn facts<R: Reader<Element = E>, H: ScopeReader<E>, C: Clock>(
        &mut self,
        reader: &mut R,
        scope_reader: &mut H,
        clock: &C,
    ) -> ElementFacts {
        self.facts_with_hits(reader, scope_reader, clock, true)
    }

    /// Facts for semantic AX actions evaluated under the headless policy.
    /// These actions do not require pointer hit evidence and must not perform
    /// a desktop or window hit test while observing actionability.
    pub(crate) fn semantic_facts<R: Reader<Element = E>, H: ScopeReader<E>, C: Clock>(
        &mut self,
        reader: &mut R,
        scope_reader: &mut H,
        clock: &C,
    ) -> ElementFacts {
        self.facts_with_hits(reader, scope_reader, clock, false)
    }

    fn facts_with_hits<R: Reader<Element = E>, H: ScopeReader<E>, C: Clock>(
        &mut self,
        reader: &mut R,
        scope_reader: &mut H,
        clock: &C,
        collect_hits: bool,
    ) -> ElementFacts {
        let target = self.target;
        let actions = advertised_actions(reader, target);
        let value_settable = observe(reader.settable(target, "AXValue"));
        let selection = observe(reader.settable(target, "AXSelected"));
        let focused = observe(reader.settable(target, "AXFocused"));
        let selected_text = observe(reader.settable(target, "AXSelectedText"));
        let selectable = selectable_text(reader, target);
        let editable = editability(&value_settable, &selected_text);
        let affordances = capabilities(
            &actions,
            [&value_settable, &selection, &focused, &selected_text],
        );
        let role = match observe(reader.string(target, "AXRole")) {
            Evidence::Known(role) => Evidence::Known(match role.as_str() {
                "AXTextField" | "AXTextArea" | "AXTextView" | "AXSearchField" => "textfield".into(),
                "AXComboBox" => "combobox".into(),
                _ => role,
            }),
            Evidence::Absent => Evidence::Absent,
            Evidence::Unknown(reason) => Evidence::Unknown(reason),
        };
        let bounds = observe(reader.frame(target));
        let stable = self.stability.observe(clock, bounds.clone());
        let receives_events = if !collect_hits {
            Evidence::Absent
        } else {
            match bounds {
                Evidence::Known(rect)
                    if rect.x.is_finite()
                        && rect.y.is_finite()
                        && rect.width.is_finite()
                        && rect.height.is_finite()
                        && rect.width > 0.0
                        && rect.height > 0.0 =>
                {
                    let mut hit = |p| match scope_reader.hit(target, self.window, self.scope, p) {
                        Hit::Occluded(mut other) => {
                            other.name = None;
                            Hit::Occluded(other)
                        }
                        other => other,
                    };
                    Evidence::Known(match self.scope {
                        Scope::Foreground => actionability::collect_hits(rect, &mut hit),
                        Scope::WindowInternal => actionability::collect_window_hits(rect, &mut hit),
                    })
                }
                Evidence::Known(_) => Evidence::Unknown("bounds cannot be hit tested".into()),
                Evidence::Unknown(ref reason) => Evidence::Unknown(reason.clone()),
                Evidence::Absent => Evidence::Absent,
            }
        };
        ElementFacts {
            visible: scope_reader.visible(target, self.window, self.scope),
            offscreen: scope_reader.offscreen(target, self.window, self.scope),
            enabled: enabled(reader, target),
            editable,
            stable,
            receives_events,
            bounds,
            affordances,
            advertised_actions: actions,
            selectable_text: selectable,
            role,
            nonspatial_visible: false,
        }
    }
    pub(crate) fn revalidate<R: Reader<Element = E>>(&self, reader: &mut R) -> Evidence<bool> {
        self.stability
            .revalidate(observe(reader.frame(self.target)))
    }
}

fn editability(value: &Evidence<bool>, selected_text: &Evidence<bool>) -> Evidence<bool> {
    if matches!(value, Evidence::Known(true)) || matches!(selected_text, Evidence::Known(true)) {
        return Evidence::Known(true);
    }
    if let Evidence::Unknown(reason) = value {
        return Evidence::Unknown(reason.clone());
    }
    if let Evidence::Unknown(reason) = selected_text {
        return Evidence::Unknown(reason.clone());
    }
    if matches!((value, selected_text), (Evidence::Absent, Evidence::Absent)) {
        Evidence::Absent
    } else {
        Evidence::Known(false)
    }
}

fn capabilities(
    actions: &Evidence<Vec<String>>,
    settables: [&Evidence<bool>; 4],
) -> Evidence<Vec<Affordance>> {
    if let Evidence::Unknown(reason) = actions {
        return Evidence::Unknown(reason.clone());
    }
    for item in settables {
        if let Evidence::Unknown(reason) = item {
            return Evidence::Unknown(reason.clone());
        }
    }
    let mut out = Vec::new();
    if let Evidence::Known(names) = actions {
        for (name, capability) in [
            ("AXPress", Affordance::Press),
            ("AXOpen", Affordance::Open),
            ("AXShowMenu", Affordance::ShowMenu),
            ("AXScrollToVisible", Affordance::ScrollIntoView),
        ] {
            if names.iter().any(|s| s == name) {
                out.push(capability);
            }
        }
    }
    for (item, capability) in settables.into_iter().zip([
        Affordance::SetValue,
        Affordance::Select,
        Affordance::Focus,
        Affordance::TypeText,
    ]) {
        if matches!(item, Evidence::Known(true)) {
            out.push(capability);
        }
    }
    Evidence::Known(out)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route<'a> {
    AxPress,
    AxShowMenu,
    AxOpen,
    Custom(&'a str),
    AxSelect,
    PhysicalDouble(bool),
    PhysicalTriple(bool),
    PhysicalRightClick(bool),
    ScrollButton,
    ScrollToVisible,
    ElementWheel(bool),
    Coordinate,
    RawKey(bool),
    NativeMenu,
    Focus,
    AxTypeText,
    KeyTypeText(bool),
    SetValue,
    SelectText,
    TypeTextChars,
    OverlayOnly,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Selection<'a> {
    Battery { intent: Intent<'a>, policy: Policy },
    Coordinate(CoordinateOutcome),
    OutsideBattery,
}
pub(crate) fn select(route: Route<'_>) -> Selection<'_> {
    use Route::*;
    let physical = |foreground| {
        if foreground {
            Policy::headed()
        } else {
            Policy::background_window()
        }
    };
    let keyboard = |foreground| {
        if foreground {
            Policy::headed()
        } else {
            Policy::focus_fallback()
        }
    };
    let (intent, policy) = match route {
        AxPress | ScrollButton | NativeMenu => (Intent::Click, Policy::headless()),
        AxShowMenu => (Intent::RightClick, Policy::headless()),
        AxOpen => (Intent::Open, Policy::headless()),
        Custom(name) => (Intent::PerformAction { name }, Policy::headless()),
        AxSelect => (Intent::Select, Policy::headless()),
        PhysicalDouble(f) => (Intent::DoubleClick, physical(f)),
        PhysicalTriple(f) => (Intent::TripleClick, physical(f)),
        PhysicalRightClick(f) => (Intent::RightClick, physical(f)),
        ScrollToVisible => (Intent::ScrollTo, Policy::headless()),
        ElementWheel(f) => (Intent::ScrollWheel, physical(f)),
        Coordinate => return Selection::Coordinate(actionability::coordinate_only()),
        RawKey(f) => (Intent::PressKey, keyboard(f)),
        Focus => (Intent::Focus, Policy::headless()),
        AxTypeText => (Intent::TypeText, Policy::headless()),
        KeyTypeText(f) => (Intent::TypeText, keyboard(f)),
        SetValue => (Intent::SetValue, Policy::headless()),
        SelectText => (Intent::SelectText, Policy::headless()),
        TypeTextChars => (Intent::TypeText, Policy::focus_fallback()),
        OverlayOnly => return Selection::OutsideBattery,
    };
    Selection::Battery { intent, policy }
}

pub(crate) fn evaluate(
    intent: Intent<'_>,
    policy: Policy,
    facts: &ElementFacts,
) -> ActionabilityReport {
    actionability::evaluate(intent, facts, policy)
}

pub(super) fn may_use_semantic_route(advertised: &Evidence<bool>) -> bool {
    matches!(advertised, Evidence::Known(true))
}
pub(crate) async fn wait<C, P, F>(
    clock: &C,
    budget: Duration,
    prologue_attempted: bool,
    probe: P,
) -> ActionabilityReport
where
    C: Clock + Sync,
    P: FnMut(Duration) -> F + Send,
    F: Future<Output = ActionabilityReport> + Send,
{
    actionability::auto_wait(clock, budget, prologue_attempted, probe).await
}
pub(super) fn semantic_clock() -> impl Clock + Sync {
    semantic::SemanticClock::default()
}

/// Dispatch one semantic action after its admitted target is sampled.
pub(super) async fn dispatch_semantic<C, P, A, F, T>(
    clock: &(impl Clock + Sync),
    budget: Duration,
    context: C,
    mut probe: P,
    perform: A,
) -> Result<T, ToolResult>
where
    C: Sync,
    P: for<'a> FnMut(
            &'a C,
            Duration,
        )
            -> std::pin::Pin<Box<dyn Future<Output = ActionabilityReport> + Send + 'a>>
        + Send,
    A: FnOnce(C) -> F,
    F: Future<Output = T>,
{
    let report = wait(clock, budget, false, |remaining| probe(&context, remaining)).await;
    match report.verdict {
        Verdict::Pass => Ok(perform(context).await),
        Verdict::Terminal { .. } => Err(report.refusal_result().expect("terminal report")),
        Verdict::Transient { .. } => unreachable!("auto_wait returns pass or terminal"),
    }
}

pub(super) fn click_probe<'a>(
    lease: &'a Option<crate::tools::BackgroundMutationLease>,
    target: crate::ax::snapshot::RetainedElement,
    pid: i32,
    window_id: u32,
) -> std::pin::Pin<Box<dyn Future<Output = ActionabilityReport> + Send + 'a>> {
    semantic_probe(
        lease.as_ref(),
        target,
        pid,
        window_id,
        semantic::SemanticIntent::Click,
    )
}

pub(super) fn semantic_probe<'a>(
    lease: Option<&'a crate::tools::BackgroundMutationLease>,
    target: crate::ax::snapshot::RetainedElement,
    pid: i32,
    window_id: u32,
    intent: semantic::SemanticIntent,
) -> std::pin::Pin<Box<dyn Future<Output = ActionabilityReport> + Send + 'a>> {
    Box::pin(async move {
        let bound = if let Some(lease) = lease {
            semantic::bind_exact_after_lease(lease, target, window_id).await
        } else {
            semantic::bind_exact_after_admission(pid, target, window_id).await
        };
        match bound {
            Evidence::Known(probe) => probe.sample(intent).await,
            Evidence::Unknown(reason) => semantic::unavailable(reason),
            Evidence::Absent => semantic::unavailable("target binding absent".into()),
        }
    })
}

pub(crate) enum ResultDecision {
    Continue(ToolResult),
    Wait(ToolResult),
    Refused(ToolResult),
    Preserve(ToolResult),
}
pub(crate) fn result(
    report: &ActionabilityReport,
    prologue_attempted: bool,
    existing: ToolResult,
) -> ResultDecision {
    match report.verdict {
        Verdict::Pass => ResultDecision::Continue(existing),
        Verdict::Transient { .. } => ResultDecision::Wait(existing),
        Verdict::Terminal { .. } if prologue_attempted => ResultDecision::Preserve(existing),
        Verdict::Terminal { .. } => {
            ResultDecision::Refused(report.refusal_result().expect("terminal report"))
        }
    }
}

#[cfg(test)]
mod u31_facts_tests {
    use super::*;
    use cua_driver_core::actionability::{evaluate, ElementFacts, Intent, Policy, Rect, Verdict};
    use std::{
        collections::HashMap,
        future::Future,
        pin::Pin,
        sync::atomic::{AtomicU64, Ordering},
    };

    struct Fake {
        bool_result: Evidence<bool>,
        actions_result: Evidence<Vec<String>>,
        settable_result: Evidence<bool>,
        settables: HashMap<String, Evidence<bool>>,
        role_result: Evidence<String>,
        frame_result: Evidence<Rect>,
        calls: Vec<String>,
        targets: Vec<u32>,
    }
    impl Default for Fake {
        fn default() -> Self {
            Self {
                bool_result: Evidence::Known(true),
                actions_result: Evidence::Known(Vec::new()),
                settable_result: Evidence::Known(true),
                settables: HashMap::new(),
                role_result: Evidence::Absent,
                frame_result: Evidence::Absent,
                calls: Vec::new(),
                targets: Vec::new(),
            }
        }
    }
    impl Fake {
        fn record(&mut self, target: u32, kind: &str, attr: &str) {
            // Asking whether an attribute is settable does not read its value.
            if kind != "settable" {
                assert_ne!(attr, "AXValue");
                assert_ne!(attr, "AXSelectedText");
            }
            self.targets.push(target);
            self.calls.push(format!("{kind}:{attr}"));
        }
    }
    impl Reader for Fake {
        type Element = u32;
        fn boolean(&mut self, target: u32, attr: &str) -> Evidence<bool> {
            self.record(target, "bool", attr);
            self.bool_result.clone()
        }
        fn string(&mut self, target: u32, attr: &str) -> Evidence<String> {
            self.record(target, "string", attr);
            self.role_result.clone()
        }
        fn actions(&mut self, target: u32) -> Evidence<Vec<String>> {
            self.record(target, "actions", "");
            self.actions_result.clone()
        }
        fn frame(&mut self, target: u32) -> Evidence<Rect> {
            self.record(target, "frame", "");
            self.frame_result.clone()
        }
        fn settable(&mut self, target: u32, attr: &str) -> Evidence<bool> {
            self.record(target, "settable", attr);
            self.settables
                .get(attr)
                .cloned()
                .unwrap_or_else(|| self.settable_result.clone())
        }
    }
    fn facts(actions: Evidence<Vec<String>>) -> ElementFacts {
        ElementFacts {
            visible: Evidence::Known(true),
            offscreen: Evidence::Known(false),
            enabled: Evidence::Known(true),
            editable: Evidence::Absent,
            stable: Evidence::Known(true),
            receives_events: Evidence::Absent,
            bounds: Evidence::Known(Rect {
                x: 0.0,
                y: 0.0,
                width: 50.0,
                height: 50.0,
            }),
            affordances: Evidence::Known(Vec::new()),
            advertised_actions: actions,
            selectable_text: Evidence::Absent,
            role: Evidence::Absent,
            nonspatial_visible: false,
        }
    }

    #[test]
    fn failed_attribute_is_unknown() {
        let mut reader = Fake {
            bool_result: Evidence::Unknown("AX error".into()),
            ..Fake::default()
        };
        assert!(matches!(enabled(&mut reader, 7), Evidence::Unknown(_)));
        assert_eq!(reader.calls, ["bool:AXEnabled"]);
    }

    #[test]
    fn wrong_typed_attribute_is_unknown() {
        let mut reader = Fake {
            bool_result: crate::ax::actionability::classify_attribute(
                crate::ax::bindings::kAXErrorSuccess,
                true,
                None,
                "AXEnabled",
            ),
            ..Fake::default()
        };
        assert!(matches!(enabled(&mut reader, 7), Evidence::Unknown(_)));
        assert_eq!(reader.calls, ["bool:AXEnabled"]);
    }

    #[test]
    fn failed_action_names_block_custom_action() {
        let mut reader = Fake {
            actions_result: Evidence::Unknown("AX error".into()),
            ..Fake::default()
        };
        let result = evaluate(
            Intent::PerformAction { name: "AXCustom" },
            &facts(advertised_actions(&mut reader, 7)),
            Policy::headless(),
        );
        assert!(matches!(
            result.verdict,
            Verdict::Transient { unknown: true, .. }
        ));
        assert_eq!(reader.calls, ["actions:"]);
        let assembled = adapter_facts(&mut reader, &mut FakeScope::default());
        assert!(matches!(assembled.advertised_actions, Evidence::Unknown(_)));
        assert!(matches!(
            evaluate(
                Intent::PerformAction { name: "AXCustom" },
                &assembled,
                Policy::headless()
            )
            .verdict,
            Verdict::Transient { unknown: true, .. }
        ));
        let mut scope = FakeScope::default();
        let result = semantic::sample_with(
            7,
            13,
            &mut reader,
            &mut scope,
            &FakeClock::default(),
            &mut StabilitySampler::default(),
            &semantic::SemanticIntent::PerformAction("AXCustom".into()),
        );
        assert!(matches!(
            result.verdict,
            Verdict::Transient { unknown: true, .. }
        ));
        reader.actions_result = Evidence::Known(
            ["AXPress", "AXOpen", "AXShowMenu", "AXCancel"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        );
        for intent in [
            semantic::SemanticIntent::Click,
            semantic::SemanticIntent::Open,
            semantic::SemanticIntent::RightClick,
            semantic::SemanticIntent::Select,
            semantic::SemanticIntent::Focus,
            semantic::SemanticIntent::PerformAction("AXCancel".into()),
        ] {
            let report = semantic::sample_with(
                7,
                13,
                &mut reader,
                &mut scope,
                &FakeClock::default(),
                &mut StabilitySampler::default(),
                &intent,
            );
            assert_eq!(report.verdict, Verdict::Pass);
        }
        assert!(scope.calls.iter().all(|(_, _, _, kind)| *kind != "hit"));
    }

    #[test]
    fn failed_range_support_is_unknown() {
        let mut reader = Fake {
            settable_result: Evidence::Unknown("AX error".into()),
            ..Fake::default()
        };
        assert!(matches!(
            selectable_text(&mut reader, 7),
            Evidence::Unknown(_)
        ));
        assert_eq!(reader.calls, ["settable:AXSelectedTextRange"]);
        let assembled = adapter_facts(&mut reader, &mut FakeScope::default());
        assert!(matches!(assembled.selectable_text, Evidence::Unknown(_)));
    }

    #[derive(Default)]
    struct FakeClock(AtomicU64);
    impl Clock for FakeClock {
        fn now(&self) -> Duration {
            Duration::from_millis(self.0.load(Ordering::SeqCst))
        }
        fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            Box::pin(async move {
                self.0
                    .fetch_add(duration.as_millis() as u64, Ordering::SeqCst);
            })
        }
    }
    struct FakeScope {
        visible: Evidence<bool>,
        offscreen: Evidence<bool>,
        hit: Hit,
        calls: Vec<(u32, u32, Scope, &'static str)>,
        points: Vec<Point>,
    }
    impl Default for FakeScope {
        fn default() -> Self {
            Self {
                visible: Evidence::Known(true),
                offscreen: Evidence::Known(false),
                hit: Hit::ReachesTarget,
                calls: vec![],
                points: vec![],
            }
        }
    }
    impl ScopeReader<u32> for FakeScope {
        fn visible(&mut self, e: u32, w: u32, scope: Scope) -> Evidence<bool> {
            self.calls.push((e, w, scope, "visible"));
            self.visible.clone()
        }
        fn offscreen(&mut self, e: u32, w: u32, scope: Scope) -> Evidence<bool> {
            self.calls.push((e, w, scope, "offscreen"));
            self.offscreen.clone()
        }
        fn hit(&mut self, e: u32, w: u32, scope: Scope, point: Point) -> Hit {
            self.calls.push((e, w, scope, "hit"));
            self.points.push(point);
            self.hit.clone()
        }
    }
    fn rectangle() -> Rect {
        Rect {
            x: 4.,
            y: 8.,
            width: 80.,
            height: 30.,
        }
    }
    fn adapter_facts(reader: &mut Fake, scope: &mut FakeScope) -> ElementFacts {
        Adapter::new(7, 13, Scope::WindowInternal).facts(reader, scope, &FakeClock::default())
    }

    #[test]
    fn known_false_is_preserved() {
        let mut reader = Fake {
            bool_result: Evidence::Known(false),
            ..Fake::default()
        };
        assert_eq!(enabled(&mut reader, 7), Evidence::Known(false));
        assert_eq!(reader.calls, ["bool:AXEnabled"]);
    }
    #[test]
    fn unsupported_attribute_is_absent() {
        let mut reader = Fake {
            bool_result: crate::ax::actionability::classify_attribute(
                crate::ax::bindings::kAXErrorAttributeUnsupported,
                false,
                None,
                "AXEnabled",
            ),
            ..Fake::default()
        };
        assert_eq!(enabled(&mut reader, 7), Evidence::Absent);
        assert_eq!(reader.calls, ["bool:AXEnabled"]);
    }
    #[test]
    fn advertised_actions_keep_exact_names() {
        let names = vec!["AXPress".to_string(), "AXCustom: MiXeD".to_string()];
        let mut reader = Fake {
            actions_result: Evidence::Known(names.clone()),
            ..Fake::default()
        };
        let got = adapter_facts(&mut reader, &mut FakeScope::default());
        assert_eq!(got.advertised_actions, Evidence::Known(names));
        assert!(
            matches!(got.affordances, Evidence::Known(ref a) if a.contains(&Affordance::Press))
        );
        assert_eq!(reader.calls.iter().filter(|v| *v == "actions:").count(), 1);
    }
    #[test]
    fn selectable_text_requires_confirmed_range_support() {
        let mut reader = Fake {
            role_result: Evidence::Known("AXTextField".into()),
            ..Fake::default()
        };
        reader
            .settables
            .insert("AXSelectedTextRange".into(), Evidence::Known(false));
        let result = adapter_facts(&mut reader, &mut FakeScope::default());
        assert_eq!(result.selectable_text, Evidence::Known(false));
        reader
            .settables
            .insert("AXSelectedTextRange".into(), Evidence::Known(true));
        let result = adapter_facts(&mut reader, &mut FakeScope::default());
        assert_eq!(result.selectable_text, Evidence::Known(true));
        assert_eq!(
            reader
                .calls
                .iter()
                .filter(|v| *v == "settable:AXSelectedTextRange")
                .count(),
            2
        );
    }
    #[test]
    fn role_and_editability_preserve_evidence() {
        let mut reader = Fake {
            role_result: Evidence::Known("AXTextField".into()),
            ..Fake::default()
        };
        reader
            .settables
            .insert("AXValue".into(), Evidence::Known(false));
        reader
            .settables
            .insert("AXSelectedText".into(), Evidence::Known(false));
        let result = adapter_facts(&mut reader, &mut FakeScope::default());
        assert_eq!(result.role, Evidence::Known("textfield".into()));
        assert_eq!(result.editable, Evidence::Known(false));
        reader
            .settables
            .insert("AXSelectedText".into(), Evidence::Known(true));
        let result = adapter_facts(&mut reader, &mut FakeScope::default());
        assert_eq!(result.editable, Evidence::Known(true));
        reader
            .settables
            .insert("AXSelectedText".into(), Evidence::Known(false));
        reader.role_result = Evidence::Unknown("role unavailable".into());
        reader.settables.insert(
            "AXValue".into(),
            Evidence::Unknown("settable unavailable".into()),
        );
        let result = adapter_facts(&mut reader, &mut FakeScope::default());
        assert!(matches!(result.role, Evidence::Unknown(_)));
        assert!(matches!(result.editable, Evidence::Unknown(_)));
    }
    #[test]
    fn window_visibility_and_geometry_preserve_unknown() {
        let mut reader = Fake {
            frame_result: Evidence::Unknown("frame failed".into()),
            ..Fake::default()
        };
        let mut scope = FakeScope {
            visible: Evidence::Unknown("visibility failed".into()),
            offscreen: Evidence::Unknown("offscreen failed".into()),
            ..FakeScope::default()
        };
        let result = adapter_facts(&mut reader, &mut scope);
        assert!(matches!(result.visible, Evidence::Unknown(_)));
        assert!(matches!(result.offscreen, Evidence::Unknown(_)));
        assert!(matches!(result.bounds, Evidence::Unknown(_)));
        assert!(matches!(result.stable, Evidence::Unknown(_)));
        assert!(matches!(result.receives_events, Evidence::Unknown(_)));
        assert_eq!(scope.calls.len(), 2);
        reader.frame_result = Evidence::Known(Rect {
            width: 0.0,
            ..rectangle()
        });
        let result = adapter_facts(&mut reader, &mut scope);
        assert!(matches!(result.receives_events, Evidence::Unknown(_)));
        assert_eq!(
            scope
                .calls
                .iter()
                .filter(|(_, _, _, call)| *call == "hit")
                .count(),
            0
        );
    }
    #[test]
    fn stability_samples_and_revalidation_use_the_same_target() {
        let mut reader = Fake {
            frame_result: Evidence::Known(rectangle()),
            ..Fake::default()
        };
        let mut scope = FakeScope::default();
        let clock = FakeClock::default();
        let mut adapter = Adapter::new(7, 13, Scope::WindowInternal);
        for (time, expected) in [(0, false), (17, false), (34, true)] {
            clock.0.store(time, Ordering::SeqCst);
            assert_eq!(
                adapter.facts(&mut reader, &mut scope, &clock).stable,
                Evidence::Known(expected)
            );
        }
        assert_eq!(adapter.revalidate(&mut reader), Evidence::Known(true));
        reader.frame_result = Evidence::Known(Rect {
            x: 100.,
            ..rectangle()
        });
        assert_eq!(adapter.revalidate(&mut reader), Evidence::Known(false));
        assert!(reader.targets.iter().all(|id| *id == 7));
        assert_eq!(reader.calls.iter().filter(|v| *v == "frame:").count(), 5);
        assert!(scope
            .calls
            .iter()
            .all(|(e, w, s, _)| (*e, *w, *s) == (7, 13, Scope::WindowInternal)));
        // The blocking semantic-sample seam carries only the sampler across
        // reads; target and window are supplied afresh on every observation.
        reader.frame_result = Evidence::Known(rectangle());
        reader.actions_result = Evidence::Known(vec!["AXOpen".into()]);
        let mut stability = StabilitySampler::default();
        let mut semantic_scope = FakeScope::default();
        for (time, expected) in [(100, false), (117, false), (134, true)] {
            clock.0.store(time, Ordering::SeqCst);
            let report = semantic::sample_with(
                7,
                13,
                &mut reader,
                &mut semantic_scope,
                &clock,
                &mut stability,
                &semantic::SemanticIntent::Open,
            );
            assert_eq!(report.last_facts.unwrap().stable, Evidence::Known(expected));
        }
        assert!(semantic_scope
            .calls
            .iter()
            .all(|(target, window, scope, kind)| {
                (*target, *window, *scope) == (7, 13, Scope::WindowInternal) && *kind != "hit"
            }));
    }
    #[test]
    fn hit_evidence_preserves_delivery_scope() {
        let mut reader = Fake {
            frame_result: Evidence::Known(rectangle()),
            ..Fake::default()
        };
        let mut scope = FakeScope::default();
        let bg = Adapter::new(7, 13, Scope::WindowInternal).facts(
            &mut reader,
            &mut scope,
            &FakeClock::default(),
        );
        assert!(
            matches!(bg.receives_events, Evidence::Known(ref hit) if hit.target_window && hit.verified_point.is_some())
        );
        assert_eq!(scope.points[0], Point { x: 44.0, y: 23.0 });
        scope.hit = Hit::Unknown;
        let fg = Adapter::new(7, 13, Scope::Foreground).facts(
            &mut reader,
            &mut scope,
            &FakeClock::default(),
        );
        assert!(
            matches!(fg.receives_events, Evidence::Known(ref hit) if !hit.target_window && hit.unknown == 5 && hit.verified_point.is_none())
        );
        assert_eq!(
            scope
                .calls
                .iter()
                .filter(|(_, _, s, k)| *s == Scope::Foreground && *k == "hit")
                .count(),
            5
        );
        reader.actions_result = Evidence::Known(vec!["AXPress".into()]);
        let calls_before = scope.calls.len();
        let semantic = Adapter::new(7, 13, Scope::WindowInternal).semantic_facts(
            &mut reader,
            &mut scope,
            &FakeClock::default(),
        );
        assert_eq!(semantic.receives_events, Evidence::Absent);
        assert!(scope.calls[calls_before..]
            .iter()
            .all(|(_, _, _, kind)| *kind != "hit"));
        assert!(matches!(
            evaluate(Intent::Click, &semantic, Policy::headless()).verdict,
            Verdict::Pass
        ));
        reader.actions_result = Evidence::Unknown("action read failed".into());
        let semantic = Adapter::new(7, 13, Scope::WindowInternal).semantic_facts(
            &mut reader,
            &mut scope,
            &FakeClock::default(),
        );
        assert!(matches!(
            evaluate(
                Intent::PerformAction { name: "AXCustom" },
                &semantic,
                Policy::headless()
            )
            .verdict,
            Verdict::Transient { unknown: true, .. }
        ));
    }
}

#[cfg(test)]
mod u31_result_tests {
    use super::*;
    use cua_driver_core::{
        action_record::{ActionEffect, ActionExecutionRecord, ActionTransport, RequestedDelivery},
        actionability::DispositionIntent,
    };
    use std::{
        future::Future,
        pin::Pin,
        sync::atomic::{AtomicU64, Ordering},
    };

    #[derive(Default)]
    struct FakeClock(AtomicU64);
    impl Clock for FakeClock {
        fn now(&self) -> Duration {
            Duration::from_millis(self.0.load(Ordering::SeqCst))
        }
        fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            Box::pin(async move {
                self.0
                    .fetch_add(duration.as_millis() as u64, Ordering::SeqCst);
            })
        }
    }
    fn report(verdict: Verdict) -> ActionabilityReport {
        ActionabilityReport {
            verdict,
            checks: vec![],
            waited_ms: 0,
            samples: 1,
            last_facts: None,
            disposition: None,
        }
    }
    fn terminal() -> ActionabilityReport {
        let mut r = report(Verdict::Terminal {
            code: "unsupported_action",
            reason: "not supported".into(),
        });
        r.disposition = Some(DispositionIntent::NotDeliveredSafe);
        r
    }
    fn existing() -> ToolResult {
        ToolResult::text("existing")
            .with_structured(serde_json::json!({"effect":"unverifiable","dispatched":true,"note":"recover","warning":"keep"}))
            .with_action_record(ActionExecutionRecord::new(ActionEffect::Unverifiable, ActionTransport::MacosAxAction, RequestedDelivery::Background))
    }
    fn assert_existing(result: ToolResult) {
        let expected = existing();
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert_eq!(result.action_record, expected.action_record);
        assert_eq!(result.disposition, None);
    }
    #[test]
    fn pass_preserves_existing_result_fields() {
        match result(&report(Verdict::Pass), false, existing()) {
            ResultDecision::Continue(value) => assert_existing(value),
            _ => panic!("pass changed result"),
        }
    }
    #[test]
    fn transient_does_not_create_public_refusal() {
        let r = report(Verdict::Transient {
            reason: "retry".into(),
            unknown: true,
        });
        assert!(r.refusal_result().is_none());
        for prologue_attempted in [false, true] {
            match result(&r, prologue_attempted, existing()) {
                ResultDecision::Wait(value) => assert_existing(value),
                _ => panic!("transient report must wait without losing the result"),
            }
        }
    }
    #[test]
    fn preprologue_terminal_passes_core_refusal_unchanged() {
        let r = terminal();
        let expected = r.refusal_result().unwrap();
        match result(&r, false, existing()) {
            ResultDecision::Refused(value) => {
                assert_eq!(value.disposition, expected.disposition);
                assert_eq!(
                    serde_json::to_value(value).unwrap(),
                    serde_json::to_value(expected).unwrap()
                );
            }
            _ => panic!("terminal not forwarded"),
        }
    }
    #[tokio::test]
    async fn zero_budget_unknown_refuses_not_delivered_safe() {
        let clock = FakeClock::default();
        let r = wait(&clock, Duration::ZERO, false, |_| async {
            report(Verdict::Transient {
                reason: "unknown".into(),
                unknown: true,
            })
        })
        .await;
        assert!(matches!(
            r.verdict,
            Verdict::Terminal {
                code: "timeout",
                ..
            }
        ));
        assert_eq!(r.disposition, Some(DispositionIntent::NotDeliveredSafe));
        assert_eq!(
            r.refusal_result().unwrap().disposition,
            Some(cua_driver_contract::ErrorDisposition::NOT_DELIVERED)
        );
    }
    #[test]
    fn postprologue_failure_preserves_existing_unverifiable_result() {
        match result(&terminal(), true, existing()) {
            ResultDecision::Preserve(value) => assert_existing(value),
            _ => panic!("post-prologue result was replaced"),
        }
    }
    #[tokio::test]
    async fn uncertain_prologue_never_claims_safe_or_unsent() {
        let clock = FakeClock::default();
        let r = wait(&clock, Duration::ZERO, true, |_| async {
            report(Verdict::Transient {
                reason: "unknown".into(),
                unknown: true,
            })
        })
        .await;
        assert_eq!(r.disposition, None);
        match result(&r, true, existing()) {
            ResultDecision::Preserve(value) => {
                let body = value.structured_content.as_ref().unwrap();
                assert_ne!(body.get("dispatched"), Some(&serde_json::json!(false)));
                assert!(body.get("disposition").is_none());
                assert_existing(value);
            }
            _ => panic!("uncertain result was replaced"),
        }
    }
    #[test]
    fn coordinate_only_remains_not_evaluated() {
        assert_eq!(
            select(Route::Coordinate),
            Selection::Coordinate(CoordinateOutcome::NotEvaluated)
        );
        for (foreground, policy) in [
            (false, Policy::background_window()),
            (true, Policy::headed()),
        ] {
            assert_eq!(
                select(Route::ElementWheel(foreground)),
                Selection::Battery {
                    intent: Intent::ScrollWheel,
                    policy,
                }
            );
        }
        assert_eq!(
            select(Route::ScrollToVisible),
            Selection::Battery {
                intent: Intent::ScrollTo,
                policy: Policy::headless()
            }
        );
        assert_eq!(
            select(Route::ScrollButton),
            Selection::Battery {
                intent: Intent::Click,
                policy: Policy::headless()
            }
        );
        assert_eq!(select(Route::OverlayOnly), Selection::OutsideBattery);
        for (route, intent, policy) in [
            (Route::AxPress, Intent::Click, Policy::headless()),
            (Route::AxShowMenu, Intent::RightClick, Policy::headless()),
            (Route::AxOpen, Intent::Open, Policy::headless()),
            (
                Route::Custom("AXCustom"),
                Intent::PerformAction { name: "AXCustom" },
                Policy::headless(),
            ),
            (Route::AxSelect, Intent::Select, Policy::headless()),
            (
                Route::PhysicalDouble(false),
                Intent::DoubleClick,
                Policy::background_window(),
            ),
            (
                Route::PhysicalDouble(true),
                Intent::DoubleClick,
                Policy::headed(),
            ),
            (
                Route::PhysicalTriple(false),
                Intent::TripleClick,
                Policy::background_window(),
            ),
            (
                Route::PhysicalTriple(true),
                Intent::TripleClick,
                Policy::headed(),
            ),
            (
                Route::PhysicalRightClick(false),
                Intent::RightClick,
                Policy::background_window(),
            ),
            (
                Route::PhysicalRightClick(true),
                Intent::RightClick,
                Policy::headed(),
            ),
            (
                Route::RawKey(false),
                Intent::PressKey,
                Policy::focus_fallback(),
            ),
            (Route::RawKey(true), Intent::PressKey, Policy::headed()),
            (Route::NativeMenu, Intent::Click, Policy::headless()),
            (Route::Focus, Intent::Focus, Policy::headless()),
            (Route::AxTypeText, Intent::TypeText, Policy::headless()),
            (
                Route::KeyTypeText(false),
                Intent::TypeText,
                Policy::focus_fallback(),
            ),
            (Route::KeyTypeText(true), Intent::TypeText, Policy::headed()),
            (Route::SetValue, Intent::SetValue, Policy::headless()),
            (Route::SelectText, Intent::SelectText, Policy::headless()),
            (
                Route::TypeTextChars,
                Intent::TypeText,
                Policy::focus_fallback(),
            ),
        ] {
            assert_eq!(select(route), Selection::Battery { intent, policy });
        }
    }
    #[test]
    fn serialized_refusal_exposes_only_core_contract_fields() {
        let r = terminal();
        let ResultDecision::Refused(value) = result(&r, false, existing()) else {
            panic!("missing refusal");
        };
        let serialized = serde_json::to_value(value).unwrap();
        let body = serialized["structuredContent"].as_object().unwrap();
        assert_eq!(
            body.keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            ["code", "disposition", "effect", "refusal"]
                .into_iter()
                .collect()
        );
        assert_eq!(body["code"], "element_not_actionable");
        assert!(!body.contains_key("actionability"));
        assert!(!body.contains_key("dispatched"));
    }
}
