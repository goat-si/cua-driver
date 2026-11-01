//! AX evidence required before a key belief can be published.

use core_foundation::base::{CFRelease, CFTypeRef};

use super::key_belief::{Ticket, BELIEFS};
use crate::ax::bindings::{
    ax_get_window_id, copy_ax_windows_including, copy_string_attr, AXUIElementCreateApplication,
};
pub(super) fn is_proven_standard_window(ticket: Ticket, wid: u32) -> bool {
    let cached = BELIEFS.standard_proven(ticket, wid);
    let standard = cached || read_standard_window(ticket.pid, wid);
    if standard && !cached {
        BELIEFS.remember_standard(ticket, wid);
    }
    standard
}

fn read_standard_window(pid: i32, wid: u32) -> bool {
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
        standard
    }
}

fn is_standard_match(wid: u32, actual: Option<u32>, subrole: Option<&str>) -> bool {
    actual == Some(wid) && subrole == Some("AXStandardWindow")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_window_requires_exact_identity_and_subrole() {
        assert!(is_standard_match(1, Some(1), Some("AXStandardWindow")));
        assert!(!is_standard_match(1, Some(2), Some("AXStandardWindow")));
        assert!(!is_standard_match(1, Some(1), Some("AXSheet")));
    }
}
