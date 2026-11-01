//! Raw HRESULTs for dispatched UIA writes. The generated windows-rs wrappers
//! treat positive HRESULTs as success; only S_OK proves the call was accepted.
use windows::core::{Interface, Param, BSTR};
use windows::Win32::UI::Accessibility::{
    IUIAutomationElement, IUIAutomationExpandCollapsePattern, IUIAutomationInvokePattern,
    IUIAutomationRangeValuePattern, IUIAutomationScrollItemPattern, IUIAutomationScrollPattern,
    IUIAutomationSelectionItemPattern, IUIAutomationTextRange, IUIAutomationTogglePattern,
    IUIAutomationValuePattern, ScrollAmount, TextPatternRangeEndpoint,
};

macro_rules! no_args {
    ($name:ident, $ty:ty, $method:ident) => {
        pub(crate) unsafe fn $name(target: &$ty) -> i32 {
            (Interface::vtable(target).$method)(Interface::as_raw(target)).0
        }
    };
}
no_args!(invoke, IUIAutomationInvokePattern, Invoke);
no_args!(toggle, IUIAutomationTogglePattern, Toggle);
no_args!(select, IUIAutomationSelectionItemPattern, Select);
no_args!(expand_raw, IUIAutomationExpandCollapsePattern, Expand);
no_args!(collapse, IUIAutomationExpandCollapsePattern, Collapse);
pub(crate) unsafe fn expand(target: &IUIAutomationExpandCollapsePattern) -> i32 {
    expand_or_collapse(target, true)
}

unsafe fn expand_or_collapse(target: &IUIAutomationExpandCollapsePattern, expand: bool) -> i32 {
    if expand {
        expand_raw(target)
    } else {
        collapse(target)
    }
}
no_args!(
    scroll_into_view,
    IUIAutomationScrollItemPattern,
    ScrollIntoView
);
no_args!(set_focus, IUIAutomationElement, SetFocus);
no_args!(select_text_range, IUIAutomationTextRange, Select);

pub(crate) unsafe fn set_value(target: &IUIAutomationValuePattern, value: &BSTR) -> i32 {
    let value_abi = <&BSTR as Param<BSTR>>::param(value);
    (Interface::vtable(target).SetValue)(Interface::as_raw(target), value_abi.abi()).0
}
pub(crate) unsafe fn set_range_value(target: &IUIAutomationRangeValuePattern, value: f64) -> i32 {
    (Interface::vtable(target).SetValue)(Interface::as_raw(target), value).0
}
pub(crate) unsafe fn scroll(
    target: &IUIAutomationScrollPattern,
    horizontal: ScrollAmount,
    vertical: ScrollAmount,
) -> i32 {
    (Interface::vtable(target).Scroll)(Interface::as_raw(target), horizontal, vertical).0
}

pub(crate) unsafe fn move_endpoint_by_range(
    target: &IUIAutomationTextRange,
    endpoint: TextPatternRangeEndpoint,
    range: &IUIAutomationTextRange,
    range_endpoint: TextPatternRangeEndpoint,
) -> i32 {
    (Interface::vtable(target).MoveEndpointByRange)(
        Interface::as_raw(target),
        endpoint,
        Interface::as_raw(range),
        range_endpoint,
    )
    .0
}
