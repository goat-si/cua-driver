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

    /// The single reads, each started only before the deadline. `None` when
    /// one would start past it.
    unsafe fn from_singles(element: AXUIElementRef, step: &Stepper) -> Option<Self> {
        Some(Self {
            enabled: step.read(|| copy_bool_attr(element, "AXEnabled"))?,
            frame: frame_from_singles(element, step)?,
            value_description: step.read(|| copy_string_attr(element, "AXValueDescription"))?,
            min_value: step.read(|| copy_number_attr(element, "AXMinValue"))?,
            max_value: step.read(|| copy_number_attr(element, "AXMaxValue"))?,
            selected: step.read(|| copy_bool_attr(element, "AXSelected"))?,
        })
    }
}

fn frame_from_slots(slots: &BatchSlots) -> Option<[f64; 4]> {
    unsafe {
        rect_from_parts(
            slots.raw(0).and_then(|v| point_from_cf(v)),
            slots.raw(1).and_then(|v| size_from_cf(v)),
        )
    }
}

/// Starts one AX read after another, none past the walk's deadline.
pub(crate) struct Stepper<'a> {
    expired: &'a dyn Fn() -> bool,
}

impl Stepper<'_> {
    /// Run `read` unless the deadline has passed.
    pub(crate) fn read<T>(&self, read: impl FnOnce() -> T) -> Option<T> {
        (!(self.expired)()).then(read)
    }
}

/// [`element_screen_rect`]'s two reads, each started only before the
/// deadline: `Some(frame)` as it reads, `None` when a read would start past it.
unsafe fn frame_from_singles(element: AXUIElementRef, step: &Stepper) -> Option<Option<[f64; 4]>> {
    let Some(position) = step.read(|| copy_position_attr(element))? else {
        return Some(None);
    };
    let size = step.read(|| copy_size_attr(element))?;
    Some(rect_from_parts(Some(position), size))
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

/// One batched read, or its single reads, within the walk's deadline. No read
/// starts past it, and a read that ends past it is discarded: the walk stops
/// and the caller keeps the row's role only.
fn read_within_deadline<T>(
    budget: &mut WalkBudget,
    batching_disabled: &mut bool,
    batch: impl FnOnce() -> BatchRead,
    from_slots: impl FnOnce(&BatchSlots) -> T,
    singles: impl FnOnce(&Stepper) -> Option<T>,
) -> Option<T> {
    let read = 'read: {
        if !*batching_disabled {
            let batched = {
                let budget = &*budget;
                let expired = || budget.expired();
                Stepper { expired: &expired }.read(batch)
            };
            match batched {
                None => break 'read None,
                Some(BatchRead::Slots(slots)) => break 'read Some(from_slots(&slots)),
                Some(BatchRead::Fallback(kind)) => {
                    if !replay_after_batch_failure(kind, budget, batching_disabled) {
                        return None;
                    }
                }
            }
        }
        let budget = &*budget;
        let expired = || budget.expired();
        singles(&Stepper { expired: &expired })
    };
    if read.is_none() || budget.expired() {
        budget.stop_for_timeout();
        return None;
    }
    read
}

/// A frame read for planning, outside the walk's per-node accounting: one
/// batched call, with the two single reads only when the batch fails. `None`
/// when a read would start, or ended, past the deadline.
///
/// # Safety
/// `element` must be a live AX element.
pub(crate) unsafe fn frame_batched(
    element: AXUIElementRef,
    budget: &WalkBudget,
) -> Option<Option<[f64; 4]>> {
    let expired = || budget.expired();
    let step = Stepper { expired: &expired };
    let frame = match step.read(|| copy_attrs_batch(element, &FRAME_ATTRIBUTES))? {
        BatchRead::Slots(slots) => Some(frame_from_slots(&slots)),
        BatchRead::Fallback(_) => frame_from_singles(element, &step),
    }?;
    step.read(|| frame)
}

/// The control state and frame of an element that may be indexed, or `None`
/// when a read would start, or ended, past the walk's deadline (the walk is
/// stopped).
///
/// # Safety
/// `element` must be a live AX element.
pub(crate) unsafe fn read_control_attrs(
    element: AXUIElementRef,
    budget: &mut WalkBudget,
    batching_disabled: &mut bool,
) -> Option<ControlAttrs> {
    read_within_deadline(
        budget,
        batching_disabled,
        || copy_attrs_batch(element, &CONTROL_ATTRIBUTES),
        ControlAttrs::from_slots,
        |step| ControlAttrs::from_singles(element, step),
    )
}

/// The frame of an element, as [`element_screen_rect`] reads it, or `None` when
/// a read would start, or ended, past the walk's deadline (the walk is stopped).
///
/// # Safety
/// `element` must be a live AX element.
pub(crate) unsafe fn read_frame(
    element: AXUIElementRef,
    budget: &mut WalkBudget,
    batching_disabled: &mut bool,
) -> Option<Option<[f64; 4]>> {
    read_within_deadline(
        budget,
        batching_disabled,
        || copy_attrs_batch(element, &FRAME_ATTRIBUTES),
        frame_from_slots,
        |step| frame_from_singles(element, step),
    )
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
    fn no_read_starts_past_the_deadline_and_a_late_read_is_discarded() {
        use super::super::batch_attrs::BatchFailure;
        let never = || -> BatchRead { panic!("no batch past the deadline") };
        let mut expired = WalkBudget::new(0, 10);
        let _ = expired.admit();
        let mut disabled = false;
        let read = read_within_deadline(
            &mut expired,
            &mut disabled,
            never,
            |_| 1,
            |_| panic!("no single read past the deadline"),
        );
        assert_eq!(read, None);
        assert_eq!(expired.outcome().reason(), Some("timeout"));

        // Batching off: the stepper refuses every single read.
        let mut disabled = true;
        let read = read_within_deadline(
            &mut expired,
            &mut disabled,
            never,
            |_| 1,
            |step| step.read(|| panic!("no single read past the deadline")),
        );
        assert_eq!(read, None);

        // Within the deadline the single reads run and their value is kept.
        let mut live = WalkBudget::nodes_only(10);
        let _ = live.admit();
        let read = read_within_deadline(
            &mut live,
            &mut disabled,
            never,
            |_| 1,
            |step| step.read(|| 7),
        );
        assert_eq!(read, Some(7));
        assert!(!live.outcome().truncated());

        // A rejected batch replays the single reads within the deadline.
        let mut disabled = false;
        let read = read_within_deadline(
            &mut live,
            &mut disabled,
            || BatchRead::Fallback(BatchFailure::Decode),
            |_| 1,
            |step| step.read(|| 9),
        );
        assert_eq!(read, Some(9));
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
