//! Internal source boundary for the accessibility tree walker.
//!
//! The walker still uses the native readers until the adapters are installed.
#![allow(dead_code)]

use super::batch_attrs::{BatchFailure, BatchSlots};
use super::bindings::{
    bool_from_cf, number_from_cf, point_from_cf, size_from_cf, AXError, AXUIElementRef,
    StringishAttrValue,
};
use super::content_attrs::AttrStatus;
use super::write_outcome::Settable;
use core_foundation::base::{CFRelease, CFRetain, CFTypeRef};
use std::ptr::NonNull;

pub(super) mod native;

pub(crate) enum Read<T> {
    Known(T),
    Absent,
    Unknown(ReadFailure),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadFailure {
    Ax(AXError),
    WrongType,
}

pub(crate) trait TreeBatch {
    fn text(&self, slot: usize) -> AttrStatus;
    fn boolean(&self, slot: usize) -> Read<bool>;
    fn number(&self, slot: usize) -> Read<f64>;
    fn position(&self, slot: usize) -> Read<(f64, f64)>;
    fn size(&self, slot: usize) -> Read<(f64, f64)>;
}

/// The copied batch array owns all slots until decoding is complete.
pub(crate) struct NativeBatch(pub(super) BatchSlots);

impl NativeBatch {
    fn decode<T>(&self, slot: usize, f: unsafe fn(CFTypeRef) -> Option<T>) -> Read<T> {
        match self.0.raw(slot) {
            None => Read::Absent,
            Some(value) => match unsafe { f(value) } {
                Some(value) => Read::Known(value),
                None => Read::Unknown(ReadFailure::WrongType),
            },
        }
    }
}

impl TreeBatch for NativeBatch {
    fn text(&self, slot: usize) -> AttrStatus {
        super::content_attrs::string_slot(&self.0, slot)
    }

    fn boolean(&self, slot: usize) -> Read<bool> {
        self.decode(slot, bool_from_cf)
    }

    fn number(&self, slot: usize) -> Read<f64> {
        self.decode(slot, number_from_cf)
    }

    fn position(&self, slot: usize) -> Read<(f64, f64)> {
        self.decode(slot, point_from_cf)
    }

    fn size(&self, slot: usize) -> Read<(f64, f64)> {
        self.decode(slot, size_from_cf)
    }
}

pub(crate) trait AxTreeSource {
    type Element: Clone;
    type Batch: TreeBatch;
    type ElementSet;

    /// Observe the current walk deadline without making an AX request.
    fn budget_expired(&self, budget: &cua_driver_core::walk_budget::WalkBudget) -> bool {
        budget.expired()
    }

    fn key(&self, element: &Self::Element) -> usize;
    fn set_messaging_timeout(&self, element: &Self::Element);
    fn set_optional_timeout(&self, element: &Self::Element, seconds: f32);
    fn text(&self, element: &Self::Element, name: &str) -> Read<String>;
    fn text_status(&self, element: &Self::Element, name: &str) -> AttrStatus;
    fn boolean(&self, element: &Self::Element, name: &str) -> Read<bool>;
    fn number(&self, element: &Self::Element, name: &str) -> Read<f64>;
    fn position(&self, element: &Self::Element) -> Read<(f64, f64)>;
    fn size(&self, element: &Self::Element) -> Read<(f64, f64)>;
    fn value(&self, element: &Self::Element) -> Result<Option<StringishAttrValue>, AXError>;
    fn observed_value(
        &self,
        element: &Self::Element,
    ) -> super::evidence::Observed<Result<Option<StringishAttrValue>, AXError>> {
        super::evidence::Observed::legacy_only(self.value(element))
    }
    fn batch(&self, element: &Self::Element, names: &[&str]) -> Result<Self::Batch, BatchFailure>;
    fn children(&self, element: &Self::Element) -> Read<Vec<Self::Element>>;
    fn observed_children(
        &self,
        element: &Self::Element,
    ) -> super::evidence::Observed<Read<Vec<Self::Element>>> {
        let legacy = self.children(element);
        super::evidence::observe_read(
            Some(self.key(element)),
            super::evidence::ReadSite::Children,
            legacy,
        )
    }
    fn element_attribute(&self, element: &Self::Element, name: &str) -> Read<Self::Element>;
    fn element_set(&self, element: &Self::Element, name: &str) -> Read<Self::ElementSet>;
    fn set_contains(&self, set: &Self::ElementSet, element: &Self::Element) -> bool;
    fn action_names(&self, element: &Self::Element) -> Read<Vec<String>>;
    fn value_settable(&self, element: &Self::Element) -> Settable;
    fn selected_settable(&self, element: &Self::Element) -> Settable;
    fn frame(&self, element: &Self::Element) -> Read<[f64; 4]>;
    fn retain_for_snapshot(&self, element: &Self::Element) -> usize;
}

/// Owns exactly one existing Core Foundation retain credit.
struct OwnedCf<T>(NonNull<T>);

impl<T> OwnedCf<T> {
    /// `raw` is a non-null CF object with a +1 credit transferred to this owner.
    unsafe fn from_owned(raw: *mut T) -> Self {
        Self(NonNull::new(raw).expect("owned CF reference must not be null"))
    }

    fn as_ptr(&self) -> *mut T {
        self.0.as_ptr()
    }

    fn into_raw(self) -> *mut T {
        let raw = self.as_ptr();
        std::mem::forget(self);
        raw
    }
}

impl<T> Clone for OwnedCf<T> {
    fn clone(&self) -> Self {
        unsafe { CFRetain(self.as_ptr() as CFTypeRef) };
        Self(self.0)
    }
}

impl<T> Drop for OwnedCf<T> {
    fn drop(&mut self) {
        unsafe { CFRelease(self.as_ptr() as CFTypeRef) };
    }
}

/// A retained native AX element. Raw pointers stay inside the native adapter.
pub(crate) struct NativeElement(OwnedCf<super::bindings::__AXUIElement>);

impl NativeElement {
    /// Adopt a non-null, retained AX element without an additional retain.
    unsafe fn from_owned(raw: AXUIElementRef) -> Self {
        Self(OwnedCf::from_owned(raw))
    }

    fn into_raw(self) -> AXUIElementRef {
        self.0.into_raw()
    }

    pub(crate) unsafe fn from_borrowed(raw: AXUIElementRef) -> Self {
        CFRetain(raw as CFTypeRef);
        Self::from_owned(raw)
    }
}

impl Clone for NativeElement {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    use core_foundation::{
        array::CFArray,
        base::{CFGetRetainCount, CFType, TCFType},
        string::CFString,
    };
    use std::ffi::c_void;

    fn array() -> CFArray<CFType> {
        CFArray::from_CFTypes(&[CFString::new("owned-tree-source-reference").as_CFType()])
    }

    fn object() -> (CFArray<CFType>, OwnedCf<c_void>) {
        let array = array();
        let raw = array.as_CFTypeRef();
        unsafe { CFRetain(raw) };
        (array, unsafe { OwnedCf::from_owned(raw as *mut c_void) })
    }

    fn count(array: &CFArray<CFType>) -> isize {
        unsafe { CFGetRetainCount(array.as_CFTypeRef()) }
    }

    #[test]
    fn adopt_does_not_retain_and_drop_releases_once() {
        let array = array();
        let raw = array.as_CFTypeRef();
        let initial = count(&array);
        unsafe { CFRetain(raw) };
        let owner = unsafe { OwnedCf::from_owned(raw as *mut c_void) };
        assert_eq!(count(&array), initial + 1);
        drop(owner);
        assert_eq!(count(&array), initial);
    }

    #[test]
    fn clone_adds_exactly_one_reference() {
        let (array, owner) = object();
        let initial = count(&array);
        let clone = owner.clone();
        assert_eq!(count(&array), initial + 1);
        drop(clone);
        assert_eq!(count(&array), initial);
    }

    #[test]
    fn into_raw_transfers_without_releasing() {
        let (array, owner) = object();
        let initial = count(&array);
        let raw = owner.into_raw();
        assert_eq!(count(&array), initial);
        unsafe { CFRelease(raw as CFTypeRef) };
        assert_eq!(count(&array), initial - 1);
    }

    #[test]
    fn deferred_clone_and_snapshot_transfer_have_distinct_credits() {
        let (array, owner) = object();
        let initial = count(&array);
        let deferred = owner.clone();
        let snapshot = owner.clone().into_raw();
        assert_eq!(count(&array), initial + 2);
        drop(owner);
        assert_eq!(count(&array), initial + 1);
        drop(deferred);
        assert_eq!(count(&array), initial);
        unsafe { CFRelease(snapshot as CFTypeRef) };
        assert_eq!(count(&array), initial - 1);
    }
}
