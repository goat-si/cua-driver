use super::batch_attrs::{batch_failure_action, BatchFailureAction, BatchRead, BatchSlots};
use super::bindings::*;
use cua_driver_core::walk_budget::WalkBudget;

const CONTENT_ATTRIBUTES: [&str; 6] = [
    "AXTitle",
    "AXValue",
    "AXPlaceholderValue",
    "AXDescription",
    "AXIdentifier",
    "AXHelp",
];

pub(crate) struct ContentAttrs {
    pub title: Option<String>,
    pub copied_value: Option<StringishAttrValue>,
    pub value: Option<String>,
    pub placeholder: Option<String>,
    pub description: Option<String>,
    pub identifier: Option<String>,
    pub help: Option<String>,
}

fn value_with_placeholder(
    value: Option<&StringishAttrValue>,
    placeholder: impl FnOnce() -> Option<String>,
) -> Option<String> {
    value
        .and_then(|value| value.string_value.clone())
        .filter(|value| !value.trim().is_empty())
        .or_else(placeholder)
}

impl ContentAttrs {
    fn from_slots(slots: &BatchSlots) -> Self {
        let copied_value = slots.stringish(1);
        let placeholder = slots.string(2);
        let value = value_with_placeholder(copied_value.as_ref(), || placeholder.clone());
        Self {
            title: slots.string(0),
            copied_value,
            value,
            placeholder,
            description: slots.string(3),
            identifier: slots.string(4),
            help: slots.string(5).filter(|help| !help.trim().is_empty()),
        }
    }

    unsafe fn from_singles(element: AXUIElementRef) -> Self {
        let title = copy_string_attr(element, "AXTitle");
        let copied_value = copy_stringish_attr(element, "AXValue");
        let mut placeholder = None;
        let value = value_with_placeholder(copied_value.as_ref(), || {
            placeholder = copy_string_attr(element, "AXPlaceholderValue");
            placeholder.clone()
        });
        Self {
            title,
            copied_value,
            value,
            placeholder,
            description: copy_string_attr(element, "AXDescription"),
            identifier: copy_string_attr(element, "AXIdentifier"),
            help: copy_string_attr(element, "AXHelp").filter(|help| !help.trim().is_empty()),
        }
    }
}

/// A timeout returns no content, so the walker emits only the role already read.
pub(crate) unsafe fn read_content_attrs(
    element: AXUIElementRef,
    budget: &mut WalkBudget,
    batching_disabled: &mut bool,
) -> Option<ContentAttrs> {
    if !*batching_disabled {
        match copy_attrs_batch(element, &CONTENT_ATTRIBUTES) {
            BatchRead::Slots(slots) => return Some(ContentAttrs::from_slots(&slots)),
            BatchRead::Fallback(kind) => {
                let action = batch_failure_action(kind, budget.expired(), *batching_disabled);
                if !replay_after_failure(action, budget, batching_disabled) {
                    return None;
                }
            }
        }
    }
    Some(ContentAttrs::from_singles(element))
}

fn replay_after_failure(
    action: BatchFailureAction,
    budget: &mut WalkBudget,
    batching_disabled: &mut bool,
) -> bool {
    match action {
        BatchFailureAction::DisableAndReplay => *batching_disabled = true,
        BatchFailureAction::Replay => {}
        BatchFailureAction::StopForTimeout => {
            budget.stop_for_timeout();
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_foundation::{
        array::CFArray,
        base::{CFRetain, CFType, TCFType},
        boolean::CFBoolean,
        number::CFNumber,
        string::CFString,
    };

    fn content(value: &CFType, placeholder: &str) -> ContentAttrs {
        let array = CFArray::from_CFTypes(&[
            CFString::new("  title  ").as_CFType(),
            value.clone(),
            CFString::new(placeholder).as_CFType(),
            CFString::new("  description  ").as_CFType(),
            CFString::new("  identifier  ").as_CFType(),
            CFString::new("  help  ").as_CFType(),
        ]);
        let raw = array.as_CFTypeRef();
        let batch = unsafe {
            CFRetain(raw);
            super::super::batch_attrs::finish_batch(kAXErrorSuccess, raw, 6)
        };
        let BatchRead::Slots(slots) = batch else {
            panic!("valid content must decode")
        };
        ContentAttrs::from_slots(&slots)
    }

    #[test]
    fn empty_or_whitespace_values_use_the_placeholder_without_trimming_it() {
        for value in ["", " \t\n "] {
            let attrs = content(&CFString::new(value).as_CFType(), "  placeholder  ");
            assert_eq!(attrs.value.as_deref(), Some("  placeholder  "));
            assert_eq!(attrs.copied_value.unwrap().state_value, value);
        }
    }

    #[test]
    fn non_empty_string_values_win_over_placeholders_without_trimming() {
        let attrs = content(&CFString::new("  value  ").as_CFType(), "placeholder");
        assert_eq!(attrs.value.as_deref(), Some("  value  "));
        assert_eq!(attrs.title.as_deref(), Some("  title  "));
        assert_eq!(attrs.description.as_deref(), Some("  description  "));
        assert_eq!(attrs.identifier.as_deref(), Some("  identifier  "));
        assert_eq!(attrs.help.as_deref(), Some("  help  "));
    }

    #[test]
    fn numeric_and_boolean_values_show_placeholders_but_keep_typed_state() {
        for (value, state) in [
            (CFNumber::from(8.0).as_CFType(), "8"),
            (CFNumber::from(2.5).as_CFType(), "2.5"),
            (CFBoolean::true_value().as_CFType(), "1"),
            (CFBoolean::false_value().as_CFType(), "0"),
        ] {
            let attrs = content(&value, "  placeholder  ");
            assert_eq!(attrs.value.as_deref(), Some("  placeholder  "));
            let copied = attrs.copied_value.unwrap();
            assert_eq!(copied.string_value, None);
            assert_eq!(copied.state_value, state);
        }
    }

    #[test]
    fn empty_and_whitespace_only_placeholders_are_preserved_exactly() {
        for placeholder in ["", " \t\n "] {
            let attrs = content(&CFString::new("").as_CFType(), placeholder);
            assert_eq!(attrs.value.as_deref(), Some(placeholder));
        }
    }

    #[test]
    fn absent_values_use_placeholders_and_absent_placeholders_stay_absent() {
        assert_eq!(
            value_with_placeholder(None, || Some("hint".into())),
            Some("hint".into())
        );
        assert_eq!(value_with_placeholder(None, || None), None);
    }

    #[test]
    fn a_non_empty_value_does_not_request_the_single_read_placeholder() {
        let value = StringishAttrValue {
            string_value: Some("value".into()),
            state_value: "value".into(),
        };
        assert_eq!(
            value_with_placeholder(Some(&value), || panic!("placeholder must stay lazy")),
            Some("value".into())
        );
    }

    #[test]
    fn an_expired_batch_failure_records_timeout_and_refuses_single_replay() {
        let mut budget = WalkBudget::nodes_only(10);
        assert!(!budget.outcome().truncated());
        let mut disabled = false;
        assert!(!replay_after_failure(
            BatchFailureAction::StopForTimeout,
            &mut budget,
            &mut disabled
        ));
        assert_eq!(budget.outcome().reason(), Some("timeout"));
        assert!(!disabled);
    }

    #[test]
    fn a_batch_rejected_after_the_deadline_records_timeout_instead_of_six_single_reads() {
        use super::super::batch_attrs::{batch_failure_action, BatchFailure};
        use super::super::bindings::{
            kAXErrorAttributeUnsupported, kAXErrorIllegalArgument, kAXErrorNotImplemented,
        };
        for error in [
            kAXErrorAttributeUnsupported,
            kAXErrorNotImplemented,
            kAXErrorIllegalArgument,
        ] {
            let mut budget = WalkBudget::nodes_only(10);
            let mut disabled = false;
            let action = batch_failure_action(BatchFailure::WholeCall(error), true, disabled);
            assert!(
                !replay_after_failure(action, &mut budget, &mut disabled),
                "error {error} replayed single reads after the deadline"
            );
            assert_eq!(budget.outcome().reason(), Some("timeout"));
        }
    }

    #[test]
    fn unsupported_batching_changes_only_the_current_walks_flag() {
        let mut budget = WalkBudget::nodes_only(10);
        let mut disabled = false;
        assert!(replay_after_failure(
            BatchFailureAction::DisableAndReplay,
            &mut budget,
            &mut disabled
        ));
        assert!(disabled);
        assert!(!budget.outcome().truncated());
        let mut next_walk_disabled = false;
        assert!(replay_after_failure(
            BatchFailureAction::Replay,
            &mut budget,
            &mut next_walk_disabled
        ));
        assert!(!next_walk_disabled);
    }
}
