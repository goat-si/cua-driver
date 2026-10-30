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
            if is_standard_match(
                wid,
                ax_get_window_id(window),
                copy_string_attr(window, "AXSubrole").as_deref(),
            ) {
                standard = true;
            }
            CFRelease(window as CFTypeRef);
        }
        CFRelease(app as CFTypeRef);
        standard_window_with(standard, || check_window_focus_conflict(pid, wid, None))
    }
}

fn is_standard_match(wid: u32, actual: Option<u32>, subrole: Option<&str>) -> bool {
    actual == Some(wid) && subrole == Some("AXStandardWindow")
}

fn standard_window_with(standard: bool, conflict: impl FnOnce() -> WindowFocusConflict) -> bool {
    standard && matches!(conflict(), WindowFocusConflict::Clear)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_window_requires_clear_conflict() {
        assert!(is_standard_match(1, Some(1), Some("AXStandardWindow")));
        assert!(!is_standard_match(1, Some(2), Some("AXStandardWindow")));
        assert!(!is_standard_match(1, Some(1), Some("AXSheet")));
        assert!(!standard_window_with(false, || panic!(
            "nonstandard window"
        )));
        assert!(standard_window_with(true, || WindowFocusConflict::Clear));
        assert!(!standard_window_with(true, || {
            WindowFocusConflict::SiblingWindowFocused {
                target_window_id: 1,
                sibling_window_id: 2,
            }
        }));
    }
}
