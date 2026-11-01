use crate::ax::bindings::{
    self, kAXErrorAttributeUnsupported, kAXErrorNoValue, kAXErrorSuccess, AXError,
    AXUIElementCopyAttributeValue, AXUIElementGetTypeID, AXUIElementRef,
};
use core_foundation::{
    array::CFArray,
    base::{CFGetTypeID, CFRelease, CFRetain, CFTypeRef, TCFType},
    string::CFString,
};

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Read<T> {
    Present(T),
    Absent,
    Failed(AXError),
    WrongType,
}

unsafe fn read<T>(
    element: AXUIElementRef,
    name: &str,
    convert: impl FnOnce(CFTypeRef) -> Option<T>,
) -> Read<T> {
    let attr = CFString::new(name);
    let mut value: CFTypeRef = std::ptr::null();
    let error = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if error != kAXErrorSuccess {
        if !value.is_null() {
            CFRelease(value);
        }
        return if error == kAXErrorNoValue || error == kAXErrorAttributeUnsupported {
            Read::Absent
        } else {
            Read::Failed(error)
        };
    }
    if value.is_null() {
        return Read::WrongType;
    }
    let result = convert(value).map_or(Read::WrongType, Read::Present);
    CFRelease(value);
    result
}

pub(super) unsafe fn element(node: AXUIElementRef, name: &str) -> Read<AXUIElementRef> {
    read(node, name, |value| {
        if CFGetTypeID(value) != AXUIElementGetTypeID() {
            return None;
        }
        CFRetain(value);
        Some(value as AXUIElementRef)
    })
}
pub(super) unsafe fn children(node: AXUIElementRef) -> Read<Vec<AXUIElementRef>> {
    read(node, "AXChildren", |value| {
        if CFGetTypeID(value) != CFArray::<CFTypeRef>::type_id() {
            return None;
        }
        let array = CFArray::<CFTypeRef>::wrap_under_get_rule(value as _);
        let mut items = Vec::with_capacity(array.len() as usize);
        for index in 0..array.len() {
            let item = match array.get(index) {
                Some(value) => *value,
                None => {
                    for retained in items {
                        CFRelease(retained as CFTypeRef);
                    }
                    return None;
                }
            };
            if item.is_null() || CFGetTypeID(item) != AXUIElementGetTypeID() {
                for retained in items {
                    CFRelease(retained as CFTypeRef);
                }
                return None;
            }
            CFRetain(item);
            items.push(item as AXUIElementRef);
        }
        Some(items)
    })
}
pub(super) unsafe fn string(node: AXUIElementRef, name: &str) -> Read<String> {
    read(node, name, |value| bindings::string_from_cf(value))
}
pub(super) unsafe fn number(node: AXUIElementRef, name: &str) -> Read<f64> {
    read(node, name, |value| bindings::number_from_cf(value))
}
pub(super) unsafe fn boolean(node: AXUIElementRef, name: &str) -> Read<bool> {
    read(node, name, |value| {
        use core_foundation::boolean::CFBoolean;
        if CFGetTypeID(value) == CFBoolean::type_id() {
            return bindings::bool_from_cf(value);
        }
        match bindings::number_from_cf(value) {
            Some(0.0) => Some(false),
            Some(1.0) => Some(true),
            _ => None,
        }
    })
}
