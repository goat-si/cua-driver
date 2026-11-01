//! Attribute and child access for label construction.
use super::bindings::{copy_bool_attr, copy_children, copy_string_attr, AXUIElementRef};
use super::content_attrs::{AttrStatus, CLASSIFICATION_ATTRIBUTES};
use core_foundation::base::{CFRelease, CFTypeRef};

pub(crate) trait LabelReader {
    type Element: Copy;
    fn string(&mut self, element: Self::Element, name: &str) -> Option<String>;
    fn boolean(&mut self, element: Self::Element, name: &str) -> Option<bool>;
    fn children(&mut self, element: Self::Element) -> Vec<Self::Element>;
    fn release(&mut self, element: Self::Element);
    fn status(&mut self, element: Self::Element, name: &str) -> AttrStatus;
    fn security_batch(&mut self, element: Self::Element) -> Option<[AttrStatus; 8]>;
}

pub(crate) struct NativeLabelReader;

impl LabelReader for NativeLabelReader {
    type Element = AXUIElementRef;
    fn string(&mut self, element: Self::Element, name: &str) -> Option<String> {
        unsafe { copy_string_attr(element, name) }
    }
    fn boolean(&mut self, element: Self::Element, name: &str) -> Option<bool> {
        unsafe { copy_bool_attr(element, name) }
    }
    fn children(&mut self, element: Self::Element) -> Vec<Self::Element> {
        unsafe { copy_children(element) }
    }
    fn release(&mut self, element: Self::Element) {
        unsafe { CFRelease(element as CFTypeRef) }
    }
    fn status(&mut self, element: Self::Element, name: &str) -> AttrStatus {
        unsafe { super::content_attrs::read_string_status(element, name) }
    }
    fn security_batch(&mut self, element: Self::Element) -> Option<[AttrStatus; 8]> {
        match unsafe { super::batch_attrs::copy_attrs_batch(element, &CLASSIFICATION_ATTRIBUTES) } {
            super::batch_attrs::BatchRead::Slots(slots) => Some(std::array::from_fn(|i| {
                super::content_attrs::string_slot(&slots, i)
            })),
            super::batch_attrs::BatchRead::Fallback(_) => None,
        }
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use super::{AttrStatus, LabelReader, CLASSIFICATION_ATTRIBUTES};
    use std::collections::HashMap;

    #[derive(Default)]
    pub(crate) struct FakeReader {
        pub strings: HashMap<(usize, String), String>,
        pub children: HashMap<usize, Vec<usize>>,
        pub trace: Vec<(usize, String)>,
        pub failures: std::collections::HashSet<(usize, String)>,
        pub statuses: HashMap<(usize, String), AttrStatus>,
        pub reject_batch: bool,
    }

    impl FakeReader {
        pub fn set(&mut self, element: usize, name: &str, value: &str) {
            self.strings.insert((element, name.into()), value.into());
        }
    }

    impl LabelReader for FakeReader {
        type Element = usize;
        fn string(&mut self, element: usize, name: &str) -> Option<String> {
            self.trace.push((element, name.into()));
            if self.failures.contains(&(element, name.into())) {
                return None;
            }
            self.strings.get(&(element, name.into())).cloned()
        }
        fn boolean(&mut self, _element: usize, _name: &str) -> Option<bool> {
            None
        }
        fn children(&mut self, element: usize) -> Vec<usize> {
            self.children.get(&element).cloned().unwrap_or_default()
        }
        fn release(&mut self, _element: usize) {}
        fn status(&mut self, element: usize, name: &str) -> AttrStatus {
            self.trace.push((element, name.into()));
            if let Some(status) = self.statuses.get(&(element, name.into())) {
                status.clone()
            } else if self.failures.contains(&(element, name.into())) {
                super::super::content_attrs::FAILED_TEST
            } else if let Some(value) = self.strings.get(&(element, name.into())) {
                AttrStatus::Known(value.clone())
            } else {
                AttrStatus::Absent
            }
        }
        fn security_batch(&mut self, element: usize) -> Option<[AttrStatus; 8]> {
            if self.reject_batch {
                None
            } else {
                Some(CLASSIFICATION_ATTRIBUTES.map(|name| self.status(element, name)))
            }
        }
    }

    impl super::super::write_outcome::DirectValueReader for FakeReader {
        fn value(&mut self, element: usize) -> super::super::write_outcome::ValueObs {
            match self.string(element, "AXValue") {
                Some(value) => super::super::write_outcome::ValueObs::Text(value),
                None => super::super::write_outcome::ValueObs::Missing,
            }
        }
    }
}
