use super::walker_seam_tests::{run_fake_observed, step, Fake, Reply};
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

#[derive(Clone, Copy)]
enum Case {
    Known,
    Absent,
    AxFailure,
    WrongType,
}

const CASES: [Case; 4] = [Case::Known, Case::Absent, Case::AxFailure, Case::WrongType];
const CONTROL_NAMES: [&str; 7] = [
    "AXEnabled",
    "AXPosition",
    "AXSize",
    "AXValueDescription",
    "AXMinValue",
    "AXMaxValue",
    "AXSelected",
];

impl Case {
    fn status(self) -> EvidenceStatus {
        match self {
            Self::Known => EvidenceStatus::Known,
            Self::Absent => EvidenceStatus::Absent,
            Self::AxFailure => EvidenceStatus::Unknown(failure()),
            Self::WrongType => EvidenceStatus::Unknown(ReadFailure::WrongType),
        }
    }
    fn read<T>(self, known: T) -> Read<T> {
        match self {
            Self::Known => Read::Known(known),
            Self::Absent => Read::Absent,
            Self::AxFailure => Read::Unknown(failure()),
            Self::WrongType => Read::Unknown(ReadFailure::WrongType),
        }
    }
    fn text(self) -> AttrStatus {
        match self {
            Self::Known => AttrStatus::Known("label".into()),
            Self::Absent => AttrStatus::Absent,
            Self::AxFailure => AttrStatus::Failed {
                error: kAXErrorCannotComplete,
                listed: AttributeListing::ListUnreadable,
            },
            Self::WrongType => AttrStatus::WrongType,
        }
    }
    fn slot(self, index: usize) -> Slot {
        if index == 3 {
            return Slot::Text(self.text());
        }
        match self {
            Self::AxFailure => Slot::Failure(SlotFailure::Ax(kAXErrorCannotComplete)),
            Self::WrongType => Slot::Failure(SlotFailure::WrongType),
            Self::Known | Self::Absent => {
                let present = matches!(self, Self::Known);
                match index {
                    0 | 6 => Slot::Bool(present.then_some(true)),
                    1 => Slot::Point(present.then_some((1.0, 2.0))),
                    2 => Slot::Size(present.then_some((3.0, 4.0))),
                    4 | 5 => Slot::Number(present.then_some(3.0)),
                    _ => unreachable!(),
                }
            }
        }
    }
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

struct ValueScript {
    value: Option<Result<Option<StringishAttrValue>, AXError>>,
    expire_after_value: bool,
    expired: std::cell::Cell<bool>,
    trace: Vec<&'static str>,
}

impl ContentReader for ValueScript {
    fn batch(&mut self) -> Result<[AttrStatus; 7], BatchFailure> {
        self.trace.push("batch");
        let mut attrs = std::array::from_fn(|_| AttrStatus::Absent);
        attrs[0] = AttrStatus::Known("label".into());
        Ok(attrs)
    }
    fn string(&mut self, _: &str) -> AttrStatus {
        panic!("unexpected single read")
    }
    fn value(&mut self) -> Result<Option<StringishAttrValue>, AXError> {
        self.trace.push("value");
        if self.expire_after_value {
            self.expired.set(true);
        }
        self.value.take().expect("value read at most once")
    }
    fn expired(&self) -> bool {
        self.expired.get()
    }
}

#[test]
fn content_value_preserves_failure_and_absence() {
    for (value, expected) in [
        (
            Ok(Some(StringishAttrValue {
                string_value: Some("visible".into()),
                state_value: "visible".into(),
            })),
            EvidenceStatus::Known,
        ),
        (Ok(None), EvidenceStatus::Absent),
        (
            Err(kAXErrorCannotComplete),
            EvidenceStatus::Unknown(failure()),
        ),
    ] {
        let mut script = ValueScript {
            value: Some(value),
            expire_after_value: false,
            expired: std::cell::Cell::new(false),
            trace: vec![],
        };
        let observed = crate::ax::content_attrs::read_with_observed(
            AttrStatus::Known("AXTextField".into()),
            &mut script,
            &mut false,
        );
        assert!(observed.legacy.is_some());
        assert_eq!(script.trace, ["batch", "value"]);
        assert_evidence(&observed.evidence, ReadSite::Content("AXValue"), expected);
    }
}

#[test]
fn abandoned_attribute_read_keeps_deadline_evidence() {
    let mut script = ValueScript {
        value: Some(Err(kAXErrorCannotComplete)),
        expire_after_value: true,
        expired: std::cell::Cell::new(false),
        trace: vec![],
    };
    let observed = crate::ax::content_attrs::read_with_observed(
        AttrStatus::Known("AXTextField".into()),
        &mut script,
        &mut false,
    );
    assert!(observed.legacy.is_none());
    assert_eq!(script.trace, ["batch", "value"]);
    assert_evidence(
        &observed.evidence,
        ReadSite::Content("AXValue"),
        EvidenceStatus::Unknown(failure()),
    );
    assert_evidence(
        &observed.evidence,
        ReadSite::Content("deadline"),
        EvidenceStatus::DeadlineExpired,
    );
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
    assert!(observed
        .evidence
        .iter()
        .any(|entry| entry.site == ReadSite::Content("AXDescription")
            && entry.listing == Some(AttributeListing::Listed)));
    assert_evidence(
        &observed.evidence,
        ReadSite::Content("AXTitle"),
        EvidenceStatus::Known,
    );
    assert_evidence(
        &observed.evidence,
        ReadSite::Content("AXPlaceholderValue"),
        EvidenceStatus::Absent,
    );

    for (index, name) in [
        "AXTitle",
        "AXPlaceholderValue",
        "AXDescription",
        "AXIdentifier",
        "AXHelp",
        "AXSubrole",
        "AXRoleDescription",
    ]
    .into_iter()
    .enumerate()
    {
        for case in CASES {
            let mut attrs = std::array::from_fn(|_| AttrStatus::Absent);
            attrs[0] = AttrStatus::Known("title".into());
            attrs[6] = AttrStatus::Known("text entry".into());
            attrs[index] = case.text();
            let mut script = ContentScript {
                attrs,
                trace: vec![],
            };
            let result = crate::ax::content_attrs::read_with_observed(
                AttrStatus::Known("AXTextArea".into()),
                &mut script,
                &mut false,
            );
            assert!(result.legacy.is_some());
            assert_evidence(&result.evidence, ReadSite::Content(name), case.status());
            let permitted = index == 6 || matches!(case, Case::Known | Case::Absent);
            assert_eq!(
                script.trace,
                if permitted {
                    vec!["batch", "value"]
                } else {
                    vec!["batch"]
                }
            );
        }
    }
}

const CONTROL: &str =
    "AXEnabled,AXPosition,AXSize,AXValueDescription,AXMinValue,AXMaxValue,AXSelected";

fn plain_root(
    role: Read<String>,
    actions: Read<Vec<String>>,
    read_value: bool,
) -> crate::ax::evidence::Observed<TreeWalkResult> {
    const CONTENT: &str =
        "AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription";
    const FRAME: &str = "AXPosition,AXSize";
    let mut attrs = vec![Slot::Text(AttrStatus::Absent); 7];
    attrs[0] = Slot::Text(AttrStatus::Known("Root".into()));
    let mut steps = vec![
        step("timeout 1", Reply::Unit),
        step("text 1 AXRole", Reply::Text(role)),
        step(
            &format!("batch 1 {CONTENT}"),
            Reply::Batch(Ok(Batch(attrs))),
        ),
    ];
    if read_value {
        steps.push(step("value 1", Reply::Value(Ok(None))));
    }
    steps.extend([
        step("actions 1", Reply::Actions(actions)),
        step(
            &format!("batch 1 {FRAME}"),
            Reply::Batch(Ok(Batch(vec![Slot::Point(None), Slot::Size(None)]))),
        ),
        step("children 1", Reply::Children(Read::Absent)),
    ]);
    let fake = Fake::new(&[(1, 1, 1, &[])], steps);
    let root = fake.handle(1);
    let observed = run_fake_observed(&fake, &root, None, 10, WalkBudget::nodes_only(10), true);
    assert!(observed.legacy.tree_markdown.contains("Root"));
    drop(root);
    fake.assert_consumed();
    fake.assert_lifetimes(&[]);
    observed
}

#[test]
fn role_read_preserves_unknown_and_absent() {
    for case in [Case::AxFailure, Case::Known, Case::Absent, Case::WrongType] {
        let observed = plain_root(
            case.read("AXWindow".into()),
            Read::Known(vec![]),
            matches!(case, Case::Known),
        );
        assert_eq!(observed.legacy.nodes_skipped, !matches!(case, Case::Known));
        assert_evidence(&observed.evidence, ReadSite::Role, case.status());
    }
}

#[test]
fn action_names_preserve_unknown_and_absent() {
    for case in [Case::AxFailure, Case::Known, Case::Absent, Case::WrongType] {
        let observed = plain_root(Read::Known("AXWindow".into()), case.read(vec![]), true);
        assert!(!observed.legacy.nodes_skipped);
        assert!(observed.legacy.nodes[0].actions.is_empty());
        assert_evidence(&observed.evidence, ReadSite::ActionNames, case.status());
    }
}

#[test]
fn menu_selected_preserves_unknown_and_absent_with_legacy_skip() {
    const CONTENT: &str =
        "AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription";
    const FRAME: &str = "AXPosition,AXSize";
    for (selection, expected, skipped) in [
        (
            Read::Unknown(failure()),
            EvidenceStatus::Unknown(failure()),
            true,
        ),
        (
            Read::Unknown(ReadFailure::WrongType),
            EvidenceStatus::Unknown(ReadFailure::WrongType),
            true,
        ),
        (Read::Absent, EvidenceStatus::Absent, true),
        (Read::Known(false), EvidenceStatus::Known, true),
        (Read::Known(true), EvidenceStatus::Known, false),
    ] {
        let mut attrs = vec![Slot::Text(AttrStatus::Absent); 7];
        attrs[0] = Slot::Text(AttrStatus::Known("Menu".into()));
        let mut steps = vec![
            step("timeout 1", Reply::Unit),
            step(
                "text 1 AXRole",
                Reply::Text(Read::Known("AXMenuBarItem".into())),
            ),
            step(
                &format!("batch 1 {CONTENT}"),
                Reply::Batch(Ok(Batch(attrs))),
            ),
            step("value 1", Reply::Value(Ok(None))),
            step("actions 1", Reply::Actions(Read::Known(vec![]))),
            step(
                &format!("batch 1 {FRAME}"),
                Reply::Batch(Ok(Batch(vec![Slot::Point(None), Slot::Size(None)]))),
            ),
            step("bool 1 AXSelected", Reply::Bool(selection)),
        ];
        if !skipped {
            steps.push(step("children 1", Reply::Children(Read::Absent)));
        }
        let fake = Fake::new(&[(1, 1, 1, &[])], steps);
        let root = fake.handle(1);
        let observed = with_menus_collapsed(true, || {
            run_fake_observed(&fake, &root, None, 10, WalkBudget::nodes_only(10), true)
        });
        assert_eq!(observed.legacy.nodes_skipped, skipped);
        assert!(observed.legacy.tree_markdown.contains("Menu"));
        drop(root);
        fake.assert_consumed();
        fake.assert_lifetimes(&[]);
        assert_evidence(&observed.evidence, ReadSite::MenuSelected, expected);
    }
}

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
        Some(1),
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
    for name in ["AXPosition", "AXSize", "AXValueDescription", "AXMaxValue"] {
        assert_evidence(
            &observed.evidence,
            ReadSite::Control(name),
            EvidenceStatus::Absent,
        );
    }
    assert_evidence(
        &observed.evidence,
        ReadSite::Control("AXMinValue"),
        EvidenceStatus::Unknown(ReadFailure::WrongType),
    );
    assert_evidence(
        &observed.evidence,
        ReadSite::Control("AXSelected"),
        EvidenceStatus::Known,
    );
    assert!(observed
        .evidence
        .iter()
        .all(|entry| entry.owner == Some(1) && entry.attempt == 0));

    for (index, name) in CONTROL_NAMES.into_iter().enumerate() {
        for case in CASES {
            let mut slots: Vec<_> = (0..7).map(|i| Case::Known.slot(i)).collect();
            slots[index] = case.slot(index);
            let fake = Fake::new(
                &[(1, 1, 1, &[])],
                vec![step(
                    &format!("batch 1 {CONTROL}"),
                    Reply::Batch(Ok(Batch(slots))),
                )],
            );
            let element = fake.handle(1);
            let result = crate::ax::control_attrs::read_control_attrs_observed_with(
                &fake,
                &element,
                &mut WalkBudget::nodes_only(10),
                &mut false,
                Some(1),
            );
            assert!(result.legacy.is_some());
            fake.assert_consumed();
            drop(element);
            fake.assert_lifetimes(&[]);
            assert_evidence(&result.evidence, ReadSite::Control(name), case.status());
            assert!(result.evidence.iter().all(|entry| entry.owner == Some(1)));
        }
    }
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
        Some(1),
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
    assert_evidence(
        &observed.evidence,
        ReadSite::Batch("control"),
        EvidenceStatus::Unknown(ReadFailure::WrongType),
    );
    for name in ["AXPosition", "AXValueDescription", "AXMaxValue"] {
        assert_evidence(
            &observed.evidence,
            ReadSite::Control(name),
            EvidenceStatus::Absent,
        );
    }
    assert_evidence(
        &observed.evidence,
        ReadSite::Control("AXMinValue"),
        EvidenceStatus::Unknown(ReadFailure::WrongType),
    );
    assert!(observed
        .evidence
        .iter()
        .any(|entry| entry.site == ReadSite::Batch("control") && entry.attempt == 0));
    assert!(observed
        .evidence
        .iter()
        .any(|entry| entry.site == ReadSite::Control("AXEnabled") && entry.attempt == 1));

    for (index, name) in CONTROL_NAMES.into_iter().enumerate() {
        for case in CASES {
            let selected = |i| if i == index { case } else { Case::Known };
            let mut steps = vec![
                step(
                    &format!("batch 1 {CONTROL}"),
                    Reply::Batch(Err(BatchFailure::Decode)),
                ),
                step("bool 1 AXEnabled", Reply::Bool(selected(0).read(true))),
                step("position 1", Reply::Point(selected(1).read((1.0, 2.0)))),
            ];
            if matches!(selected(1), Case::Known) {
                steps.push(step("size 1", Reply::Point(selected(2).read((3.0, 4.0)))));
            }
            steps.extend([
                step(
                    "text 1 AXValueDescription",
                    Reply::Text(selected(3).read("label".into())),
                ),
                step("number 1 AXMinValue", Reply::Number(selected(4).read(1.0))),
                step("number 1 AXMaxValue", Reply::Number(selected(5).read(2.0))),
                step("bool 1 AXSelected", Reply::Bool(selected(6).read(true))),
            ]);
            let fake = Fake::new(&[(1, 1, 1, &[])], steps);
            let element = fake.handle(1);
            let result = crate::ax::control_attrs::read_control_attrs_observed_with(
                &fake,
                &element,
                &mut WalkBudget::nodes_only(10),
                &mut false,
                Some(1),
            );
            assert!(result.legacy.is_some());
            fake.assert_consumed();
            drop(element);
            fake.assert_lifetimes(&[]);
            assert_evidence(&result.evidence, ReadSite::Control(name), case.status());
            assert!(result
                .evidence
                .iter()
                .any(|entry| entry.site == ReadSite::Control(name)
                    && entry.owner == Some(1)
                    && entry.attempt == 1));
        }
    }
}

#[test]
fn recovered_batch_failure_preserves_attempt_history() {
    let fake = Fake::new(
        &[(1, 1, 1, &[])],
        vec![
            step(
                &format!("batch 1 {CONTROL}"),
                Reply::Batch(Err(BatchFailure::WholeCall(kAXErrorCannotComplete))),
            ),
            step("bool 1 AXEnabled", Reply::Bool(Read::Known(true))),
            step("position 1", Reply::Point(Read::Known((1.0, 2.0)))),
            step("size 1", Reply::Point(Read::Known((3.0, 4.0)))),
            step(
                "text 1 AXValueDescription",
                Reply::Text(Read::Known("value".into())),
            ),
            step("number 1 AXMinValue", Reply::Number(Read::Known(0.0))),
            step("number 1 AXMaxValue", Reply::Number(Read::Known(1.0))),
            step("bool 1 AXSelected", Reply::Bool(Read::Known(false))),
        ],
    );
    let root = fake.handle(1);
    let mut batching_disabled = false;
    let observed = crate::ax::control_attrs::read_control_attrs_observed_with(
        &fake,
        &root,
        &mut WalkBudget::nodes_only(10),
        &mut batching_disabled,
        Some(1),
    );
    assert_eq!(
        observed.legacy.as_ref().expect("replayed control").frame,
        Some([1.0, 2.0, 3.0, 4.0])
    );
    assert!(!batching_disabled);
    fake.assert_consumed();
    drop(root);
    fake.assert_lifetimes(&[]);
    assert!(observed
        .evidence
        .iter()
        .any(|entry| entry.site == ReadSite::Batch("control")
            && entry.owner == Some(1)
            && entry.attempt == 0
            && entry.status == EvidenceStatus::Unknown(failure())));
    for name in CONTROL_NAMES {
        assert!(observed
            .evidence
            .iter()
            .any(|entry| entry.site == ReadSite::Control(name)
                && entry.owner == Some(1)
                && entry.attempt == 1
                && entry.status == EvidenceStatus::Known));
    }
}

#[test]
fn frame_batch_preserves_unknown_and_absent() {
    const FRAME: &str = "AXPosition,AXSize";
    for (index, name) in [(1, "AXPosition"), (2, "AXSize")] {
        for case in [Case::AxFailure, Case::Known, Case::Absent, Case::WrongType] {
            let mut slots = vec![Case::Known.slot(1), Case::Known.slot(2)];
            slots[index - 1] = case.slot(index);
            let fake = Fake::new(
                &[(1, 1, 1, &[])],
                vec![step(
                    &format!("batch 1 {FRAME}"),
                    Reply::Batch(Ok(Batch(slots))),
                )],
            );
            let root = fake.handle(1);
            let observed = crate::ax::control_attrs::read_frame_observed_with(
                &fake,
                &root,
                &mut WalkBudget::nodes_only(10),
                &mut false,
                Some(1),
            );
            assert_eq!(
                observed.legacy,
                Some(if matches!(case, Case::Known) {
                    Some([1.0, 2.0, 3.0, 4.0])
                } else {
                    None
                })
            );
            fake.assert_consumed();
            drop(root);
            fake.assert_lifetimes(&[]);
            assert_evidence(&observed.evidence, ReadSite::Frame(name), case.status());
            assert!(observed
                .evidence
                .iter()
                .any(|entry| entry.site == ReadSite::Frame(name) && entry.owner == Some(1)));
        }
    }
}

#[test]
fn frame_singles_preserve_unknown_and_absent() {
    const FRAME: &str = "AXPosition,AXSize";
    for (index, name) in [(1, "AXPosition"), (2, "AXSize")] {
        for case in [Case::AxFailure, Case::Known, Case::Absent, Case::WrongType] {
            let position = if index == 1 { case } else { Case::Known };
            let size = if index == 2 { case } else { Case::Known };
            let mut steps = vec![
                step(
                    &format!("batch 1 {FRAME}"),
                    Reply::Batch(Err(BatchFailure::Decode)),
                ),
                step("position 1", Reply::Point(position.read((1.0, 2.0)))),
            ];
            if matches!(position, Case::Known) {
                steps.push(step("size 1", Reply::Point(size.read((3.0, 4.0)))));
            }
            let fake = Fake::new(&[(1, 1, 1, &[])], steps);
            let root = fake.handle(1);
            let observed = crate::ax::control_attrs::read_frame_observed_with(
                &fake,
                &root,
                &mut WalkBudget::nodes_only(10),
                &mut false,
                Some(1),
            );
            assert_eq!(
                observed.legacy,
                Some(if matches!(case, Case::Known) {
                    Some([1.0, 2.0, 3.0, 4.0])
                } else {
                    None
                })
            );
            fake.assert_consumed();
            drop(root);
            fake.assert_lifetimes(&[]);
            assert_evidence(&observed.evidence, ReadSite::Frame(name), case.status());
            assert!(observed
                .evidence
                .iter()
                .any(|entry| entry.site == ReadSite::Frame(name)
                    && entry.owner == Some(1)
                    && entry.attempt == 1));
        }
    }
}

#[test]
fn selected_settable_preserves_unknown_and_no() {
    use crate::ax::write_outcome::Settable;
    for value in [
        Settable::Unknown(kAXErrorCannotComplete),
        Settable::No,
        Settable::Yes,
    ] {
        let called = std::cell::Cell::new(0);
        let observed = selected_probe_observed(
            Some(7),
            false,
            || {
                called.set(called.get() + 1);
                value
            },
            || panic!("deadline not reached"),
        );
        assert_eq!(observed.legacy, value);
        assert_eq!(called.get(), 1);
        let status = match value {
            Settable::Unknown(code) => EvidenceStatus::Unknown(ReadFailure::Ax(code)),
            _ => EvidenceStatus::Known,
        };
        assert_evidence(&observed.evidence, ReadSite::SelectedSettable, status);
        assert_eq!(observed.evidence[0].owner, Some(7));
    }
    let skipped = std::cell::Cell::new(false);
    let observed = selected_probe_observed(
        Some(7),
        true,
        || panic!("no probe after deadline"),
        || skipped.set(true),
    );
    assert_eq!(observed.legacy, Settable::Unknown(kAXErrorCannotComplete));
    assert!(skipped.get());
    assert_evidence(
        &observed.evidence,
        ReadSite::SelectedSettable,
        EvidenceStatus::DeadlineExpired,
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
    let fake = Fake::new(
        &[(1, 1, 1, &[])],
        vec![step(
            "element 1 AXVerticalScrollBar",
            Reply::Element(Read::Absent),
        )],
    );
    let element = fake.handle(1);
    let absent = crate::ax::scroll_area::bar_position_observed_with(&fake, &element, "down");
    assert_eq!(absent.legacy, None);
    fake.assert_consumed();
    drop(element);
    fake.assert_lifetimes(&[]);
    assert_evidence(
        &absent.evidence,
        ReadSite::Scrollbar("AXVerticalScrollBar"),
        EvidenceStatus::Absent,
    );

    for case in CASES {
        let mut steps = vec![step(
            "element 1 AXVerticalScrollBar",
            Reply::Element(case.read(2)),
        )];
        if matches!(case, Case::Known) {
            steps.push(step(
                "text 2 AXRole",
                Reply::Text(Read::Known("AXGroup".into())),
            ));
        }
        let fake = Fake::new(&[(1, 1, 1, &[]), (2, 2, 2, &[])], steps);
        let element = fake.handle(1);
        let result = crate::ax::scroll_area::bar_position_observed_with(&fake, &element, "down");
        assert_eq!(result.legacy, None);
        fake.assert_consumed();
        drop(element);
        fake.assert_lifetimes(&[]);
        assert_evidence(
            &result.evidence,
            ReadSite::Scrollbar("AXVerticalScrollBar"),
            case.status(),
        );
        assert!(result.evidence.iter().all(|entry| entry.owner.is_none()));
    }
}

#[test]
fn scrollbar_role_preserves_unknown_and_absent() {
    for case in CASES {
        let mut steps = vec![
            step(
                "element 1 AXVerticalScrollBar",
                Reply::Element(Read::Known(2)),
            ),
            step(
                "text 2 AXRole",
                Reply::Text(case.read("AXScrollBar".into())),
            ),
        ];
        if matches!(case, Case::Known) {
            steps.push(step("number 2 AXValue", Reply::Number(Read::Known(0.5))));
        }
        let fake = Fake::new(&[(1, 1, 1, &[]), (2, 2, 2, &[])], steps);
        let root = fake.handle(1);
        let observed = crate::ax::scroll_area::bar_position_observed_with(&fake, &root, "down");
        assert_eq!(
            observed.legacy,
            if matches!(case, Case::Known) {
                Some(0.5)
            } else {
                None
            }
        );
        fake.assert_consumed();
        drop(root);
        fake.assert_lifetimes(&[]);
        assert_evidence(
            &observed.evidence,
            ReadSite::Scrollbar("AXRole"),
            case.status(),
        );
    }
}

#[test]
fn scrollbar_value_preserves_unknown_and_absent() {
    for case in CASES {
        let fake = Fake::new(
            &[(1, 1, 1, &[]), (2, 2, 2, &[])],
            vec![
                step(
                    "element 1 AXVerticalScrollBar",
                    Reply::Element(Read::Known(2)),
                ),
                step(
                    "text 2 AXRole",
                    Reply::Text(Read::Known("AXScrollBar".into())),
                ),
                step("number 2 AXValue", Reply::Number(case.read(0.5))),
            ],
        );
        let root = fake.handle(1);
        let observed = crate::ax::scroll_area::bar_position_observed_with(&fake, &root, "down");
        assert_eq!(
            observed.legacy,
            if matches!(case, Case::Known) {
                Some(0.5)
            } else {
                None
            }
        );
        fake.assert_consumed();
        drop(root);
        fake.assert_lifetimes(&[]);
        assert_evidence(
            &observed.evidence,
            ReadSite::Scrollbar("AXValue"),
            case.status(),
        );
    }
}

#[test]
fn scroll_extra_labels_preserve_unknown_and_absent() {
    const CONTENT: &str =
        "AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription";
    for name in ["AXDescription", "AXIdentifier"] {
        for case in [Case::AxFailure, Case::Known, Case::Absent, Case::WrongType] {
            let mut attrs = vec![Slot::Text(AttrStatus::Absent); 7];
            attrs[0] = Slot::Text(AttrStatus::Known("Scroll".into()));
            let desc = if name == "AXDescription" {
                case
            } else {
                Case::Known
            };
            let id = if name == "AXIdentifier" {
                case
            } else {
                Case::Known
            };
            let fake = Fake::new(
                &[(1, 1, 1, &[]), (2, 2, 2, &[])],
                vec![
                    step("timeout 1", Reply::Unit),
                    step(
                        "text 1 AXRole",
                        Reply::Text(Read::Known("AXScrollArea".into())),
                    ),
                    step(
                        &format!("batch 1 {CONTENT}"),
                        Reply::Batch(Ok(Batch(attrs))),
                    ),
                    step("value 1", Reply::Value(Ok(None))),
                    step(
                        "element 1 AXVerticalScrollBar",
                        Reply::Element(Read::Known(2)),
                    ),
                    step(
                        "text 2 AXRole",
                        Reply::Text(Read::Known("AXScrollBar".into())),
                    ),
                    step("number 2 AXValue", Reply::Number(Read::Known(0.25))),
                    step(
                        "element 1 AXHorizontalScrollBar",
                        Reply::Element(Read::Absent),
                    ),
                    step("snapshot 1", Reply::Unit),
                    step(
                        "text 1 AXDescription",
                        Reply::Text(desc.read("Pane".into())),
                    ),
                    step("text 1 AXIdentifier", Reply::Text(id.read("main".into()))),
                    step("actions 1", Reply::Actions(Read::Known(vec![]))),
                    step("position 1", Reply::Point(Read::Absent)),
                    step("children 1", Reply::Children(Read::Absent)),
                ],
            );
            let root = fake.handle(1);
            let observed = with_exhaustive_nodes(false, || {
                run_fake_observed(&fake, &root, None, 10, WalkBudget::nodes_only(10), true)
            });
            assert!(!observed.legacy.nodes_skipped);
            assert!(observed.legacy.tree_markdown.contains("scrolled 25% down"));
            fake.assert_consumed();
            drop(root);
            fake.assert_lifetimes(&[(1, 1)]);
            assert_evidence(&observed.evidence, ReadSite::Scrollbar(name), case.status());
            assert_evidence(
                &observed.evidence,
                ReadSite::Frame("scroll"),
                EvidenceStatus::Absent,
            );
        }
    }
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
    assert_evidence(
        &observed.evidence,
        ReadSite::Tab("AXDescription"),
        EvidenceStatus::Absent,
    );
    assert_evidence(
        &observed.evidence,
        ReadSite::Tab("AXRoleDescription"),
        EvidenceStatus::Absent,
    );

    for (index, name) in ["AXTitle", "AXDescription", "AXRoleDescription"]
        .into_iter()
        .enumerate()
    {
        for case in CASES {
            let steps = ["AXTitle", "AXDescription", "AXRoleDescription"]
                .into_iter()
                .enumerate()
                .map(|(i, name)| {
                    let state = if i == index { case } else { Case::Known };
                    step(
                        &format!("text 1 {name}"),
                        Reply::Text(state.read("label".into())),
                    )
                })
                .collect();
            let fake = Fake::new(&[(1, 1, 1, &[])], steps);
            let element = fake.handle(1);
            let result = plan_macos_tab_strip_observed(
                &fake,
                &element,
                "AXGroup",
                &[],
                &mut WalkBudget::nodes_only(10),
                &mut false,
            );
            assert!(result.legacy.is_none());
            fake.assert_consumed();
            drop(element);
            fake.assert_lifetimes(&[]);
            assert_evidence(&result.evidence, ReadSite::Tab(name), case.status());
            assert!(result.evidence.iter().all(|entry| entry.owner.is_none()));
        }
    }
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
    assert_evidence(
        &observed.evidence,
        ReadSite::Dense("AXRows"),
        EvidenceStatus::Absent,
    );
    assert_evidence(
        &observed.evidence,
        ReadSite::Dense("AXVisibleChildren"),
        EvidenceStatus::Absent,
    );

    for name in ["AXVisibleRows", "AXVisibleChildren"] {
        for case in CASES {
            let rows = if name == "AXVisibleRows" {
                case
            } else {
                Case::Absent
            };
            let mut steps = vec![
                step("set 1 AXRows", Reply::Set(Read::Absent)),
                step("set 1 AXVisibleRows", Reply::Set(rows.read(vec![1]))),
            ];
            if !matches!(rows, Case::Known) {
                let children = if name == "AXVisibleChildren" {
                    case
                } else {
                    Case::Absent
                };
                steps.push(step(
                    "set 1 AXVisibleChildren",
                    Reply::Set(children.read(vec![1])),
                ));
            }
            let fake = Fake::new(&[(1, 1, 1, &[])], steps);
            let element = fake.handle(1);
            let result = dense_rows::plan_collection_children_observed(
                &fake,
                &element,
                &element,
                "AXTable",
                &[],
                &WalkBudget::nodes_only(10),
            );
            assert!(result.legacy.is_empty());
            fake.assert_consumed();
            drop(element);
            fake.assert_lifetimes(&[]);
            assert_evidence(&result.evidence, ReadSite::Dense(name), case.status());
        }
    }
}

#[test]
fn dense_row_sets_preserve_unknown_and_absent() {
    for case in CASES {
        let fake = Fake::new(
            &[(1, 1, 1, &[])],
            vec![
                step("set 1 AXRows", Reply::Set(case.read(vec![1]))),
                step("set 1 AXVisibleRows", Reply::Set(Read::Known(vec![1]))),
            ],
        );
        let root = fake.handle(1);
        let observed = dense_rows::plan_collection_children_observed(
            &fake,
            &root,
            &root,
            "AXTable",
            &[],
            &WalkBudget::nodes_only(10),
        );
        assert!(observed.legacy.is_empty());
        fake.assert_consumed();
        drop(root);
        fake.assert_lifetimes(&[]);
        assert_evidence(&observed.evidence, ReadSite::Dense("AXRows"), case.status());
    }
}

#[test]
fn dense_child_role_preserves_unknown_and_absent() {
    for case in CASES {
        let mut steps = vec![
            step("set 1 AXRows", Reply::Set(Read::Absent)),
            step("set 1 AXVisibleRows", Reply::Set(Read::Known(vec![2]))),
            step("timeout 2", Reply::Unit),
            step("text 2 AXRole", Reply::Text(case.read("AXRow".into()))),
        ];
        if matches!(case, Case::Known) {
            steps.push(step("contains 2 [2]", Reply::Unit));
        }
        let fake = Fake::new(&[(1, 1, 1, &[2]), (2, 2, 2, &[])], steps);
        let root = fake.handle(1);
        let child = fake.handle(2);
        let observed = dense_rows::plan_collection_children_observed(
            &fake,
            &root,
            &root,
            "AXTable",
            std::slice::from_ref(&child),
            &WalkBudget::nodes_only(10),
        );
        assert_eq!(
            observed.legacy,
            [if matches!(case, Case::Known) {
                ChildPlan::Defer { visible: true }
            } else {
                ChildPlan::WalkNow
            }]
        );
        fake.assert_consumed();
        drop(child);
        drop(root);
        fake.assert_lifetimes(&[]);
        assert_evidence(&observed.evidence, ReadSite::Dense("AXRole"), case.status());
    }
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

    for case in CASES {
        let result =
            crate::ax::tree_source::native::observed_children_read(7, case.read(vec![11, 33]));
        assert_evidence(&result.evidence, ReadSite::Children, case.status());
        assert_eq!(result.evidence[0].owner, Some(7));
        match (case, result.legacy) {
            (Case::Known, Read::Known(items)) => assert_eq!(items, [11, 33]),
            (Case::Absent, Read::Absent) => {}
            (Case::AxFailure, Read::Unknown(ReadFailure::Ax(code)))
                if code == kAXErrorCannotComplete => {}
            (Case::WrongType, Read::Unknown(ReadFailure::WrongType)) => {}
            _ => panic!("legacy child projection changed"),
        }
    }
    let credits = std::cell::Cell::new(0);
    let mixed =
        crate::ax::tree_source::native::decode_child_members([Some(1), None, Some(2)], |item| {
            if item.is_some() {
                credits.set(credits.get() + 1);
            }
            item
        });
    assert_eq!(mixed.legacy, [1, 2]);
    assert_eq!(credits.get(), 2);
    assert_eq!(mixed.evidence.len(), 1);
}

#[test]
fn nested_walk_evidence_is_isolated() {
    use std::cell::RefCell;
    use std::rc::Rc;

    const CONTENT: &str =
        "AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription";
    const FRAME: &str = "AXPosition,AXSize";
    let inner_evidence = Rc::new(RefCell::new(None));
    let saved = inner_evidence.clone();
    let nested = move || {
        let mut attrs = vec![Slot::Text(AttrStatus::Absent); 7];
        attrs[0] = Slot::Text(AttrStatus::Known("inner".into()));
        attrs[3] = Slot::Text(AttrStatus::WrongType);
        let inner = Fake::new(
            &[(2, 2, 2, &[])],
            vec![
                step("timeout 2", Reply::Unit),
                step("text 2 AXRole", Reply::Text(Read::Known("AXWindow".into()))),
                step(
                    &format!("batch 2 {CONTENT}"),
                    Reply::Batch(Ok(Batch(attrs))),
                ),
                step("actions 2", Reply::Actions(Read::Known(vec![]))),
                step(
                    &format!("batch 2 {FRAME}"),
                    Reply::Batch(Ok(Batch(vec![Slot::Point(None), Slot::Size(None)]))),
                ),
                step("children 2", Reply::Children(Read::Absent)),
            ],
        );
        let root = inner.handle(2);
        let observed = run_fake_observed(&inner, &root, None, 10, WalkBudget::nodes_only(10), true);
        assert!(observed.legacy.tree_markdown.contains("inner"));
        drop(root);
        inner.assert_consumed();
        inner.assert_lifetimes(&[]);
        *saved.borrow_mut() = Some(observed.evidence);
    };
    let mut attrs = vec![Slot::Text(AttrStatus::Absent); 7];
    attrs[0] = Slot::Text(AttrStatus::Known("outer".into()));
    attrs[2] = Slot::Text(AttrStatus::WrongType);
    let outer = Fake::new(
        &[(1, 1, 1, &[])],
        vec![
            step("timeout 1", Reply::Unit),
            step("text 1 AXRole", Reply::Text(Read::Known("AXWindow".into()))),
            step(
                &format!("batch 1 {CONTENT}"),
                Reply::Batch(Ok(Batch(attrs))),
            ),
            step("actions 1", Reply::Nested(Box::new(nested))),
            step("actions 1", Reply::Actions(Read::Known(vec![]))),
            step(
                &format!("batch 1 {FRAME}"),
                Reply::Batch(Ok(Batch(vec![Slot::Point(None), Slot::Size(None)]))),
            ),
            step("children 1", Reply::Children(Read::Absent)),
        ],
    );
    let root = outer.handle(1);
    let observed = run_fake_observed(&outer, &root, None, 10, WalkBudget::nodes_only(10), true);
    assert!(observed.legacy.tree_markdown.contains("outer"));
    drop(root);
    outer.assert_consumed();
    outer.assert_lifetimes(&[]);
    let inner = inner_evidence.borrow();
    let inner = inner.as_ref().expect("nested walk executed");
    assert_evidence(
        inner,
        ReadSite::Content("AXIdentifier"),
        EvidenceStatus::Unknown(ReadFailure::WrongType),
    );
    assert!(!inner
        .iter()
        .any(|entry| entry.site == ReadSite::Content("AXDescription")
            && entry.status == EvidenceStatus::Unknown(ReadFailure::WrongType)));
    assert_evidence(
        &observed.evidence,
        ReadSite::Content("AXDescription"),
        EvidenceStatus::Unknown(ReadFailure::WrongType),
    );
    assert!(!observed
        .evidence
        .iter()
        .any(|entry| entry.site == ReadSite::Content("AXIdentifier")
            && entry.status == EvidenceStatus::Unknown(ReadFailure::WrongType)));
}

#[test]
fn value_settable_preserves_failure_and_false() {
    const CONTENT: &str =
        "AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole,AXRoleDescription";
    const FRAME: &str = "AXPosition,AXSize";
    for failed in [true, false] {
        let mut attrs = vec![Slot::Text(AttrStatus::Absent); 7];
        attrs[0] = Slot::Text(AttrStatus::Known("Edit".into()));
        let reply = if failed {
            Reply::WritableFailure(kAXErrorCannotComplete)
        } else {
            Reply::Writable(false)
        };
        let fake = Fake::new(
            &[(1, 1, 1, &[])],
            vec![
                step("timeout 1", Reply::Unit),
                step(
                    "text 1 AXRole",
                    Reply::Text(Read::Known("AXTextField".into())),
                ),
                step(
                    &format!("batch 1 {CONTENT}"),
                    Reply::Batch(Ok(Batch(attrs))),
                ),
                step("value 1", Reply::Value(Ok(None))),
                step("actions 1", Reply::Actions(Read::Known(vec![]))),
                step("value_settable 1", reply),
                step(
                    &format!("batch 1 {FRAME}"),
                    Reply::Batch(Ok(Batch(vec![Slot::Point(None), Slot::Size(None)]))),
                ),
                step("children 1", Reply::Children(Read::Absent)),
            ],
        );
        let root = fake.handle(1);
        let observed = run_fake_observed(&fake, &root, None, 10, WalkBudget::nodes_only(10), true);
        assert!(observed.legacy.tree_markdown.contains("Edit"));
        assert!(observed
            .legacy
            .nodes
            .iter()
            .all(|node| node.element_index.is_none()));
        drop(root);
        fake.assert_consumed();
        fake.assert_lifetimes(&[]);
        assert_evidence(
            &observed.evidence,
            ReadSite::ValueSettable,
            if failed {
                EvidenceStatus::Unknown(failure())
            } else {
                EvidenceStatus::Known
            },
        );
        assert!(observed
            .evidence
            .iter()
            .any(|entry| entry.site == ReadSite::ValueSettable && entry.owner == Some(1)));
    }

    let mut attrs = vec![Slot::Text(AttrStatus::Absent); 7];
    attrs[0] = Slot::Text(AttrStatus::Known("Edit".into()));
    let control = Batch(vec![
        Slot::Bool(None),
        Slot::Point(None),
        Slot::Size(None),
        Slot::Text(AttrStatus::Absent),
        Slot::Number(None),
        Slot::Number(None),
        Slot::Bool(None),
    ]);
    let fake = Fake::new(
        &[(1, 1, 1, &[])],
        vec![
            step("timeout 1", Reply::Unit),
            step(
                "text 1 AXRole",
                Reply::Text(Read::Known("AXTextField".into())),
            ),
            step(
                &format!("batch 1 {CONTENT}"),
                Reply::Batch(Ok(Batch(attrs))),
            ),
            step("value 1", Reply::Value(Ok(None))),
            step("actions 1", Reply::Actions(Read::Known(vec![]))),
            step("value_settable 1", Reply::Writable(true)),
            step(&format!("batch 1 {CONTROL}"), Reply::Batch(Ok(control))),
            step("snapshot 1", Reply::Unit),
            step("children 1", Reply::Children(Read::Absent)),
        ],
    );
    let root = fake.handle(1);
    let observed = run_fake_observed(&fake, &root, None, 10, WalkBudget::nodes_only(10), true);
    assert_eq!(observed.legacy.nodes[0].element_index, Some(0));
    drop(root);
    fake.assert_consumed();
    fake.assert_lifetimes(&[(1, 1)]);
    assert_evidence(
        &observed.evidence,
        ReadSite::ValueSettable,
        EvidenceStatus::Known,
    );
}
