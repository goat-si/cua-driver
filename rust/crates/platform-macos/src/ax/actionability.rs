//! State-preserving, content-free accessibility reads for actionability.
use super::bindings::*;
use core_foundation::{
    array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef},
    base::{CFGetTypeID, CFRelease, CFTypeRef, TCFType},
    boolean::CFBoolean,
    string::CFString,
};
use cua_driver_core::actionability::{Evidence, Rect};

pub(crate) trait Reader {
    type Element: Copy;
    fn boolean(&mut self, element: Self::Element, attribute: &str) -> Evidence<bool>;
    fn string(&mut self, element: Self::Element, attribute: &str) -> Evidence<String>;
    fn actions(&mut self, element: Self::Element) -> Evidence<Vec<String>>;
    fn frame(&mut self, element: Self::Element) -> Evidence<Rect>;
    fn settable(&mut self, element: Self::Element, attribute: &str) -> Evidence<bool>;
}

/// A native reader; callers must provide a live retained element for every read.
pub(crate) struct NativeReader;

#[cfg(test)]
fn forbid_live_tests() {
    panic!("live AX access is forbidden in tests");
}
#[cfg(not(test))]
fn forbid_live_tests() {}

fn error<T>(code: AXError) -> Evidence<T> {
    if code == kAXErrorAttributeUnsupported || code == kAXErrorNoValue {
        Evidence::Absent
    } else {
        Evidence::Unknown(format!("AX error {code}"))
    }
}

pub(crate) fn classify_attribute<T>(
    code: AXError,
    present: bool,
    decoded: Option<T>,
    name: &str,
) -> Evidence<T> {
    if code != kAXErrorSuccess {
        error(code)
    } else if !present {
        Evidence::Absent
    } else {
        decoded.map_or_else(
            || Evidence::Unknown(format!("wrong CF type for {name}")),
            Evidence::Known,
        )
    }
}

unsafe fn attr<T>(
    element: AXUIElementRef,
    name: &str,
    decode: impl FnOnce(CFTypeRef) -> Option<T>,
) -> Evidence<T> {
    assert!(
        name != "AXValue" && name != "AXSelectedText",
        "content reads are forbidden"
    );
    let attribute = CFString::new(name);
    let mut value: CFTypeRef = std::ptr::null();
    let code = AXUIElementCopyAttributeValue(element, attribute.as_concrete_TypeRef(), &mut value);
    let decoded = if code == kAXErrorSuccess && !value.is_null() {
        decode(value)
    } else {
        None
    };
    let result = classify_attribute(code, !value.is_null(), decoded, name);
    if !value.is_null() {
        CFRelease(value);
    }
    result
}

impl Reader for NativeReader {
    type Element = AXUIElementRef;
    fn boolean(&mut self, element: Self::Element, attribute: &str) -> Evidence<bool> {
        forbid_live_tests();
        unsafe {
            attr(element, attribute, |v| {
                (CFGetTypeID(v) == CFBoolean::type_id())
                    .then(|| CFBoolean::wrap_under_get_rule(v as _).into())
            })
        }
    }
    fn string(&mut self, element: Self::Element, attribute: &str) -> Evidence<String> {
        forbid_live_tests();
        assert_eq!(attribute, "AXRole", "only target role can be read");
        unsafe { attr(element, attribute, |v| string_from_cf(v)) }
    }
    fn actions(&mut self, element: Self::Element) -> Evidence<Vec<String>> {
        forbid_live_tests();
        unsafe {
            let mut names: CFArrayRef = std::ptr::null();
            let code = AXUIElementCopyActionNames(element, &mut names);
            if code != kAXErrorSuccess {
                if !names.is_null() {
                    CFRelease(names as CFTypeRef);
                }
                return error(code);
            }
            if names.is_null() {
                return Evidence::Absent;
            }
            if CFGetTypeID(names as CFTypeRef)
                != core_foundation::array::CFArray::<CFTypeRef>::type_id()
            {
                CFRelease(names as CFTypeRef);
                return Evidence::Unknown("wrong CF type for action names".into());
            }
            let mut actions = Vec::new();
            for i in 0..CFArrayGetCount(names) {
                let value = CFArrayGetValueAtIndex(names, i) as CFTypeRef;
                let Some(action) = string_from_cf(value) else {
                    CFRelease(names as CFTypeRef);
                    return Evidence::Unknown("wrong CF type in action names".into());
                };
                actions.push(action);
            }
            CFRelease(names as CFTypeRef);
            Evidence::Known(actions)
        }
    }
    fn frame(&mut self, element: Self::Element) -> Evidence<Rect> {
        forbid_live_tests();
        let (position, size) = unsafe {
            (
                attr(element, "AXPosition", |v| point_from_cf(v)),
                attr(element, "AXSize", |v| size_from_cf(v)),
            )
        };
        match (position, size) {
            (Evidence::Unknown(e), _) | (_, Evidence::Unknown(e)) => Evidence::Unknown(e),
            (Evidence::Known((x, y)), Evidence::Known((width, height))) => Evidence::Known(Rect {
                x,
                y,
                width,
                height,
            }),
            _ => Evidence::Absent,
        }
    }
    fn settable(&mut self, element: Self::Element, attribute: &str) -> Evidence<bool> {
        forbid_live_tests();
        unsafe {
            let name = CFString::new(attribute);
            let mut value = 0_u8;
            let code =
                AXUIElementIsAttributeSettable(element, name.as_concrete_TypeRef(), &mut value);
            if code == kAXErrorSuccess {
                Evidence::Known(value != 0)
            } else {
                error(code)
            }
        }
    }
}

pub(crate) fn observe<T>(evidence: Evidence<T>) -> Evidence<T> {
    evidence
}
