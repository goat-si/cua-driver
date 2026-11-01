//! Evidence from AX attribute writes and value reads.
use super::bindings::*;
use core_foundation::{
    base::{CFRelease, CFTypeRef, TCFType},
    string::CFString,
};
use cua_driver_core::mutation::{Reason, WriteOutcome};

// U32B-D will connect this boundary to the macOS write consumers.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeWriteKind {
    SetAttribute,
    Perform,
}

// U32B-D will connect this boundary to the macOS write consumers.
#[allow(dead_code)]
pub(crate) fn native_write_outcome(
    kind: NativeWriteKind,
    call_ran: bool,
    pre_settable: &Settable,
    error: i32,
) -> WriteOutcome {
    use super::bindings::{
        kAXErrorAPIDisabled as API_DISABLED, kAXErrorActionUnsupported as ACTION_UNSUPPORTED,
        kAXErrorAttributeUnsupported as ATTRIBUTE_UNSUPPORTED,
        kAXErrorCannotComplete as CANNOT_COMPLETE, kAXErrorFailure as FAILURE,
        kAXErrorNoValue as NO_VALUE, kAXErrorNotImplemented as NOT_IMPLEMENTED,
    };

    if !call_ran {
        return if kind == NativeWriteKind::SetAttribute && matches!(pre_settable, Settable::No) {
            WriteOutcome::FallThrough(Reason::AttributeUnsupported)
        } else {
            WriteOutcome::StopNotDelivered(Reason::Unknown)
        };
    }
    if error == kAXErrorSuccess {
        return WriteOutcome::Delivered;
    }
    match (kind, error) {
        (_, API_DISABLED) => WriteOutcome::StopNotDelivered(Reason::PermissionDenied),
        (_, CANNOT_COMPLETE) => WriteOutcome::StopUncertain(Reason::CannotComplete),
        (_, FAILURE) => WriteOutcome::StopUncertain(Reason::Failure),
        (NativeWriteKind::Perform, ATTRIBUTE_UNSUPPORTED) => {
            WriteOutcome::StopUncertain(Reason::AttributeUnsupported)
        }
        (NativeWriteKind::Perform, NO_VALUE) => WriteOutcome::StopUncertain(Reason::NoValue),
        (NativeWriteKind::Perform, ACTION_UNSUPPORTED) => {
            WriteOutcome::FallThrough(Reason::Unsupported)
        }
        (NativeWriteKind::Perform, NOT_IMPLEMENTED) => {
            WriteOutcome::FallThrough(Reason::NotImplemented)
        }
        _ => WriteOutcome::StopUncertain(Reason::Unknown),
    }
}

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
    if super::content_attrs::classify_value_read(reader, element) {
        ValueRead::Allowed(reader.value(element))
    } else {
        ValueRead::Withheld
    }
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

#[cfg(test)]
mod u32_native_write_tests {
    use super::*;
    use cua_driver_core::mutation::WriteChain;

    const KINDS: [NativeWriteKind; 2] = [NativeWriteKind::SetAttribute, NativeWriteKind::Perform];
    const SETTABLES: [Settable; 3] = [Settable::Yes, Settable::No, Settable::Unknown(-25211)];
    const CODES: [i32; 19] = [
        0,
        -25200,
        -25201,
        -25202,
        -25203,
        -25204,
        -25205,
        -25206,
        -25207,
        -25208,
        -25209,
        -25210,
        -25211,
        -25212,
        -25213,
        -25214,
        -1,
        i32::MIN,
        i32::MAX,
    ];

    fn check_returned(codes: &[i32], expected: impl Fn(NativeWriteKind, i32) -> WriteOutcome) {
        for &code in codes {
            for kind in KINDS {
                for settable in SETTABLES {
                    assert_eq!(
                        native_write_outcome(kind, true, &settable, code),
                        expected(kind, code),
                        "kind={kind:?}, settable={settable:?}, code={code}"
                    );
                }
            }
        }
    }

    #[test]
    fn returned_success_is_delivered_for_both_write_kinds() {
        check_returned(&[0], |_, _| WriteOutcome::Delivered);
    }

    #[test]
    fn returned_illegal_argument_without_nonapplication_proof_is_uncertain() {
        check_returned(&[-25201], |_, _| {
            WriteOutcome::StopUncertain(Reason::Unknown)
        });
    }

    #[test]
    fn returned_invalid_element_without_nonapplication_proof_is_uncertain() {
        check_returned(&[-25202], |_, _| {
            WriteOutcome::StopUncertain(Reason::Unknown)
        });
    }

    #[test]
    fn returned_setter_attribute_errors_ignore_preflight_settable_and_stop_uncertain() {
        for code in [-25205, -25212] {
            for settable in SETTABLES {
                assert_eq!(
                    native_write_outcome(NativeWriteKind::SetAttribute, true, &settable, code),
                    WriteOutcome::StopUncertain(Reason::Unknown)
                );
            }
        }
    }

    #[test]
    fn returned_perform_attribute_errors_are_uncertain_without_effect_evidence() {
        for (code, reason) in [
            (-25205, Reason::AttributeUnsupported),
            (-25212, Reason::NoValue),
        ] {
            for settable in SETTABLES {
                assert_eq!(
                    native_write_outcome(NativeWriteKind::Perform, true, &settable, code),
                    WriteOutcome::StopUncertain(reason)
                );
            }
        }
    }

    #[test]
    fn unsupported_and_not_implemented_follow_the_frozen_kind_table() {
        check_returned(&[-25206, -25208], |kind, code| match kind {
            NativeWriteKind::SetAttribute => WriteOutcome::StopUncertain(Reason::Unknown),
            NativeWriteKind::Perform => WriteOutcome::FallThrough(if code == -25206 {
                Reason::Unsupported
            } else {
                Reason::NotImplemented
            }),
        });
    }

    #[test]
    fn api_disabled_proves_non_delivery() {
        check_returned(&[-25211], |_, _| {
            WriteOutcome::StopNotDelivered(Reason::PermissionDenied)
        });
    }

    #[test]
    fn failure_cannot_complete_and_other_codes_stop_uncertain() {
        check_returned(
            &[
                -25200,
                -25204,
                -25203,
                -25207,
                -25209,
                -25210,
                -25213,
                -25214,
                -1,
                i32::MIN,
                i32::MAX,
            ],
            |_, code| {
                WriteOutcome::StopUncertain(match code {
                    -25200 => Reason::Failure,
                    -25204 => Reason::CannotComplete,
                    _ => Reason::Unknown,
                })
            },
        );
    }

    #[test]
    fn uncalled_nonsettable_setter_falls_through_without_fake_error() {
        for code in CODES {
            assert_eq!(
                native_write_outcome(NativeWriteKind::SetAttribute, false, &Settable::No, code),
                WriteOutcome::FallThrough(Reason::AttributeUnsupported)
            );
        }
    }

    #[test]
    fn uncalled_other_contexts_stop_without_input() {
        for code in CODES {
            for kind in KINDS {
                for settable in SETTABLES {
                    if kind == NativeWriteKind::SetAttribute && settable == Settable::No {
                        continue;
                    }
                    let outcome = native_write_outcome(kind, false, &settable, code);
                    assert_eq!(outcome, WriteOutcome::StopNotDelivered(Reason::Unknown));
                    assert!(WriteChain::run([outcome], false).no_input());
                }
            }
        }
    }

    #[test]
    fn uncalled_error_code_cannot_override_call_evidence() {
        for code in CODES {
            for kind in KINDS {
                for settable in SETTABLES {
                    let expected =
                        if kind == NativeWriteKind::SetAttribute && settable == Settable::No {
                            WriteOutcome::FallThrough(Reason::AttributeUnsupported)
                        } else {
                            WriteOutcome::StopNotDelivered(Reason::Unknown)
                        };
                    assert_eq!(
                        native_write_outcome(kind, false, &settable, code),
                        expected,
                        "kind={kind:?}, settable={settable:?}, code={code}"
                    );
                }
            }
        }
    }

    #[test]
    fn settable_query_error_is_not_classified_as_write_error() {
        for query_error in CODES {
            for kind in KINDS {
                for code in CODES {
                    let unknown = Settable::Unknown(query_error);
                    assert_eq!(
                        native_write_outcome(kind, false, &unknown, code),
                        WriteOutcome::StopNotDelivered(Reason::Unknown)
                    );
                    assert_eq!(
                        native_write_outcome(kind, true, &unknown, code),
                        native_write_outcome(kind, true, &Settable::Yes, code)
                    );
                }
            }
        }
    }

    #[test]
    fn chain_no_input_preserves_side_effectful_prologue() {
        let delivered = native_write_outcome(NativeWriteKind::Perform, true, &Settable::Yes, 0);
        assert_eq!(delivered, WriteOutcome::Delivered);
        for code in CODES {
            for kind in KINDS {
                for settable in SETTABLES {
                    let outcome = native_write_outcome(kind, false, &settable, code);
                    assert!(WriteChain::run([outcome], false).no_input());
                    assert!(
                        !WriteChain::run([outcome], delivered == WriteOutcome::Delivered)
                            .no_input()
                    );
                }
            }
        }
    }
}
