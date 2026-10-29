//! The control state and frame of a row, read in one batched AX call.
//!
//! The walker used to ask for each of these with its own round trip (enabled,
//! position, size, value description, min, max, selected). A batch answers
//! them together; any failure replays the single reads, which convert with the
//! same functions so both paths return the same values.

use super::batch_attrs::{
    batch_failure_action, copy_attrs_batch, BatchFailureAction, BatchRead, BatchSlots,
};
use super::bindings::*;
use cua_driver_core::walk_budget::WalkBudget;

const CONTROL_ATTRIBUTES: [&str; 7] = [
    "AXEnabled",
    "AXPosition",
    "AXSize",
    "AXValueDescription",
    "AXMinValue",
    "AXMaxValue",
    "AXSelected",
];

const FRAME_ATTRIBUTES: [&str; 2] = ["AXPosition", "AXSize"];

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ControlAttrs {
    pub enabled: Option<bool>,
    pub frame: Option<[f64; 4]>,
    pub value_description: Option<String>,
    pub min_value: Option<f64>,
    pub max_value: Option<f64>,
    pub selected: Option<bool>,
}

impl ControlAttrs {
    fn from_slots(slots: &BatchSlots) -> Self {
        unsafe {
            Self {
                enabled: slots.raw(0).and_then(|v| bool_from_cf(v)),
                frame: rect_from_parts(
                    slots.raw(1).and_then(|v| point_from_cf(v)),
                    slots.raw(2).and_then(|v| size_from_cf(v)),
                ),
                value_description: slots.string(3),
                min_value: slots.raw(4).and_then(|v| number_from_cf(v)),
                max_value: slots.raw(5).and_then(|v| number_from_cf(v)),
                selected: slots.raw(6).and_then(|v| bool_from_cf(v)),
            }
        }
    }

    unsafe fn from_singles(element: AXUIElementRef) -> Self {
        Self {
            enabled: copy_bool_attr(element, "AXEnabled"),
            frame: element_screen_rect(element),
            value_description: copy_string_attr(element, "AXValueDescription"),
            min_value: copy_number_attr(element, "AXMinValue"),
            max_value: copy_number_attr(element, "AXMaxValue"),
            selected: copy_bool_attr(element, "AXSelected"),
        }
    }
}

/// Whether a failed batch replays single reads. An app that rejects batching
/// outright is not asked to batch again; past the walk's deadline nothing is
/// replayed and the walk stops, as for content attributes.
fn replay_after_batch_failure(
    kind: super::batch_attrs::BatchFailure,
    budget: &mut WalkBudget,
    batching_disabled: &mut bool,
) -> bool {
    match batch_failure_action(kind, budget.expired(), *batching_disabled) {
        BatchFailureAction::DisableAndReplay => *batching_disabled = true,
        BatchFailureAction::Replay => {}
        BatchFailureAction::StopForTimeout => {
            budget.stop_for_timeout();
            return false;
        }
    }
    true
}

/// Single reads run only within the walk's deadline: one that would start, or
/// that ran, past it stops the walk and returns nothing.
fn singles_within_deadline<T>(budget: &mut WalkBudget, read: impl FnOnce() -> T) -> Option<T> {
    if budget.expired() {
        budget.stop_for_timeout();
        return None;
    }
    let value = read();
    if budget.expired() {
        budget.stop_for_timeout();
        return None;
    }
    Some(value)
}

/// A frame read for planning, outside the walk's per-node accounting: one
/// batched call, with the two single reads only when the batch fails.
///
/// # Safety
/// `element` must be a live AX element.
pub(crate) unsafe fn frame_batched(element: AXUIElementRef) -> Option<[f64; 4]> {
    match copy_attrs_batch(element, &FRAME_ATTRIBUTES) {
        BatchRead::Slots(slots) => rect_from_parts(
            slots.raw(0).and_then(|v| point_from_cf(v)),
            slots.raw(1).and_then(|v| size_from_cf(v)),
        ),
        BatchRead::Fallback(_) => element_screen_rect(element),
    }
}

/// The control state and frame of an element that may be indexed, or `None`
/// when its read ran past the walk's deadline (the walk is stopped).
///
/// # Safety
/// `element` must be a live AX element.
pub(crate) unsafe fn read_control_attrs(
    element: AXUIElementRef,
    budget: &mut WalkBudget,
    batching_disabled: &mut bool,
) -> Option<ControlAttrs> {
    if !*batching_disabled {
        match copy_attrs_batch(element, &CONTROL_ATTRIBUTES) {
            BatchRead::Slots(slots) => return Some(ControlAttrs::from_slots(&slots)),
            BatchRead::Fallback(kind) => {
                if !replay_after_batch_failure(kind, budget, batching_disabled) {
                    return None;
                }
            }
        }
    }
    singles_within_deadline(budget, || ControlAttrs::from_singles(element))
}

/// The frame of an element, as [`element_screen_rect`] reads it, or `None` when
/// a failed batch ran past the walk's deadline (the walk is stopped).
///
/// # Safety
/// `element` must be a live AX element.
pub(crate) unsafe fn read_frame(
    element: AXUIElementRef,
    budget: &mut WalkBudget,
    batching_disabled: &mut bool,
) -> Option<Option<[f64; 4]>> {
    if !*batching_disabled {
        match copy_attrs_batch(element, &FRAME_ATTRIBUTES) {
            BatchRead::Slots(slots) => {
                return Some(rect_from_parts(
                    slots.raw(0).and_then(|v| point_from_cf(v)),
                    slots.raw(1).and_then(|v| size_from_cf(v)),
                ))
            }
            BatchRead::Fallback(kind) => {
                if !replay_after_batch_failure(kind, budget, batching_disabled) {
                    return None;
                }
            }
        }
    }
    singles_within_deadline(budget, || element_screen_rect(element))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_foundation::{
        array::CFArray,
        base::{CFRetain, CFType, CFTypeRef, TCFType},
        boolean::CFBoolean,
        number::CFNumber,
        string::CFString,
    };

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFNull: CFTypeRef;
    }

    #[repr(C)]
    struct Pair(f64, f64);

    fn ax_value(kind: AXValueType, pair: Pair) -> CFType {
        unsafe {
            CFType::wrap_under_create_rule(
                AXValueCreate(kind, &pair as *const _ as *const _) as CFTypeRef
            )
        }
    }

    fn slots(values: &[CFType]) -> BatchSlots {
        let array = CFArray::from_CFTypes(values);
        let raw = array.as_CFTypeRef();
        let read = unsafe {
            CFRetain(raw);
            super::super::batch_attrs::finish_batch(kAXErrorSuccess, raw, values.len())
        };
        let BatchRead::Slots(slots) = read else {
            panic!("valid control state must decode");
        };
        slots
    }

    #[test]
    fn single_reads_never_start_past_the_deadline() {
        let mut expired = WalkBudget::new(0, 10);
        let _ = expired.admit();
        let mut read = false;
        assert_eq!(singles_within_deadline(&mut expired, || read = true), None);
        assert!(!read, "no single read is issued past the deadline");
        assert_eq!(expired.outcome().reason(), Some("timeout"));

        let mut live = WalkBudget::nodes_only(10);
        let _ = live.admit();
        assert_eq!(singles_within_deadline(&mut live, || 7), Some(7));
        assert!(!live.outcome().truncated());
    }

    #[test]
    fn a_batch_failing_after_the_deadline_replays_no_single_reads() {
        use super::super::batch_attrs::BatchFailure;
        let mut expired = WalkBudget::new(0, 10);
        let _ = expired.admit();
        assert!(expired.expired());
        let mut disabled = false;
        assert!(!replay_after_batch_failure(
            BatchFailure::Decode,
            &mut expired,
            &mut disabled
        ));
        assert_eq!(expired.outcome().reason(), Some("timeout"));

        let mut live = WalkBudget::nodes_only(10);
        let _ = live.admit();
        assert!(replay_after_batch_failure(
            BatchFailure::Decode,
            &mut live,
            &mut disabled
        ));
        assert!(!disabled, "a decode failure keeps batching on");
        assert!(replay_after_batch_failure(
            BatchFailure::WholeCall(kAXErrorNotImplemented),
            &mut live,
            &mut disabled
        ));
        assert!(disabled, "an app that rejects batching is not asked again");
    }

    #[test]
    fn a_batched_row_reads_what_the_single_reads_would() {
        let null = unsafe { CFType::wrap_under_get_rule(kCFNull) };
        let read = ControlAttrs::from_slots(&slots(&[
            CFBoolean::true_value().as_CFType(),
            ax_value(kAXValueCGPointType, Pair(10.0, 20.0)),
            ax_value(kAXValueCGSizeType, Pair(30.0, 40.0)),
            CFString::new("  반쯤 ✓ ").as_CFType(),
            CFNumber::from(0.0).as_CFType(),
            CFNumber::from(10.5).as_CFType(),
            null,
        ]));
        assert_eq!(
            read,
            ControlAttrs {
                enabled: Some(true),
                frame: Some([10.0, 20.0, 30.0, 40.0]),
                value_description: Some("  반쯤 ✓ ".into()),
                min_value: Some(0.0),
                max_value: Some(10.5),
                selected: None,
            }
        );
    }

    #[test]
    fn batched_values_convert_like_single_reads() {
        // A 0/1 or any nonzero CFNumber is a boolean; a sub-point size has no
        // frame; a value of the wrong type is absent, never misread.
        let read = ControlAttrs::from_slots(&slots(&[
            CFNumber::from(2.0).as_CFType(),
            ax_value(kAXValueCGPointType, Pair(1.0, 2.0)),
            ax_value(kAXValueCGSizeType, Pair(0.5, 40.0)),
            CFNumber::from(3.0).as_CFType(),
            CFString::new("7").as_CFType(),
            CFBoolean::true_value().as_CFType(),
            CFNumber::from(0.0).as_CFType(),
        ]));
        assert_eq!(read.enabled, Some(true));
        assert_eq!(read.frame, None);
        assert_eq!(read.value_description, None);
        assert_eq!(read.min_value, None);
        assert_eq!(read.max_value, None);
        assert_eq!(read.selected, Some(false));
        // A position stored as a size is not a position.
        assert_eq!(
            rect_from_parts(
                unsafe {
                    point_from_cf(ax_value(kAXValueCGSizeType, Pair(1.0, 1.0)).as_CFTypeRef())
                },
                Some((5.0, 5.0)),
            ),
            None
        );
    }
}
