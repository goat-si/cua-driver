//! Borrowed batch slots stay valid until the single owning array is dropped.

use super::bindings::*;
use core_foundation::{
    array::CFArray,
    base::{CFGetTypeID, CFType, CFTypeRef, TCFType},
    string::CFString,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BatchFailure {
    WholeCall(AXError),
    Decode,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BatchFailureAction {
    DisableAndReplay,
    Replay,
    StopForTimeout,
}

pub(crate) fn batch_failure_action(
    kind: BatchFailure,
    budget_expired: bool,
    batching_disabled: bool,
) -> BatchFailureAction {
    if batching_disabled {
        return BatchFailureAction::Replay;
    }
    // Past the walk deadline nothing is replayed, whatever the failure: an app
    // that rejects batching would otherwise get six more blocking reads.
    if budget_expired {
        return BatchFailureAction::StopForTimeout;
    }
    match kind {
        BatchFailure::WholeCall(error)
            if [
                kAXErrorAttributeUnsupported,
                kAXErrorNotImplemented,
                kAXErrorIllegalArgument,
            ]
            .contains(&error) =>
        {
            BatchFailureAction::DisableAndReplay
        }
        _ => BatchFailureAction::Replay,
    }
}

pub(crate) enum BatchRead {
    Slots(BatchSlots),
    Fallback(BatchFailure),
}

#[derive(Debug, PartialEq, Eq)]
enum Slot {
    Value(CFTypeRef),
    Absent,
}

pub(crate) struct BatchSlots {
    // Slot pointers never escape this owner; no per-slot retain is needed.
    _array: CFType,
    slots: Vec<Slot>,
}

impl BatchSlots {
    pub(crate) fn string(&self, index: usize) -> Option<String> {
        match self.slots[index] {
            Slot::Value(value) => unsafe { string_from_cf(value) },
            Slot::Absent => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn stringish(&self, index: usize) -> Option<StringishAttrValue> {
        match self.slots[index] {
            Slot::Value(value) => unsafe { stringish_from_cf(value) },
            Slot::Absent => None,
        }
    }

    /// The slot's borrowed value, valid while these slots live.
    pub(crate) fn raw(&self, index: usize) -> Option<CFTypeRef> {
        match self.slots[index] {
            Slot::Value(value) => Some(value),
            Slot::Absent => None,
        }
    }
}

fn error_slot(error: Option<AXError>) -> Option<Slot> {
    match error {
        Some(error) if [kAXErrorNoValue, kAXErrorAttributeUnsupported].contains(&error) => {
            Some(Slot::Absent)
        }
        _ => None,
    }
}

// Pure decoding: only local CF objects are inspected, never the target app.
unsafe fn decode_slots(value: CFTypeRef, expected: usize) -> Option<Vec<Slot>> {
    decode_slots_with(value, expected, |value| {
        let mut error = kAXErrorFailure;
        AXValueGetValue(value, kAXValueAXErrorType, &mut error as *mut _ as *mut _).then_some(error)
    })
}

unsafe fn decode_slots_with(
    value: CFTypeRef,
    expected: usize,
    extract_error: impl Fn(AXValueRef) -> Option<AXError>,
) -> Option<Vec<Slot>> {
    use core_foundation::array::{CFArrayGetCount, CFArrayGetValueAtIndex};
    if value.is_null() || CFGetTypeID(value) != CFArray::<CFTypeRef>::type_id() {
        return None;
    }
    let array = value as core_foundation::array::CFArrayRef;
    if CFArrayGetCount(array) != expected as isize {
        return None;
    }
    (0..expected)
        .map(|index| {
            let value = CFArrayGetValueAtIndex(array, index as isize);
            if value.is_null() {
                return None;
            }
            if CFGetTypeID(value) == CFNullGetTypeID() {
                return Some(Slot::Absent);
            }
            if CFGetTypeID(value) == AXValueGetTypeID()
                && AXValueGetType(value as AXValueRef) == kAXValueAXErrorType
            {
                return error_slot(extract_error(value as AXValueRef));
            }
            Some(Slot::Value(value))
        })
        .collect()
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFNullGetTypeID() -> core_foundation::base::CFTypeID;
}

// Own even malformed or error-associated output, so all exits release it once.
pub(super) unsafe fn finish_batch(error: AXError, value: CFTypeRef, expected: usize) -> BatchRead {
    let owner = (!value.is_null()).then(|| CFType::wrap_under_create_rule(value));
    if error != kAXErrorSuccess {
        return BatchRead::Fallback(BatchFailure::WholeCall(error));
    }
    let Some(array) = owner else {
        return BatchRead::Fallback(BatchFailure::Decode);
    };
    match decode_slots(array.as_CFTypeRef(), expected) {
        Some(slots) => BatchRead::Slots(BatchSlots {
            _array: array,
            slots,
        }),
        None => BatchRead::Fallback(BatchFailure::Decode),
    }
}

/// `element` must be a live AX element; the result owns any copied array.
pub(crate) unsafe fn copy_attrs_batch(element: AXUIElementRef, names: &[&str]) -> BatchRead {
    let names: Vec<_> = names.iter().map(|name| CFString::new(name)).collect();
    let attributes = CFArray::from_CFTypes(&names);
    let mut values = std::ptr::null();
    let error = AXUIElementCopyMultipleAttributeValues(
        element,
        attributes.as_concrete_TypeRef(),
        0,
        &mut values,
    );
    finish_batch(error, values as CFTypeRef, names.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_foundation::{
        base::{CFGetRetainCount, CFRetain},
        boolean::CFBoolean,
        number::CFNumber,
    };

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFNull: CFTypeRef;
    }

    fn ax_error(error: AXError) -> CFType {
        unsafe {
            CFType::wrap_under_create_rule(AXValueCreate(
                kAXValueAXErrorType,
                &error as *const _ as *const _,
            ) as CFTypeRef)
        }
    }

    fn finish(values: &[CFType]) -> BatchRead {
        let array = CFArray::from_CFTypes(values);
        let raw = array.as_CFTypeRef();
        unsafe {
            CFRetain(raw);
            finish_batch(kAXErrorSuccess, raw, values.len())
        }
    }

    #[test]
    fn batch_decoder_keeps_strings_numbers_and_booleans() {
        let text = CFString::new("  title  ");
        let number = CFNumber::from(8.0);
        let boolean = CFBoolean::true_value();
        let BatchRead::Slots(slots) =
            finish(&[text.as_CFType(), number.as_CFType(), boolean.as_CFType()])
        else {
            panic!("valid content must decode");
        };
        assert_eq!(slots.string(0).as_deref(), Some("  title  "));
        assert_eq!(slots.stringish(1).unwrap().state_value, "8");
        assert_eq!(slots.stringish(2).unwrap().state_value, "1");
        assert_eq!(slots.string(1), None);
        assert_eq!(slots.string(2), None);
    }

    #[test]
    fn only_null_no_value_and_unsupported_slots_are_absent() {
        let null = unsafe { CFType::wrap_under_get_rule(kCFNull) };
        let BatchRead::Slots(slots) = finish(&[
            null,
            ax_error(kAXErrorNoValue),
            ax_error(kAXErrorAttributeUnsupported),
        ]) else {
            panic!("absence allowlist must decode");
        };
        assert_eq!(slots.slots, vec![Slot::Absent, Slot::Absent, Slot::Absent]);
    }

    #[test]
    fn every_other_error_slot_requires_single_read_fallback() {
        for error in [
            kAXErrorFailure,
            kAXErrorIllegalArgument,
            kAXErrorCannotComplete,
            kAXErrorNotImplemented,
            kAXErrorAPIDisabled,
            kAXErrorInvalidUIElement,
            kAXErrorSuccess,
            -98765,
        ] {
            assert!(
                matches!(
                    finish(&[ax_error(error)]),
                    BatchRead::Fallback(BatchFailure::Decode)
                ),
                "error {error} must not masquerade as absent"
            );
        }
    }

    #[test]
    fn an_undecodable_error_slot_requires_fallback() {
        // A valid local AXValue always extracts successfully. Inject failure at
        // that boundary to cover the array decoder without corrupting a CF object.
        let array = CFArray::from_CFTypes(&[
            CFString::new("decoded before extraction fails").as_CFType(),
            ax_error(kAXErrorNoValue),
        ]);
        assert!(unsafe { decode_slots_with(array.as_CFTypeRef(), 2, |_| None) }.is_none());
    }

    #[test]
    fn malformed_batch_results_require_fallback() {
        let array = CFArray::from_CFTypes(&[CFString::new("one")]);
        let text = CFString::new("not an array");
        unsafe {
            assert!(decode_slots(array.as_CFTypeRef(), 2).is_none());
            assert!(decode_slots(text.as_CFTypeRef(), 1).is_none());
            assert!(decode_slots(std::ptr::null(), 1).is_none());
        }
    }

    #[test]
    fn non_error_ax_values_reach_the_converters_without_becoming_absent() {
        let point = [1.0_f64, 2.0];
        let value = unsafe {
            CFType::wrap_under_create_rule(AXValueCreate(
                kAXValueCGPointType,
                point.as_ptr() as *const _,
            ) as CFTypeRef)
        };
        let BatchRead::Slots(slots) = finish(&[value]) else {
            panic!("a non-error AXValue is a value slot");
        };
        assert!(matches!(slots.slots[0], Slot::Value(_)));
        assert_eq!(slots.string(0), None);
        assert_eq!(slots.stringish(0), None);
    }

    #[test]
    fn success_borrows_slots_and_releases_the_array_exactly_once() {
        let text = CFString::new("batch-success-retain-balance-long-string");
        let raw = text.as_CFTypeRef();
        let count = unsafe { CFGetRetainCount(raw) };
        let array = CFArray::from_CFTypes(&[text.as_CFType()]);
        let array_raw = array.as_CFTypeRef();
        let array_count = unsafe { CFGetRetainCount(array_raw) };
        let result = unsafe {
            CFRetain(array_raw);
            finish_batch(kAXErrorSuccess, array_raw, 1)
        };
        assert_eq!(unsafe { CFGetRetainCount(raw) }, count + 1);
        let BatchRead::Slots(slots) = result else {
            panic!("must decode")
        };
        assert_eq!(
            slots.string(0).as_deref(),
            Some("batch-success-retain-balance-long-string")
        );
        assert_eq!(unsafe { CFGetRetainCount(raw) }, count + 1);
        assert_eq!(unsafe { CFGetRetainCount(array_raw) }, array_count + 1);
        drop(slots);
        assert_eq!(unsafe { CFGetRetainCount(array_raw) }, array_count);
        drop(array);
        assert_eq!(unsafe { CFGetRetainCount(raw) }, count);
    }

    #[test]
    fn partial_decode_fallback_releases_the_array_without_retaining_earlier_slots() {
        let text = CFString::new("batch-partial-fallback-retain-balance-long-string");
        let raw = text.as_CFTypeRef();
        let count = unsafe { CFGetRetainCount(raw) };
        let array = CFArray::from_CFTypes(&[text.as_CFType(), ax_error(kAXErrorCannotComplete)]);
        let array_raw = array.as_CFTypeRef();
        let array_count = unsafe { CFGetRetainCount(array_raw) };
        let result = unsafe {
            CFRetain(array_raw);
            finish_batch(kAXErrorSuccess, array_raw, 2)
        };
        assert!(matches!(result, BatchRead::Fallback(BatchFailure::Decode)));
        assert_eq!(unsafe { CFGetRetainCount(raw) }, count + 1);
        assert_eq!(unsafe { CFGetRetainCount(array_raw) }, array_count);
        drop(array);
        assert_eq!(unsafe { CFGetRetainCount(raw) }, count);
    }

    #[test]
    fn whole_call_errors_release_any_returned_object() {
        let text = CFString::new("batch-error-output-retain-balance-long-string");
        let raw = text.as_CFTypeRef();
        let count = unsafe { CFGetRetainCount(raw) };
        let result = unsafe {
            CFRetain(raw);
            finish_batch(kAXErrorCannotComplete, raw, 1)
        };
        assert!(matches!(
            result,
            BatchRead::Fallback(BatchFailure::WholeCall(error)) if error == kAXErrorCannotComplete
        ));
        assert_eq!(unsafe { CFGetRetainCount(raw) }, count);
    }

    #[test]
    fn unsupported_whole_calls_disable_batching_and_replay_until_the_deadline() {
        for error in [
            kAXErrorAttributeUnsupported,
            kAXErrorNotImplemented,
            kAXErrorIllegalArgument,
        ] {
            assert_eq!(
                batch_failure_action(BatchFailure::WholeCall(error), false, false),
                BatchFailureAction::DisableAndReplay
            );
            assert_eq!(
                batch_failure_action(BatchFailure::WholeCall(error), true, false),
                BatchFailureAction::StopForTimeout
            );
        }
    }

    #[test]
    fn expired_transient_or_decode_failures_stop_without_replay() {
        for kind in [
            BatchFailure::Decode,
            BatchFailure::WholeCall(kAXErrorCannotComplete),
            BatchFailure::WholeCall(kAXErrorFailure),
        ] {
            assert_eq!(
                batch_failure_action(kind, true, false),
                BatchFailureAction::StopForTimeout
            );
        }
    }

    #[test]
    fn failures_with_budget_remaining_replay_singles() {
        for kind in [
            BatchFailure::Decode,
            BatchFailure::WholeCall(kAXErrorCannotComplete),
            BatchFailure::WholeCall(-98765),
        ] {
            assert_eq!(
                batch_failure_action(kind, false, false),
                BatchFailureAction::Replay
            );
        }
    }

    #[test]
    fn disabled_batching_uses_singles_for_the_rest_of_the_walk() {
        for expired in [false, true] {
            assert_eq!(
                batch_failure_action(
                    BatchFailure::WholeCall(kAXErrorNotImplemented),
                    expired,
                    true
                ),
                BatchFailureAction::Replay
            );
        }
    }
}
