//! Evidence assembly and dispatch-independent decisions for macOS actionability.
use crate::ax::actionability::{observe, Reader};
use cua_driver_core::actionability::{
    self, ActionabilityReport, Affordance, Clock, CoordinateOutcome, ElementFacts, Evidence, Hit,
    Intent, Point, Policy, StabilitySampler, Verdict,
};
use cua_driver_core::protocol::ToolResult;
use std::{future::Future, time::Duration};

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

/// The existing element pointer hit reader collapses errors into misses; it
/// cannot prove either foreground or window-internal delivery evidence.
pub(crate) struct UnprovenScope;
impl<E> ScopeReader<E> for UnprovenScope {
    fn visible(&mut self, _: E, _: E, _: Scope) -> Evidence<bool> {
        Evidence::Unknown("visibility is unproven".into())
    }
    fn offscreen(&mut self, _: E, _: E, _: Scope) -> Evidence<bool> {
        Evidence::Unknown("offscreen state is unproven".into())
    }
    fn hit(&mut self, _: E, _: E, _: Scope, _: Point) -> Hit {
        Hit::Unknown
    }
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
        let target = self.target;
        let actions = advertised_actions(reader, target);
        let editable = observe(reader.settable(target, "AXValue"));
        let selection = observe(reader.settable(target, "AXSelected"));
        let focused = observe(reader.settable(target, "AXFocused"));
        let selected_text = observe(reader.settable(target, "AXSelectedText"));
        let selectable = selectable_text(reader, target);
        let affordances = capabilities(&actions, [&editable, &selection, &focused, &selected_text]);
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
        let receives_events = match bounds {
            Evidence::Known(rect) => {
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
            Evidence::Unknown(ref reason) => Evidence::Unknown(reason.clone()),
            Evidence::Absent => Evidence::Absent,
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
    ElementWheel,
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
    UnresolvedWheel,
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
        ElementWheel => return Selection::UnresolvedWheel,
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
pub(crate) enum ResultDecision {
    Continue(ToolResult),
    Wait,
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
        Verdict::Transient { .. } if prologue_attempted => ResultDecision::Preserve(existing),
        Verdict::Transient { .. } => ResultDecision::Wait,
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

    struct Fake {
        bool_result: Evidence<bool>,
        actions_result: Evidence<Vec<String>>,
        settable_result: Evidence<bool>,
        calls: Vec<String>,
    }
    impl Default for Fake {
        fn default() -> Self {
            Self {
                bool_result: Evidence::Known(true),
                actions_result: Evidence::Known(Vec::new()),
                settable_result: Evidence::Known(true),
                calls: Vec::new(),
            }
        }
    }
    impl Fake {
        fn record(&mut self, kind: &str, attr: &str) {
            assert_ne!(attr, "AXValue");
            assert_ne!(attr, "AXSelectedText");
            self.calls.push(format!("{kind}:{attr}"));
        }
    }
    impl Reader for Fake {
        type Element = u32;
        fn boolean(&mut self, _: u32, attr: &str) -> Evidence<bool> {
            self.record("bool", attr);
            self.bool_result.clone()
        }
        fn string(&mut self, _: u32, attr: &str) -> Evidence<String> {
            self.record("string", attr);
            Evidence::Absent
        }
        fn actions(&mut self, _: u32) -> Evidence<Vec<String>> {
            self.record("actions", "");
            self.actions_result.clone()
        }
        fn frame(&mut self, _: u32) -> Evidence<Rect> {
            self.record("frame", "");
            Evidence::Absent
        }
        fn settable(&mut self, _: u32, attr: &str) -> Evidence<bool> {
            self.record("settable", attr);
            self.settable_result.clone()
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
            bool_result: Evidence::Unknown("wrong CF type".into()),
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
    }
}
