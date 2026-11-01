//! macOS evidence assembly for the core actionability battery.
use crate::ax::actionability::{observe, Reader};
use cua_driver_core::actionability::Evidence;

pub(crate) fn enabled<R: Reader>(reader: &mut R, element: R::Element) -> Evidence<bool> {
    observe(reader.boolean(element, "AXEnabled"))
}

pub(crate) fn advertised_actions<R: Reader>(
    reader: &mut R,
    element: R::Element,
) -> Evidence<Vec<String>> {
    observe(reader.actions(element))
}

pub(crate) fn selectable_text<R: Reader>(reader: &mut R, element: R::Element) -> Evidence<bool> {
    observe(reader.settable(element, "AXSelectedTextRange"))
}
