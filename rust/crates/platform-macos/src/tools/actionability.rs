//! macOS evidence assembly for the core actionability battery.
use crate::ax::actionability::{observe, Reader};
use cua_driver_core::actionability::Evidence;

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
            bounds: Evidence::Known(Rect { x: 0.0, y: 0.0, width: 50.0, height: 50.0 }),
            affordances: Evidence::Known(Vec::new()),
            advertised_actions: actions,
            selectable_text: Evidence::Absent,
            role: Evidence::Absent,
            nonspatial_visible: false,
        }
    }

    #[test]
    fn failed_attribute_is_unknown() {
        let mut reader = Fake { bool_result: Evidence::Unknown("AX error".into()), ..Fake::default() };
        assert!(matches!(enabled(&mut reader, 7), Evidence::Unknown(_)));
        assert_eq!(reader.calls, ["bool:AXEnabled"]);
    }

    #[test]
    fn wrong_typed_attribute_is_unknown() {
        let mut reader = Fake { bool_result: Evidence::Unknown("wrong CF type".into()), ..Fake::default() };
        assert!(matches!(enabled(&mut reader, 7), Evidence::Unknown(_)));
        assert_eq!(reader.calls, ["bool:AXEnabled"]);
    }

    #[test]
    fn failed_action_names_block_custom_action() {
        let mut reader = Fake { actions_result: Evidence::Unknown("AX error".into()), ..Fake::default() };
        let result = evaluate(Intent::PerformAction { name: "AXCustom" }, &facts(advertised_actions(&mut reader, 7)), Policy::headless());
        assert!(matches!(result.verdict, Verdict::Transient { unknown: true, .. }));
        assert_eq!(reader.calls, ["actions:"]);
    }

    #[test]
    fn failed_range_support_is_unknown() {
        let mut reader = Fake { settable_result: Evidence::Unknown("AX error".into()), ..Fake::default() };
        assert!(matches!(selectable_text(&mut reader, 7), Evidence::Unknown(_)));
        assert_eq!(reader.calls, ["settable:AXSelectedTextRange"]);
    }
}
