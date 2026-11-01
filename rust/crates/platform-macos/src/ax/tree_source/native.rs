use super::{AxTreeSource, NativeBatch, NativeElement, Read, ReadFailure};
use crate::ax::batch_attrs::{copy_attrs_batch, BatchFailure, BatchRead};
use crate::ax::bindings::*;
use crate::ax::content_attrs::{read_string_status, AttrStatus};
use crate::ax::write_outcome::{query_settable, Settable};
use core_foundation::{
    array::CFArray,
    base::{CFEqual, CFGetTypeID, CFHash, CFRelease, CFRetain, CFTypeRef, TCFType},
    string::CFString,
};
use std::collections::HashMap;

pub(crate) struct NativeSource;

/// The original copied array remains the sole owner of its element slots.
pub(crate) struct NativeElementSet {
    _array: CFArray<CFTypeRef>,
    buckets: HashMap<usize, Vec<CFTypeRef>>,
}

impl NativeElement {
    fn raw(&self) -> AXUIElementRef {
        self.0.as_ptr()
    }
}

// Preserve the existing scalar helpers' one request and release behavior.
unsafe fn scalar<T>(
    element: AXUIElementRef,
    name: &str,
    decode: unsafe fn(CFTypeRef) -> Option<T>,
) -> Read<T> {
    let attr = CFString::new(name);
    let mut value = std::ptr::null();
    let error = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if error != kAXErrorSuccess {
        return if [kAXErrorNoValue, kAXErrorAttributeUnsupported].contains(&error) {
            Read::Absent
        } else {
            Read::Unknown(ReadFailure::Ax(error))
        };
    }
    if value.is_null() {
        return Read::Absent;
    }
    let decoded = decode(value);
    CFRelease(value);
    decoded.map_or(Read::Unknown(ReadFailure::WrongType), Read::Known)
}

impl AxTreeSource for NativeSource {
    type Element = NativeElement;
    type Batch = NativeBatch;
    type ElementSet = NativeElementSet;

    fn key(&self, element: &NativeElement) -> usize {
        element.raw() as usize
    }
    fn set_messaging_timeout(&self, element: &NativeElement) {
        unsafe { crate::ax::tree::set_messaging_timeout(element.raw()) }
    }
    fn text(&self, element: &NativeElement, name: &str) -> Read<String> {
        unsafe { scalar(element.raw(), name, string_from_cf) }
    }
    fn text_status(&self, element: &NativeElement, name: &str) -> AttrStatus {
        unsafe { read_string_status(element.raw(), name) }
    }
    fn boolean(&self, element: &NativeElement, name: &str) -> Read<bool> {
        unsafe { scalar(element.raw(), name, bool_from_cf) }
    }
    fn number(&self, element: &NativeElement, name: &str) -> Read<f64> {
        unsafe { scalar(element.raw(), name, number_from_cf) }
    }
    fn position(&self, element: &NativeElement) -> Read<(f64, f64)> {
        unsafe { scalar(element.raw(), "AXPosition", point_from_cf) }
    }
    fn size(&self, element: &NativeElement) -> Read<(f64, f64)> {
        unsafe { scalar(element.raw(), "AXSize", size_from_cf) }
    }
    fn value(&self, element: &NativeElement) -> Result<Option<StringishAttrValue>, AXError> {
        unsafe {
            crate::ax::content_attrs::single_read(element.raw(), "AXValue", |value| {
                stringish_from_cf(value)
            })
        }
    }
    fn batch(&self, element: &NativeElement, names: &[&str]) -> Result<NativeBatch, BatchFailure> {
        match unsafe { copy_attrs_batch(element.raw(), names) } {
            BatchRead::Slots(slots) => Ok(NativeBatch(slots)),
            BatchRead::Fallback(error) => Err(error),
        }
    }
    fn children(&self, element: &NativeElement) -> Read<Vec<NativeElement>> {
        unsafe {
            let attr = CFString::new("AXChildren");
            let mut value = std::ptr::null();
            let error = AXUIElementCopyAttributeValue(
                element.raw(),
                attr.as_concrete_TypeRef(),
                &mut value,
            );
            if !children_read_succeeded(error) {
                return Read::Unknown(ReadFailure::Ax(error));
            }
            if error != kAXErrorSuccess || value.is_null() {
                return Read::Absent;
            }
            if CFGetTypeID(value) != CFArray::<CFTypeRef>::type_id() {
                CFRelease(value);
                return Read::Unknown(ReadFailure::WrongType);
            }
            let array = CFArray::<CFTypeRef>::wrap_under_create_rule(value as _);
            let ax_type = AXUIElementGetTypeID();
            let children = (0..array.len())
                .filter_map(|i| {
                    let item = *array.get(i)?;
                    if CFGetTypeID(item) != ax_type {
                        return None;
                    }
                    CFRetain(item);
                    Some(NativeElement::from_owned(item as AXUIElementRef))
                })
                .collect();
            Read::Known(children)
        }
    }
    fn element_attribute(&self, element: &NativeElement, name: &str) -> Read<NativeElement> {
        unsafe {
            let attr = CFString::new(name);
            let mut value = std::ptr::null();
            let error = AXUIElementCopyAttributeValue(
                element.raw(),
                attr.as_concrete_TypeRef(),
                &mut value,
            );
            if error != kAXErrorSuccess {
                return if [kAXErrorNoValue, kAXErrorAttributeUnsupported].contains(&error) {
                    Read::Absent
                } else {
                    Read::Unknown(ReadFailure::Ax(error))
                };
            }
            if value.is_null() {
                return Read::Absent;
            }
            if CFGetTypeID(value) != AXUIElementGetTypeID() {
                CFRelease(value);
                return Read::Unknown(ReadFailure::WrongType);
            }
            Read::Known(NativeElement::from_owned(value as AXUIElementRef))
        }
    }
    fn element_set(&self, element: &NativeElement, name: &str) -> Read<NativeElementSet> {
        unsafe {
            let attr = CFString::new(name);
            let mut value = std::ptr::null();
            let error = AXUIElementCopyAttributeValue(
                element.raw(),
                attr.as_concrete_TypeRef(),
                &mut value,
            );
            if error != kAXErrorSuccess {
                return if [kAXErrorNoValue, kAXErrorAttributeUnsupported].contains(&error) {
                    Read::Absent
                } else {
                    Read::Unknown(ReadFailure::Ax(error))
                };
            }
            if value.is_null() {
                return Read::Absent;
            }
            if CFGetTypeID(value) != CFArray::<CFTypeRef>::type_id() {
                CFRelease(value);
                return Read::Unknown(ReadFailure::WrongType);
            }
            let array = CFArray::<CFTypeRef>::wrap_under_create_rule(value as _);
            let mut buckets: HashMap<usize, Vec<CFTypeRef>> = HashMap::new();
            for item in array.iter() {
                if CFGetTypeID(*item) == AXUIElementGetTypeID() {
                    buckets.entry(CFHash(*item)).or_default().push(*item);
                }
            }
            if buckets.is_empty() {
                Read::Absent
            } else {
                Read::Known(NativeElementSet {
                    _array: array,
                    buckets,
                })
            }
        }
    }
    fn set_contains(&self, set: &NativeElementSet, element: &NativeElement) -> bool {
        unsafe {
            let raw = element.raw() as CFTypeRef;
            set.buckets
                .get(&CFHash(raw))
                .is_some_and(|bucket| bucket.iter().any(|&other| CFEqual(raw, other) != 0))
        }
    }
    fn action_names(&self, element: &NativeElement) -> Read<Vec<String>> {
        unsafe {
            let mut names = std::ptr::null();
            let error = AXUIElementCopyActionNames(element.raw(), &mut names);
            if error != kAXErrorSuccess {
                return Read::Unknown(ReadFailure::Ax(error));
            }
            if names.is_null() {
                return Read::Absent;
            }
            let names = CFArray::<CFString>::wrap_under_create_rule(names);
            Read::Known(
                (0..names.len())
                    .filter_map(|i| names.get(i).map(|s| s.to_string()))
                    .collect(),
            )
        }
    }
    fn value_settable(&self, element: &NativeElement) -> bool {
        unsafe { is_attribute_settable(element.raw(), "AXValue") }
    }
    fn selected_settable(&self, element: &NativeElement) -> Settable {
        unsafe { query_settable(element.raw(), "AXSelected") }
    }
    fn frame(&self, element: &NativeElement) -> Read<[f64; 4]> {
        let position = match self.position(element) {
            Read::Known(value) => value,
            Read::Absent => return Read::Absent,
            Read::Unknown(error) => return Read::Unknown(error),
        };
        match self.size(element) {
            Read::Known(size) => {
                rect_from_parts(Some(position), Some(size)).map_or(Read::Absent, Read::Known)
            }
            Read::Absent => Read::Absent,
            Read::Unknown(error) => Read::Unknown(error),
        }
    }
    fn retain_for_snapshot(&self, element: &NativeElement) -> usize {
        unsafe { CFRetain(element.raw() as CFTypeRef) };
        element.raw() as usize
    }
}

/// Temporarily borrow the caller's existing retain credit without changing it.
/// The caller keeps the element alive throughout `f`, including unwinding.
pub(crate) unsafe fn with_borrowed<R>(
    raw: AXUIElementRef,
    f: impl FnOnce(&NativeSource, &NativeElement) -> R,
) -> R {
    let borrowed = std::mem::ManuallyDrop::new(NativeElement::from_owned(raw));
    f(&NativeSource, &borrowed)
}
