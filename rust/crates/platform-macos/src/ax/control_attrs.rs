//! The control state and frame of a row, read in one batched AX call.
//!
//! The walker used to ask for each of these with its own round trip (enabled,
//! position, size, value description, min, max, selected). A batch answers
//! them together; any failure replays the single reads, which convert with the
//! same functions so both paths return the same values.

use super::batch_attrs::{batch_failure_action, BatchFailure, BatchFailureAction};
use super::bindings::*;
use super::tree_source::{AxTreeSource, Read, TreeBatch};
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

fn known<T>(read: Read<T>) -> Option<T> {
    match read {
        Read::Known(value) => Some(value),
        Read::Absent | Read::Unknown(_) => None,
    }
}

fn batch_text(batch: &impl TreeBatch, index: usize) -> Option<String> {
    match batch.text(index) {
        super::content_attrs::AttrStatus::Known(value) => Some(value),
        _ => None,
    }
}

impl ControlAttrs {
    fn from_slots(slots: &impl TreeBatch) -> Self {
        Self {
            enabled: known(slots.boolean(0)),
            frame: rect_from_parts(known(slots.position(1)), known(slots.size(2))),
            value_description: batch_text(slots, 3),
            min_value: known(slots.number(4)),
            max_value: known(slots.number(5)),
            selected: known(slots.boolean(6)),
        }
    }

    /// The single reads, each started only before the deadline.
    fn from_singles<S: AxTreeSource>(
        source: &S,
        element: &S::Element,
        step: &Stepper,
    ) -> Option<Self> {
        Some(Self {
            enabled: step.read(|| known(source.boolean(element, "AXEnabled")))?,
            frame: frame_from_singles(source, element, step)?,
            value_description: step.read(|| known(source.text(element, "AXValueDescription")))?,
            min_value: step.read(|| known(source.number(element, "AXMinValue")))?,
            max_value: step.read(|| known(source.number(element, "AXMaxValue")))?,
            selected: step.read(|| known(source.boolean(element, "AXSelected")))?,
        })
    }
}

fn frame_from_slots(slots: &impl TreeBatch) -> Option<[f64; 4]> {
    rect_from_parts(known(slots.position(0)), known(slots.size(1)))
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
fn frame_from_singles<S: AxTreeSource>(
    source: &S,
    element: &S::Element,
    step: &Stepper,
) -> Option<Option<[f64; 4]>> {
    let Some(position) = step.read(|| known(source.position(element)))? else {
        return Some(None);
    };
    let size = step.read(|| known(source.size(element)))?;
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
fn read_within_deadline<T, B>(
    budget: &mut WalkBudget,
    batching_disabled: &mut bool,
    batch: impl FnOnce() -> Result<B, BatchFailure>,
    from_slots: impl FnOnce(&B) -> T,
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
                Some(Ok(slots)) => break 'read Some(from_slots(&slots)),
                Some(Err(kind)) => {
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
pub(crate) fn frame_batched_with<S: AxTreeSource>(
    source: &S,
    element: &S::Element,
    budget: &WalkBudget,
) -> Option<Option<[f64; 4]>> {
    let expired = || budget.expired();
    let step = Stepper { expired: &expired };
    let frame = match step.read(|| source.batch(element, &FRAME_ATTRIBUTES))? {
        Ok(slots) => Some(frame_from_slots(&slots)),
        Err(_) => frame_from_singles(source, element, &step),
    }?;
    step.read(|| frame)
}

/// The control state and frame of an element that may be indexed.
pub(crate) fn read_control_attrs_with<S: AxTreeSource>(
    source: &S,
    element: &S::Element,
    budget: &mut WalkBudget,
    batching_disabled: &mut bool,
) -> Option<ControlAttrs> {
    read_within_deadline(
        budget,
        batching_disabled,
        || source.batch(element, &CONTROL_ATTRIBUTES),
        ControlAttrs::from_slots,
        |step| ControlAttrs::from_singles(source, element, step),
    )
}

/// The frame of an element as `element_screen_rect` reads it.
pub(crate) fn read_frame_with<S: AxTreeSource>(
    source: &S,
    element: &S::Element,
    budget: &mut WalkBudget,
    batching_disabled: &mut bool,
) -> Option<Option<[f64; 4]>> {
    read_within_deadline(
        budget,
        batching_disabled,
        || source.batch(element, &FRAME_ATTRIBUTES),
        frame_from_slots,
        |step| frame_from_singles(source, element, step),
    )
}

#[cfg(test)]
mod tests {
    use super::super::batch_attrs::BatchRead;
    use super::super::tree_source::NativeBatch;
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

    fn slots(values: &[CFType]) -> NativeBatch {
        let array = CFArray::from_CFTypes(values);
        let raw = array.as_CFTypeRef();
        let read = unsafe {
            CFRetain(raw);
            super::super::batch_attrs::finish_batch(kAXErrorSuccess, raw, values.len())
        };
        let BatchRead::Slots(slots) = read else {
            panic!("valid control state must decode");
        };
        NativeBatch(slots)
    }

    #[test]
    fn no_read_starts_past_the_deadline_and_a_late_read_is_discarded() {
        use super::super::batch_attrs::BatchFailure;
        let never =
            || -> Result<NativeBatch, BatchFailure> { panic!("no batch past the deadline") };
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
            || Err::<NativeBatch, _>(BatchFailure::Decode),
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

#[cfg(test)]
mod source_adapter_tests {
    use super::*;
    use crate::ax::batch_attrs::BatchFailure;
    use crate::ax::content_attrs::AttrStatus;
    use crate::ax::tree_source::NativeBatch;
    use crate::ax::write_outcome::Settable;
    use std::cell::RefCell;

    struct Source {
        trace: RefCell<Vec<String>>,
        position_present: bool,
    }
    impl AxTreeSource for Source {
        type Element = ();
        type Batch = NativeBatch;
        type ElementSet = ();
        fn key(&self, _: &()) -> usize {
            panic!("unexpected key")
        }
        fn set_messaging_timeout(&self, _: &()) {
            panic!("unexpected timeout")
        }
        fn set_optional_timeout(&self, _: &(), _: f32) {
            panic!("unexpected timeout")
        }
        fn text(&self, _: &(), name: &str) -> Read<String> {
            self.trace.borrow_mut().push(name.into());
            Read::Absent
        }
        fn text_status(&self, _: &(), _: &str) -> AttrStatus {
            panic!("unexpected status")
        }
        fn boolean(&self, _: &(), name: &str) -> Read<bool> {
            self.trace.borrow_mut().push(name.into());
            Read::Known(true)
        }
        fn number(&self, _: &(), name: &str) -> Read<f64> {
            self.trace.borrow_mut().push(name.into());
            Read::Absent
        }
        fn position(&self, _: &()) -> Read<(f64, f64)> {
            self.trace.borrow_mut().push("AXPosition".into());
            if self.position_present {
                Read::Known((10.0, 20.0))
            } else {
                Read::Absent
            }
        }
        fn size(&self, _: &()) -> Read<(f64, f64)> {
            self.trace.borrow_mut().push("AXSize".into());
            Read::Known((30.0, 40.0))
        }
        fn value(&self, _: &()) -> Result<Option<StringishAttrValue>, AXError> {
            panic!("unexpected value")
        }
        fn batch(&self, _: &(), names: &[&str]) -> Result<NativeBatch, BatchFailure> {
            self.trace
                .borrow_mut()
                .push(format!("batch:{}", names.join(",")));
            Err(BatchFailure::Decode)
        }
        fn children(&self, _: &()) -> Read<Vec<()>> {
            panic!("unexpected children")
        }
        fn element_attribute(&self, _: &(), _: &str) -> Read<()> {
            panic!("unexpected element")
        }
        fn element_set(&self, _: &(), _: &str) -> Read<()> {
            panic!("unexpected set")
        }
        fn set_contains(&self, _: &(), _: &()) -> bool {
            panic!("unexpected membership")
        }
        fn action_names(&self, _: &()) -> Read<Vec<String>> {
            panic!("unexpected actions")
        }
        fn value_settable(&self, _: &()) -> Settable {
            panic!("unexpected writable")
        }
        fn selected_settable(&self, _: &()) -> Settable {
            panic!("unexpected selected")
        }
        fn frame(&self, _: &()) -> Read<[f64; 4]> {
            panic!("unexpected frame")
        }
        fn retain_for_snapshot(&self, _: &()) -> usize {
            panic!("unexpected retain")
        }
    }

    #[test]
    fn failed_control_batch_replays_singles_in_order() {
        let source = Source {
            trace: RefCell::new(Vec::new()),
            position_present: true,
        };
        let mut budget = WalkBudget::nodes_only(10);
        let _ = budget.admit();
        let mut disabled = false;
        let result = read_control_attrs_with(&source, &(), &mut budget, &mut disabled).unwrap();
        assert_eq!(result.frame, Some([10.0, 20.0, 30.0, 40.0]));
        assert_eq!(
            source.trace.into_inner(),
            [
                format!("batch:{}", CONTROL_ATTRIBUTES.join(",")),
                "AXEnabled".into(),
                "AXPosition".into(),
                "AXSize".into(),
                "AXValueDescription".into(),
                "AXMinValue".into(),
                "AXMaxValue".into(),
                "AXSelected".into()
            ]
        );
        assert!(!disabled);
    }

    #[test]
    fn absent_position_skips_size_in_frame_replay() {
        let source = Source {
            trace: RefCell::new(Vec::new()),
            position_present: false,
        };
        let mut budget = WalkBudget::nodes_only(10);
        let _ = budget.admit();
        let mut disabled = false;
        assert_eq!(
            read_frame_with(&source, &(), &mut budget, &mut disabled),
            Some(None)
        );
        assert_eq!(
            source.trace.into_inner(),
            [
                format!("batch:{}", FRAME_ATTRIBUTES.join(",")),
                "AXPosition".into()
            ]
        );
    }
}
