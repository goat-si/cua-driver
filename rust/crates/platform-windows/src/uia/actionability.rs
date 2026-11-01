//! Live UIA evidence for the set_value admission check.
use crate::actionability::{LiveRead, PatternKind, RawLiveFacts};
use cua_driver_core::actionability::Rect;
use windows::core::Interface;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindowVisible};

fn read<T, U>(result: windows::core::Result<T>, map: impl FnOnce(T) -> U) -> LiveRead<U> {
    match result {
        Ok(value) => LiveRead::Value(map(value)),
        Err(error) => LiveRead::Failed(error.to_string()),
    }
}

/// Called on an initialized COM worker while the retained element is alive.
pub(crate) unsafe fn read_live_facts(element: &IUIAutomationElement, hwnd: HWND) -> RawLiveFacts {
    let is_offscreen = read(element.CurrentIsOffscreen(), |v| v.as_bool());
    let is_enabled = read(element.CurrentIsEnabled(), |v| v.as_bool());
    let bounds = read(element.CurrentBoundingRectangle(), |r| Rect {
        x: f64::from(r.left),
        y: f64::from(r.top),
        width: f64::from(r.right) - f64::from(r.left),
        height: f64::from(r.bottom) - f64::from(r.top),
    });
    let control_type = read(element.CurrentControlType(), |v| {
        super::control_type_name(v.0)
    });
    let window_visible = LiveRead::Value(IsWindowVisible(hwnd).as_bool());
    let window_minimized = LiveRead::Value(IsIconic(hwnd).as_bool());
    let mut cloaked = 0u32;
    let window_cloaked = read(
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
        ),
        |_| cloaked != 0,
    );
    let mut patterns = Vec::new();
    let mut failure = None;
    let mut value_read_only = LiveRead::NotSupported;
    let mut range_read_only = LiveRead::NotSupported;
    for (kind, id) in [
        (PatternKind::Invoke, UIA_InvokePatternId),
        (PatternKind::Toggle, UIA_TogglePatternId),
        (PatternKind::SelectionItem, UIA_SelectionItemPatternId),
        (PatternKind::ExpandCollapse, UIA_ExpandCollapsePatternId),
        (PatternKind::Value, UIA_ValuePatternId),
        (PatternKind::RangeValue, UIA_RangeValuePatternId),
        (PatternKind::Scroll, UIA_ScrollPatternId),
        (PatternKind::ScrollItem, UIA_ScrollItemPatternId),
    ] {
        match element.GetCurrentPattern(id) {
            Ok(pattern) => {
                patterns.push(kind);
                match kind {
                    PatternKind::Value => {
                        value_read_only = read(
                            pattern
                                .cast::<IUIAutomationValuePattern>()
                                .and_then(|p| p.CurrentIsReadOnly()),
                            |v| v.as_bool(),
                        )
                    }
                    PatternKind::RangeValue => {
                        range_read_only = read(
                            pattern
                                .cast::<IUIAutomationRangeValuePattern>()
                                .and_then(|p| p.CurrentIsReadOnly()),
                            |v| v.as_bool(),
                        )
                    }
                    _ => {}
                }
            }
            Err(error) if crate::uia_mutation::pattern_absent(error.code().0) => {}
            Err(error) => {
                if failure.is_none() {
                    failure = Some(error.to_string());
                }
            }
        }
    }
    RawLiveFacts {
        is_offscreen,
        is_enabled,
        bounds,
        window_visible,
        window_minimized,
        window_cloaked,
        control_type,
        patterns: failure.map_or(LiveRead::Value(patterns), LiveRead::Failed),
        value_read_only,
        range_read_only,
    }
}
