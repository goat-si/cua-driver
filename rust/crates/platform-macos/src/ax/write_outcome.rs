//! Evidence from AX attribute writes and value reads.
use super::bindings::*;
use core_foundation::{
    base::{CFRelease, CFTypeRef, TCFType},
    string::CFString,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttrWrite {
    Applied,
    Rejected,
    Uncertain,
}

/// Rejected means the client or app rejected the value, not that no side
/// effect occurred. We assume a control rejecting a value can receive the same
/// value once in another representation. Apple includes values an app does not
/// recognize in IllegalArgument.
pub(crate) fn classify_attr_write(err: AXError) -> AttrWrite {
    if err == kAXErrorSuccess {
        AttrWrite::Applied
    } else if [
        kAXErrorIllegalArgument,
        kAXErrorInvalidUIElement,
        kAXErrorAPIDisabled,
    ]
    .contains(&err)
    {
        AttrWrite::Rejected
    } else {
        AttrWrite::Uncertain
    }
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ValueObs {
    Number(f64),
    Text(String),
    Missing,
    Failed(AXError),
    OtherType,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ValueRead {
    Allowed(ValueObs),
    Withheld,
}

impl ValueRead {
    pub(crate) fn text(self) -> Option<String> {
        match self {
            Self::Allowed(ValueObs::Text(value)) => Some(value),
            _ => None,
        }
    }
}

pub(crate) trait DirectValueReader: super::label_reader::LabelReader {
    fn value(&mut self, element: Self::Element) -> ValueObs;
}

impl DirectValueReader for super::label_reader::NativeLabelReader {
    fn value(&mut self, element: Self::Element) -> ValueObs {
        unsafe { read_value_typed(element) }
    }
}

pub(crate) fn read_value_if_allowed<R: DirectValueReader>(
    reader: &mut R,
    element: R::Element,
) -> ValueRead {
    ValueRead::Allowed(reader.value(element))
}

pub(crate) unsafe fn read_text_value(element: AXUIElementRef) -> Option<String> {
    read_text_value_with(&mut super::label_reader::NativeLabelReader, element)
}

pub(crate) fn read_text_value_with<R: DirectValueReader>(
    reader: &mut R,
    element: R::Element,
) -> Option<String> {
    read_value_if_allowed(reader, element).text()
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Settable {
    Yes,
    No,
    Unknown(AXError),
}

fn read_error(err: AXError) -> Option<ValueObs> {
    if err == kAXErrorSuccess {
        None
    } else if err == kAXErrorNoValue {
        Some(ValueObs::Missing)
    } else {
        Some(ValueObs::Failed(err))
    }
}

pub(crate) unsafe fn read_value_typed(element: AXUIElementRef) -> ValueObs {
    let attr = CFString::new("AXValue");
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if let Some(obs) = read_error(err) {
        if !value.is_null() {
            CFRelease(value);
        }
        return obs;
    }
    if value.is_null() {
        return ValueObs::OtherType;
    }
    let obs = if let Some(n) = number_from_cf(value) {
        ValueObs::Number(n)
    } else if let Some(s) = string_from_cf(value) {
        ValueObs::Text(s)
    } else {
        ValueObs::OtherType
    };
    CFRelease(value);
    obs
}
pub(crate) unsafe fn query_settable(element: AXUIElementRef, name: &str) -> Settable {
    let attr = CFString::new(name);
    let mut value = 0;
    let err = AXUIElementIsAttributeSettable(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess {
        Settable::Unknown(err)
    } else if value == 0 {
        Settable::No
    } else {
        Settable::Yes
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classifier_preserves_uncertainty() {
        assert_eq!(classify_attr_write(kAXErrorSuccess), AttrWrite::Applied);
        for e in [
            kAXErrorIllegalArgument,
            kAXErrorInvalidUIElement,
            kAXErrorAPIDisabled,
        ] {
            assert_eq!(classify_attr_write(e), AttrWrite::Rejected);
        }
        for e in [
            kAXErrorCannotComplete,
            kAXErrorFailure,
            kAXErrorAttributeUnsupported,
        ] {
            assert_eq!(classify_attr_write(e), AttrWrite::Uncertain);
        }
    }
    #[test]
    fn the_reader_keeps_novalue_distinct_from_failure() {
        assert_eq!(read_error(kAXErrorNoValue), Some(ValueObs::Missing));
        assert_eq!(
            read_error(kAXErrorCannotComplete),
            Some(ValueObs::Failed(kAXErrorCannotComplete))
        );
        assert_eq!(read_error(kAXErrorSuccess), None);
    }
}
