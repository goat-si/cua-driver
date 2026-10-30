//! AX evidence required before a key belief can be published.

use core_foundation::base::{CFRelease, CFTypeRef};

use crate::ax::bindings::{
    ax_get_window_id, copy_ax_windows_including, copy_string_attr, AXUIElementCreateApplication,
};
use crate::ax::exact_target::{check_window_focus_conflict, WindowFocusConflict};

pub(super) fn is_proven_standard_window(pid: i32, wid: u32) -> bool {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return false;
        }
        let mut standard = false;
        for window in copy_ax_windows_including(app, pid, wid) {
            if ax_get_window_id(window) == Some(wid)
                && copy_string_attr(window, "AXSubrole").as_deref() == Some("AXStandardWindow")
            {
                standard = true;
            }
            CFRelease(window as CFTypeRef);
        }
        CFRelease(app as CFTypeRef);
        standard
            && matches!(
                check_window_focus_conflict(pid, wid, None),
                WindowFocusConflict::Clear
            )
    }
}
