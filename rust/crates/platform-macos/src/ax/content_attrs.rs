use super::batch_attrs::{batch_failure_action, BatchFailureAction, BatchRead};
#[cfg(test)]
use super::batch_attrs::BatchSlots;
use super::bindings::*;
use cua_driver_core::walk_budget::WalkBudget;
use cua_driver_core::snapshot_compaction::{is_secure_text_field, FieldSecurityMeta, REDACTED_VALUE};

const CONTENT_ATTRIBUTES: [&str; 6] = [
    "AXTitle",
    "AXPlaceholderValue",
    "AXDescription",
    "AXIdentifier",
    "AXHelp",
    "AXSubrole",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AttrStatus {
    Known(String),
    Absent,
    Failed,
}

impl AttrStatus {
    fn text(&self) -> Option<&str> {
        match self { Self::Known(value) => Some(value), _ => None }
    }
    fn complete(&self) -> bool { !matches!(self, Self::Failed) }
}

#[derive(Clone, Debug)]
pub(crate) struct SecurityEvidence {
    role: AttrStatus,
    attrs: [AttrStatus; 6],
}

impl SecurityEvidence {
    fn secure(&self) -> bool {
        is_secure_text_field(&FieldSecurityMeta {
            role: self.role.text().unwrap_or("AXUnknown"),
            title: self.attrs[0].text(), placeholder: self.attrs[1].text(),
            description: self.attrs[2].text(), identifier: self.attrs[3].text(),
            help: self.attrs[4].text(), subrole: self.attrs[5].text(),
            is_password_flag: false,
        })
    }
}

pub(crate) fn value_read_allowed(evidence: &SecurityEvidence) -> bool {
    matches!(&evidence.role, AttrStatus::Known(role) if !role.trim().is_empty() && role != "AXUnknown")
        && evidence.attrs.iter().all(AttrStatus::complete)
        && !evidence.secure()
}

trait ContentReader {
    fn batch(&mut self) -> Result<[AttrStatus; 6], super::batch_attrs::BatchFailure>;
    fn string(&mut self, name: &str) -> AttrStatus;
    fn value(&mut self) -> Result<Option<StringishAttrValue>, AXError>;
    fn expired(&self) -> bool;
    fn classified(&mut self) {}
}

fn read_with<R: ContentReader>(
    role: AttrStatus,
    reader: &mut R,
    batching_disabled: &mut bool,
) -> Option<ContentAttrs> {
    if reader.expired() { return None; }
    let attrs = if !*batching_disabled {
        match reader.batch() {
            Ok(attrs) => Some(attrs),
            Err(kind) => match batch_failure_action(kind, reader.expired(), false) {
                BatchFailureAction::DisableAndReplay => { *batching_disabled = true; None },
                BatchFailureAction::Replay => None,
                BatchFailureAction::StopForTimeout => return None,
            },
        }
    } else { None };
    let attrs = match attrs {
        Some(attrs) => attrs,
        None => {
            let mut result = Vec::with_capacity(6);
            for name in CONTENT_ATTRIBUTES {
                if reader.expired() { return None; }
                result.push(reader.string(name));
            }
            result.try_into().expect("six content attributes")
        }
    };
    let evidence = SecurityEvidence { role, attrs };
    let secure = evidence.secure();
    let allowed = value_read_allowed(&evidence);
    reader.classified();
    if reader.expired() { return None; }
    let read_value = if allowed { Some(reader.value()) } else { None };
    if reader.expired() { return None; }
    let value_failed = read_value.as_ref().is_some_and(Result::is_err);
    let copied_value = read_value.and_then(Result::ok).flatten();
    let text = |index: usize| evidence.attrs[index].text().map(str::to_owned);
    let placeholder = text(1);
    let value = if secure {
        Some(REDACTED_VALUE.to_owned())
    } else if allowed {
        value_with_placeholder(copied_value.as_ref(), || placeholder.clone())
    } else { None };
    let label_status = |index: usize| match &evidence.attrs[index] {
        AttrStatus::Known(s) if !s.trim().is_empty() => LabelSlot::Present,
        AttrStatus::Failed => LabelSlot::Failed,
        _ => LabelSlot::Absent,
    };
    let value_status = if !secure && (!allowed || value_failed) { LabelSlot::Failed }
        else if value.as_deref().is_some_and(|v| !v.trim().is_empty()) { LabelSlot::Present }
        else { LabelSlot::Absent };
    let read_failed = (!secure && !allowed) || label_hidden(&[
        label_status(0), label_status(2), value_status, label_status(3),
    ]);
    Some(ContentAttrs {
        title: text(0), copied_value, value, placeholder,
        description: text(2), identifier: text(3),
        help: text(4).filter(|help| !help.trim().is_empty()),
        subrole: text(5), read_failed, secure,
    })
}

pub(crate) struct ContentAttrs {
    pub title: Option<String>,
    pub copied_value: Option<StringishAttrValue>,
    pub value: Option<String>,
    pub placeholder: Option<String>,
    pub description: Option<String>,
    pub identifier: Option<String>,
    pub help: Option<String>,
    pub subrole: Option<String>,
    /// A single read failed with an error other than "no value" or
    /// "unsupported": the attribute may hold content this node does not show.
    /// (A batched read with such an error falls back to single reads.)
    pub read_failed: bool,
    pub secure: bool,
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
    #[cfg(test)]
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
            read_failed: false,
            subrole: None,
            secure: false,
        }
    }

    /// Content from one read per attribute. `Err` is a read that failed with
    /// an error other than "no value" or "unsupported".
    #[cfg(test)]
    fn from_single_reads(
        mut string: impl FnMut(&str) -> Result<Option<String>, AXError>,
        stringish_value: impl FnOnce() -> Result<Option<StringishAttrValue>, AXError>,
    ) -> Self {
        let title = string("AXTitle");
        let copied_value = stringish_value();
        let mut placeholder_read = None;
        let value =
            value_with_placeholder(copied_value.as_ref().ok().and_then(Option::as_ref), || {
                let read = string("AXPlaceholderValue");
                let placeholder = read.as_ref().ok().cloned().flatten();
                placeholder_read = Some(read);
                placeholder
            });
        let description = string("AXDescription");
        let identifier = string("AXIdentifier");
        let help = string("AXHelp");
        // The label a selector matches is the first present of title,
        // description, value (or placeholder) and identifier. A failed read
        // hides content only when nothing before it in that order is present.
        let value_failed =
            copied_value.is_err() || placeholder_read.as_ref().is_some_and(|read| read.is_err());
        let read_failed = label_hidden(&[
            label_slot(&title),
            label_slot(&description),
            if value_failed {
                LabelSlot::Failed
            } else if value
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            {
                LabelSlot::Present
            } else {
                LabelSlot::Absent
            },
            label_slot(&identifier),
        ]);
        Self {
            title: title.ok().flatten(),
            copied_value: copied_value.ok().flatten(),
            value,
            placeholder: placeholder_read.and_then(|read| read.ok().flatten()),
            description: description.ok().flatten(),
            identifier: identifier.ok().flatten(),
            help: help.ok().flatten().filter(|help| !help.trim().is_empty()),
            read_failed,
            subrole: None,
            secure: false,
        }
    }
}

/// One attribute of the label precedence, as read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LabelSlot {
    Present,
    Absent,
    Failed,
}

#[cfg(test)]
fn label_slot(read: &Result<Option<String>, AXError>) -> LabelSlot {
    match read {
        Err(_) => LabelSlot::Failed,
        Ok(Some(text)) if !text.trim().is_empty() => LabelSlot::Present,
        Ok(_) => LabelSlot::Absent,
    }
}

/// Whether a failed read comes before the first present label attribute.
fn label_hidden(slots: &[LabelSlot]) -> bool {
    slots
        .iter()
        .find(|slot| **slot != LabelSlot::Absent)
        .is_some_and(|slot| *slot == LabelSlot::Failed)
}

/// One read of `name`: its decoded value with the read's own error code, so
/// a failure is never mistaken for an absent attribute and no second request
/// is made.
///
/// # Safety
/// `element` must be a live `AXUIElementRef`.
unsafe fn single_read<T>(
    element: AXUIElementRef,
    name: &str,
    decode: impl FnOnce(core_foundation::base::CFTypeRef) -> Option<T>,
) -> Result<Option<T>, AXError> {
    use core_foundation::{base::TCFType, string::CFString};
    let attr = CFString::new(name);
    let mut value: core_foundation::base::CFTypeRef = std::ptr::null();
    let error = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    let decoded = (!value.is_null() && error == kAXErrorSuccess).then(|| decode(value));
    if !value.is_null() {
        core_foundation::base::CFRelease(value);
    }
    single_read_outcome(error, decoded.flatten(), || attribute_listed(element, name))
}

/// What one read means: a value, an absent attribute, or a failure that may
/// hide content.
///
/// Success, no value and "unsupported" are definite. A timeout or lost
/// connection, a stale element, or accessibility turned off hide content.
/// Any other error (`kAXErrorFailure`, illegal argument, not implemented) is
/// a generic failure that proves nothing by itself: the attribute counts as
/// absent only when the element does not list it among its attribute names
/// (`listed` returns `Some(false)`). AppKit answers `kAXErrorFailure` for an
/// attribute an element does not implement (TextEdit's AXDescription, 28 Oct
/// 2026); the name list tells that apart from a failed read of a real label.
fn single_read_outcome<T>(
    error: AXError,
    decoded: Option<T>,
    listed: impl FnOnce() -> Option<bool>,
) -> Result<Option<T>, AXError> {
    if [
        kAXErrorSuccess,
        kAXErrorNoValue,
        kAXErrorAttributeUnsupported,
    ]
    .contains(&error)
    {
        return Ok(decoded);
    }
    if [
        kAXErrorCannotComplete,
        kAXErrorInvalidUIElement,
        kAXErrorAPIDisabled,
    ]
    .contains(&error)
    {
        return Err(error);
    }
    match listed() {
        Some(false) => Ok(None),
        _ => Err(error),
    }
}

/// Whether `element` lists `name` among its attribute names; `None` when the
/// list cannot be read.
///
/// # Safety
/// `element` must be a live `AXUIElementRef`.
unsafe fn attribute_listed(element: AXUIElementRef, name: &str) -> Option<bool> {
    use core_foundation::{array::CFArray, base::TCFType, string::CFString};
    let mut names: core_foundation::array::CFArrayRef = std::ptr::null();
    let error = AXUIElementCopyAttributeNames(element, &mut names);
    if error != kAXErrorSuccess || names.is_null() {
        return None;
    }
    let names = CFArray::<CFString>::wrap_under_create_rule(names);
    Some(names.iter().any(|listed| listed.to_string() == name))
}

/// A timeout returns no content, so the walker emits only the role already read.
pub(crate) unsafe fn read_content_attrs(
    element: AXUIElementRef,
    role: &str,
    budget: &mut WalkBudget,
    batching_disabled: &mut bool,
) -> Option<ContentAttrs> {
    let role = if role == "AXUnknown" { AttrStatus::Failed } else { AttrStatus::Known(role.to_owned()) };
    let result = read_with(role, &mut NativeReader { element, budget }, batching_disabled);
    if result.is_none() { budget.stop_for_timeout(); }
    result
}

struct NativeReader<'a> { element: AXUIElementRef, budget: &'a WalkBudget }

impl ContentReader for NativeReader<'_> {
    fn batch(&mut self) -> Result<[AttrStatus; 6], super::batch_attrs::BatchFailure> {
        match unsafe { copy_attrs_batch(self.element, &CONTENT_ATTRIBUTES) } {
            BatchRead::Fallback(failure) => Err(failure),
            BatchRead::Slots(slots) => Ok(std::array::from_fn(|index| match slots.raw(index) {
                None => AttrStatus::Absent,
                Some(raw) => unsafe { string_from_cf(raw) }.map(AttrStatus::Known).unwrap_or(AttrStatus::Failed),
            })),
        }
    }

    fn string(&mut self, name: &str) -> AttrStatus {
        unsafe { read_string_status(self.element, name) }
    }

    fn value(&mut self) -> Result<Option<StringishAttrValue>, AXError> {
        unsafe { single_read(self.element, "AXValue", |value| stringish_from_cf(value)) }
    }

    fn expired(&self) -> bool { self.budget.expired() }
}

unsafe fn read_string_status(element: AXUIElementRef, name: &str) -> AttrStatus {
    use core_foundation::{base::TCFType, string::CFString};
    let attr = CFString::new(name);
    let mut value: core_foundation::base::CFTypeRef = std::ptr::null();
    let error = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    let decoded = if error == kAXErrorSuccess && !value.is_null() { string_from_cf(value) } else { None };
    if !value.is_null() { core_foundation::base::CFRelease(value); }
    if error == kAXErrorSuccess {
        return decoded.map(AttrStatus::Known).unwrap_or(AttrStatus::Failed);
    }
    match single_read_outcome::<String>(error, None, || attribute_listed(element, name)) {
        Ok(None) => AttrStatus::Absent,
        _ => AttrStatus::Failed,
    }
}

#[cfg(test)]
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

    struct FakeReader {
        trace: Vec<String>, attrs: [AttrStatus; 6], reject_batch: bool,
        expire_on_classification: bool, expired: bool,
    }
    impl FakeReader {
        fn new() -> Self { Self { trace: vec!["AXRole".into()], attrs: std::array::from_fn(|_| AttrStatus::Absent), reject_batch: false, expire_on_classification: false, expired: false } }
    }
    impl ContentReader for FakeReader {
        fn batch(&mut self) -> Result<[AttrStatus; 6], super::super::batch_attrs::BatchFailure> {
            self.trace.push(format!("batch:{}", CONTENT_ATTRIBUTES.join(",")));
            if self.reject_batch { Err(super::super::batch_attrs::BatchFailure::WholeCall(kAXErrorNotImplemented)) } else { Ok(self.attrs.clone()) }
        }
        fn string(&mut self, name: &str) -> AttrStatus {
            self.trace.push(name.into());
            self.attrs[CONTENT_ATTRIBUTES.iter().position(|n| *n == name).unwrap()].clone()
        }
        fn value(&mut self) -> Result<Option<StringishAttrValue>, AXError> {
            self.trace.push("AXValue".into());
            Ok(Some(StringishAttrValue { string_value: Some("value".into()), state_value: "value".into() }))
        }
        fn expired(&self) -> bool { self.expired }
        fn classified(&mut self) { self.trace.push("classified".into()); self.expired = self.expire_on_classification; }
    }

    #[test]
    fn axvalue_is_never_requested_before_classification() {
        let mut reader = FakeReader::new();
        read_with(AttrStatus::Known("AXTextField".into()), &mut reader, &mut false).unwrap();
        assert_eq!(reader.trace, ["AXRole", "batch:AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole", "classified", "AXValue"]);
    }

    #[test]
    fn a_failed_role_read_never_reads_the_value() {
        let mut reader = FakeReader::new();
        let attrs = read_with(AttrStatus::Failed, &mut reader, &mut false).unwrap();
        assert!(attrs.read_failed);
        assert_eq!(attrs.value, None);
        assert!(!reader.trace.iter().any(|name| name == "AXValue"), "{:?}", reader.trace);
    }

    #[test]
    fn a_secure_subrole_field_shows_redacted_without_reading() {
        let mut reader = FakeReader::new();
        reader.attrs[5] = AttrStatus::Known("AXSecureTextField".into());
        let result = read_with(AttrStatus::Known("AXTextField".into()), &mut reader, &mut false).unwrap();
        assert_eq!(result.value.as_deref(), Some("[redacted]"));
        assert!(result.secure);
        assert!(!reader.trace.iter().any(|s| s == "AXValue"));
    }

    #[test]
    fn a_password_labelled_field_shows_redacted_without_reading() {
        let mut reader = FakeReader::new();
        reader.attrs[1] = AttrStatus::Known("Password".into());
        let result = read_with(AttrStatus::Known("AXTextField".into()), &mut reader, &mut false).unwrap();
        assert_eq!(result.value.as_deref(), Some("[redacted]"));
        assert!(!reader.trace.iter().any(|s| s == "AXValue"));
    }

    #[test]
    fn a_failed_hint_withholds_the_value() {
        for i in 0..6 {
            let mut reader = FakeReader::new();
            reader.attrs[i] = AttrStatus::Failed;
            let result = read_with(AttrStatus::Known("AXTextField".into()), &mut reader, &mut false).unwrap();
            assert!(result.read_failed);
            assert_eq!(result.value, None);
            assert!(!reader.trace.iter().any(|s| s == "AXValue"));
        }
    }

    #[test]
    fn absent_hints_still_allow_ordinary_values() {
        let mut reader = FakeReader::new();
        let result = read_with(AttrStatus::Known("AXTextField".into()), &mut reader, &mut false).unwrap();
        assert_eq!(result.value.as_deref(), Some("value"));
        assert!(!result.read_failed);
    }

    #[test]
    fn whole_batch_rejection_replays_singles_without_value() {
        let mut reader = FakeReader::new();
        reader.reject_batch = true;
        let mut disabled = false;
        read_with(AttrStatus::Known("AXTextField".into()), &mut reader, &mut disabled).unwrap();
        assert!(disabled);
        assert_eq!(&reader.trace[2..], ["AXTitle", "AXPlaceholderValue", "AXDescription", "AXIdentifier", "AXHelp", "AXSubrole", "classified", "AXValue"]);
    }

    #[test]
    fn disabled_batching_order_is_value_last() {
        let mut reader = FakeReader::new();
        read_with(AttrStatus::Known("AXButton".into()), &mut reader, &mut true).unwrap();
        assert_eq!(&reader.trace[1..], ["AXTitle", "AXPlaceholderValue", "AXDescription", "AXIdentifier", "AXHelp", "AXSubrole", "classified", "AXValue"]);
    }

    #[test]
    fn deadline_between_phases_reads_no_value() {
        let mut reader = FakeReader::new();
        reader.expire_on_classification = true;
        assert!(read_with(AttrStatus::Known("AXTextField".into()), &mut reader, &mut false).is_none());
        assert!(!reader.trace.iter().any(|s| s == "AXValue"));
    }

    #[test]
    fn subrole_is_read_once_per_node() {
        let mut reader = FakeReader::new();
        read_with(AttrStatus::Known("AXButton".into()), &mut reader, &mut false).unwrap();
        assert_eq!(reader.trace.iter().map(|line| line.matches("AXSubrole").count()).sum::<usize>(), 1);
    }

    #[test]
    fn listed_absence_after_generic_failure_still_allows_ordinary_values() {
        let absent = single_read_outcome::<String>(kAXErrorFailure, None, || Some(false));
        assert_eq!(absent, Ok(None));
        let mut reader = FakeReader::new();
        reader.attrs[2] = match absent { Ok(None) => AttrStatus::Absent, _ => AttrStatus::Failed };
        let result = read_with(AttrStatus::Known("AXTextField".into()), &mut reader, &mut false).unwrap();
        assert_eq!(result.value.as_deref(), Some("value"));
    }

    #[test]
    fn happy_path_requests_value_after_one_classification_batch_for_each_role() {
        for role in ["AXButton", "AXButton", "AXTextField", "AXStaticText"] {
            let mut reader = FakeReader::new();
            read_with(AttrStatus::Known(role.into()), &mut reader, &mut false).unwrap();
            assert_eq!(reader.trace, ["AXRole", "batch:AXTitle,AXPlaceholderValue,AXDescription,AXIdentifier,AXHelp,AXSubrole", "classified", "AXValue"]);
        }
    }

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
        // (batched reads never fail a slot: a real slot error falls back to
        // single reads, below)
        for value in ["", " \t\n "] {
            let attrs = content(&CFString::new(value).as_CFType(), "  placeholder  ");
            assert_eq!(attrs.value.as_deref(), Some("  placeholder  "));
            assert_eq!(attrs.copied_value.unwrap().state_value, value);
        }
    }

    /// Review round 2: a single read that fails with a real error (a timeout,
    /// CannotComplete) is not an absent label. The node says so, so the walk
    /// cannot claim its elements complete.
    #[test]
    fn a_failed_single_read_is_not_an_absent_label() {
        let failed = ContentAttrs::from_single_reads(
            |name| match name {
                "AXTitle" => Err(kAXErrorCannotComplete),
                "AXDescription" => Ok(Some("Save".into())),
                _ => Ok(None),
            },
            || Ok(None),
        );
        assert!(failed.read_failed);
        assert_eq!(failed.title, None);
        assert_eq!(failed.description.as_deref(), Some("Save"));

        let absent = ContentAttrs::from_single_reads(|_| Ok(None), || Ok(None));
        assert!(!absent.read_failed);

        let value_failed =
            ContentAttrs::from_single_reads(|_| Ok(None), || Err(kAXErrorCannotComplete));
        assert!(value_failed.read_failed);
    }

    #[test]
    fn only_real_errors_fail_a_single_read() {
        let never = || -> Option<bool> { panic!("a definite answer needs no name list") };
        for absent in [
            kAXErrorSuccess,
            kAXErrorNoValue,
            kAXErrorAttributeUnsupported,
        ] {
            assert_eq!(single_read_outcome::<String>(absent, None, never), Ok(None));
        }
        // The app did not answer or the element went away: content unknown.
        for failed in [
            kAXErrorCannotComplete,
            kAXErrorInvalidUIElement,
            kAXErrorAPIDisabled,
        ] {
            assert_eq!(
                single_read_outcome::<String>(failed, None, never),
                Err(failed)
            );
        }
        // Review round 4: a generic failure proves absence only when the
        // element does not list the attribute.
        for generic in [
            kAXErrorFailure,
            kAXErrorIllegalArgument,
            kAXErrorNotImplemented,
        ] {
            assert_eq!(
                single_read_outcome::<String>(generic, None, || Some(false)),
                Ok(None)
            );
            assert_eq!(
                single_read_outcome::<String>(generic, None, || Some(true)),
                Err(generic)
            );
            assert_eq!(
                single_read_outcome::<String>(generic, None, || None),
                Err(generic)
            );
        }
        // Review round 3: a read that succeeds keeps its value.
        assert_eq!(
            single_read_outcome(kAXErrorSuccess, Some("Save"), never),
            Ok(Some("Save"))
        );
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
    /// Review round 4: a failed read matters only where the label would come
    /// from it. With a title present, a failed description hides nothing.
    #[test]
    fn a_failed_read_after_the_label_hides_nothing() {
        let with_title = ContentAttrs::from_single_reads(
            |name| match name {
                "AXTitle" => Ok(Some("Ruler".into())),
                "AXDescription" => Err(kAXErrorFailure),
                _ => Ok(None),
            },
            || Ok(None),
        );
        assert!(!with_title.read_failed);
        assert_eq!(with_title.title.as_deref(), Some("Ruler"));

        let untitled = ContentAttrs::from_single_reads(
            |name| match name {
                "AXDescription" => Err(kAXErrorFailure),
                "AXIdentifier" => Ok(Some("marker".into())),
                _ => Ok(None),
            },
            || Ok(None),
        );
        assert!(untitled.read_failed, "the description would be the label");

        let help_failed = ContentAttrs::from_single_reads(
            |name| match name {
                "AXHelp" => Err(kAXErrorCannotComplete),
                _ => Ok(None),
            },
            || Ok(None),
        );
        assert!(!help_failed.read_failed, "help is not part of the label");

        let placeholder_failed = ContentAttrs::from_single_reads(
            |name| match name {
                "AXPlaceholderValue" => Err(kAXErrorCannotComplete),
                _ => Ok(None),
            },
            || Ok(None),
        );
        assert!(placeholder_failed.read_failed);
    }
}
