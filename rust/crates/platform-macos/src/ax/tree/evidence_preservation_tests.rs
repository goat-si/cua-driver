use super::walker_seam_tests::{step, Fake, Reply};
use super::walker_test_support::{Batch, Slot, SlotFailure};
use super::*;
use crate::ax::batch_attrs::BatchFailure;
use crate::ax::content_attrs::{AttrStatus, AttributeListing, ContentReader};
use crate::ax::evidence::{EvidenceStatus, ReadEvidence, ReadSite};
use crate::ax::tree_source::ReadFailure;

fn failure() -> ReadFailure {
    ReadFailure::Ax(kAXErrorCannotComplete)
}

fn assert_evidence(evidence: &[ReadEvidence], site: ReadSite, status: EvidenceStatus) {
    assert!(
        evidence
            .iter()
            .any(|entry| entry.site == site && entry.status == status),
        "missing evidence for {site:?}: {status:?}"
    );
}

struct ContentScript {
    attrs: [AttrStatus; 7],
    trace: Vec<&'static str>,
}

impl ContentReader for ContentScript {
    fn batch(&mut self) -> Result<[AttrStatus; 7], BatchFailure> {
        self.trace.push("batch");
        Ok(self.attrs.clone())
    }
    fn string(&mut self, _: &str) -> AttrStatus {
        panic!("successful batch must not replay singles")
    }
    fn value(&mut self) -> Result<Option<StringishAttrValue>, AXError> {
        self.trace.push("value");
        Ok(Some(StringishAttrValue {
            string_value: Some("visible".into()),
            state_value: "visible".into(),
        }))
    }
    fn expired(&self) -> bool {
        false
    }
}

#[test]
fn content_observation_preserves_shadowed_failures() {
    let mut attrs = std::array::from_fn(|_| AttrStatus::Absent);
    attrs[0] = AttrStatus::Known("title".into());
    attrs[2] = AttrStatus::Failed {
        error: kAXErrorFailure,
        listed: AttributeListing::Listed,
    };
    attrs[6] = AttrStatus::Known("text entry".into());
    let mut script = ContentScript {
        attrs,
        trace: vec![],
    };
    let observed = crate::ax::content_attrs::read_with_observed(
        AttrStatus::Known("AXTextArea".into()),
        &mut script,
        &mut false,
    );
    let legacy = observed.legacy.expect("content returned");
    assert_eq!(legacy.title.as_deref(), Some("title"));
    assert_eq!(legacy.value.as_deref(), Some("visible"));
    assert!(!legacy.read_failed);
    assert_eq!(script.trace, ["batch", "value"]);
    assert_evidence(
        &observed.evidence,
        ReadSite::Content("AXDescription"),
        EvidenceStatus::Unknown(ReadFailure::Ax(kAXErrorFailure)),
    );
}

const CONTROL: &str =
    "AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected";

#[test]
fn control_batch_preserves_unknown_and_absent_per_slot() {
    let batch = Batch(vec![
        Slot::Failure(SlotFailure::Ax(kAXErrorCannotComplete)),
        Slot::Point(None),
        Slot::Size(None),
        Slot::Text(AttrStatus::Absent),
        Slot::Failure(SlotFailure::WrongType),
        Slot::Number(None),
        Slot::Bool(Some(false)),
    ]);
    let fake = Fake::new(
        &[(1, 1, 1, &[])],
        vec![step(&format!("batch 1 {CONTROL}"), Reply::Batch(Ok(batch)))],
    );
    let element = fake.handle(1);
    let observed = crate::ax::control_attrs::read_control_attrs_observed_with(
        &fake,
        &element,
        &mut WalkBudget::nodes_only(10),
        &mut false,
    );
    let legacy = observed.legacy.expect("control returned");
    assert_eq!(legacy.enabled, None);
    assert_eq!(legacy.selected, Some(false));
    fake.assert_consumed();
    drop(element);
    fake.assert_lifetimes(&[]);
    assert_evidence(
        &observed.evidence,
        ReadSite::Control("AXEnabled"),
        EvidenceStatus::Unknown(failure()),
    );
}

#[test]
fn control_singles_preserve_unknown_and_absent_per_field() {
    let fake = Fake::new(
        &[(1, 1, 1, &[])],
        vec![
            step(
                &format!("batch 1 {CONTROL}"),
                Reply::Batch(Err(BatchFailure::Decode)),
            ),
            step("bool 1 AXEnabled", Reply::Bool(Read::Unknown(failure()))),
            step("position 1", Reply::Point(Read::Absent)),
            step("text 1 AXValueDescription", Reply::Text(Read::Absent)),
            step(
                "number 1 AXMinValue",
                Reply::Number(Read::Unknown(ReadFailure::WrongType)),
            ),
            step("number 1 AXMaxValue", Reply::Number(Read::Absent)),
            step("bool 1 AXSelected", Reply::Bool(Read::Known(false))),
        ],
    );
    let element = fake.handle(1);
    let observed = crate::ax::control_attrs::read_control_attrs_observed_with(
        &fake,
        &element,
        &mut WalkBudget::nodes_only(10),
        &mut false,
    );
    let legacy = observed.legacy.expect("control returned");
    assert_eq!(legacy.enabled, None);
    assert_eq!(legacy.selected, Some(false));
    fake.assert_consumed();
    drop(element);
    fake.assert_lifetimes(&[]);
    assert_evidence(
        &observed.evidence,
        ReadSite::Control("AXEnabled"),
        EvidenceStatus::Unknown(failure()),
    );
}

#[test]
fn scrollbar_owner_preserves_unknown_and_absent() {
    let fake = Fake::new(
        &[(1, 1, 1, &[])],
        vec![step(
            "element 1 AXVerticalScrollBar",
            Reply::Element(Read::Unknown(failure())),
        )],
    );
    let element = fake.handle(1);
    let observed = crate::ax::scroll_area::bar_position_observed_with(&fake, &element, "down");
    assert_eq!(observed.legacy, None);
    fake.assert_consumed();
    drop(element);
    fake.assert_lifetimes(&[]);
    assert_evidence(
        &observed.evidence,
        ReadSite::Scrollbar("AXVerticalScrollBar"),
        EvidenceStatus::Unknown(failure()),
    );
}

#[test]
fn tab_parent_metadata_preserves_unknown_and_absent() {
    let fake = Fake::new(
        &[(1, 1, 1, &[])],
        vec![
            step("text 1 AXTitle", Reply::Text(Read::Unknown(failure()))),
            step("text 1 AXDescription", Reply::Text(Read::Absent)),
            step("text 1 AXRoleDescription", Reply::Text(Read::Absent)),
        ],
    );
    let element = fake.handle(1);
    let observed = plan_macos_tab_strip_observed(
        &fake,
        &element,
        "AXGroup",
        &[],
        &mut WalkBudget::nodes_only(10),
        &mut false,
    );
    assert!(observed.legacy.is_none());
    fake.assert_consumed();
    drop(element);
    fake.assert_lifetimes(&[]);
    assert_evidence(
        &observed.evidence,
        ReadSite::Tab("AXTitle"),
        EvidenceStatus::Unknown(failure()),
    );
}

#[test]
fn dense_visibility_sets_preserve_unknown_and_absent() {
    let fake = Fake::new(
        &[(1, 1, 1, &[])],
        vec![
            step("set 1 AXRows", Reply::Set(Read::Absent)),
            step("set 1 AXVisibleRows", Reply::Set(Read::Unknown(failure()))),
            step("set 1 AXVisibleChildren", Reply::Set(Read::Absent)),
        ],
    );
    let element = fake.handle(1);
    let observed = dense_rows::plan_collection_children_observed(
        &fake,
        &element,
        &element,
        "AXTable",
        &[],
        &WalkBudget::nodes_only(10),
    );
    assert!(observed.legacy.is_empty());
    fake.assert_consumed();
    drop(element);
    fake.assert_lifetimes(&[]);
    assert_evidence(
        &observed.evidence,
        ReadSite::Dense("AXVisibleRows"),
        EvidenceStatus::Unknown(failure()),
    );
}

#[test]
fn malformed_children_preserve_valid_items_and_decode_evidence() {
    let observed =
        crate::ax::tree_source::native::decode_child_members([Some(11), None, Some(33)], |item| {
            item
        });
    assert_eq!(observed.legacy, [11, 33]);
    assert_evidence(
        &observed.evidence,
        ReadSite::Children,
        EvidenceStatus::Unknown(ReadFailure::WrongType),
    );
}
