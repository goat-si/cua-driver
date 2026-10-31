//! Attribute and child access for label construction.
use super::bindings::{copy_bool_attr, copy_children, copy_string_attr, AXUIElementRef};
use core_foundation::base::{CFRelease, CFTypeRef};

pub(crate) trait LabelReader {
    type Element: Copy;
    fn string(&mut self, element: Self::Element, name: &str) -> Option<String>;
    fn boolean(&mut self, element: Self::Element, name: &str) -> Option<bool>;
    fn children(&mut self, element: Self::Element) -> Vec<Self::Element>;
    fn release(&mut self, element: Self::Element);
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
}

#[cfg(test)]
pub(crate) mod fake {
    use super::LabelReader;
    use std::collections::HashMap;

    #[derive(Default)]
    pub(crate) struct FakeReader {
        pub strings: HashMap<(usize, String), String>,
        pub children: HashMap<usize, Vec<usize>>,
        pub trace: Vec<(usize, String)>,
        pub failures: std::collections::HashSet<(usize, String)>,
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
    }
}
