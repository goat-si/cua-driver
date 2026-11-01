//! Isolated accessibility reads for the actionability adapter.
use super::bindings::*;
use cua_driver_core::actionability::{Evidence, Rect};

pub(crate) trait Reader {
    type Element: Copy;
    fn boolean(&mut self, element: Self::Element, attribute: &str) -> Evidence<bool>;
    fn string(&mut self, element: Self::Element, attribute: &str) -> Evidence<String>;
    fn actions(&mut self, element: Self::Element) -> Evidence<Vec<String>>;
    fn frame(&mut self, element: Self::Element) -> Evidence<Rect>;
    fn settable(&mut self, element: Self::Element, attribute: &str) -> Evidence<bool>;
}

/// The element must remain valid for every call made through this reader.
pub(crate) struct NativeReader;

impl Reader for NativeReader {
    type Element = AXUIElementRef;

    fn boolean(&mut self, element: Self::Element, attribute: &str) -> Evidence<bool> {
        unsafe { copy_bool_attr(element, attribute) }.map_or(Evidence::Absent, Evidence::Known)
    }
    fn string(&mut self, element: Self::Element, attribute: &str) -> Evidence<String> {
        assert_ne!(attribute, "AXValue");
        assert_ne!(attribute, "AXSelectedText");
        unsafe { copy_string_attr(element, attribute) }.map_or(Evidence::Absent, Evidence::Known)
    }
    fn actions(&mut self, element: Self::Element) -> Evidence<Vec<String>> {
        Evidence::Known(unsafe { copy_action_names(element) })
    }
    fn frame(&mut self, element: Self::Element) -> Evidence<Rect> {
        unsafe { element_screen_rect(element) }.map_or(Evidence::Absent, |[x, y, width, height]| {
            Evidence::Known(Rect { x, y, width, height })
        })
    }
    fn settable(&mut self, element: Self::Element, attribute: &str) -> Evidence<bool> {
        Evidence::Known(unsafe { is_attribute_settable(element, attribute) })
    }
}

// Initial compatibility seam: like the existing helpers, failures disappear.
pub(crate) fn observe<T>(evidence: Evidence<T>) -> Evidence<T> {
    match evidence {
        Evidence::Unknown(_) => Evidence::Absent,
        other => other,
    }
}
