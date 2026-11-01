//! Background-safe "scroll an off-screen element into view" for coordinate actions.
//!
//! The element cache records each actionable element's screen center at walk
//! time. On a short display (e.g. 1024x768) a tall window reflows so some
//! controls sit below the visible area; their cached center then falls outside
//! the window rect. A raw coordinate tap there lands on whatever is actually
//! at that pixel - the taskbar, another window - instead of the control. The
//! click/double-click/right-click element paths already turn that into a clean
//! error via `input::point_in_window_bounds`; this module lets them first ask
//! the control to make itself visible so the action can proceed.
//!
//! Two strategies, tried in order:
//!
//! 1. `IUIAutomationScrollItemPattern::ScrollIntoView` asks the *element* to
//!    scroll itself into view through its own scroll container (the cleanest
//!    option - the control knows how to reveal itself). Only items inside an
//!    items-control (ListItem, TreeItem, web/XAML virtualized rows) implement
//!    `ScrollItemPattern`, so this is a no-op for plain controls in a generic
//!    `ScrollViewer`.
//!
//! 2. Ancestor `IUIAutomationScrollPattern` fallback. A plain WPF/Win32 control
//!    inside a `ScrollViewer` (e.g. a `Button` in the harness's main scroll
//!    region) exposes only `InvokePattern` - NOT `ScrollItemPattern` - so step
//!    1 can't move it (verified on the WPF harness: the off-screen Increment
//!    button reports `Unsupported Pattern`). Its ancestor `ScrollViewer` *does*
//!    expose `ScrollPattern`, so we walk up to the first vertically-scrollable
//!    ancestor and `Scroll` the container until the target's live bounding rect
//!    re-enters the viewport, then re-read its center.
//!
//! Both are delivered over the UI Automation accessibility channel, not the
//! input queue, so they neither raise nor activate the window. We still wrap the
//! scroll calls in the UWP foreground-steal bypass for parity with the Invoke
//! path (XAML hosts can self-foreground on UIA-driven layout changes); the
//! bypass is a no-op for classic Win32 hosts.

use std::thread::sleep;
use std::time::Duration;

use windows::core::Interface;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::UI::Accessibility::{
    IUIAutomationElement, IUIAutomationScrollItemPattern, IUIAutomationScrollPattern,
    ScrollAmount_LargeDecrement, ScrollAmount_LargeIncrement, ScrollAmount_NoAmount,
    ScrollAmount_SmallDecrement, ScrollAmount_SmallIncrement, UIA_ScrollItemPatternId,
    UIA_ScrollPatternId,
};

/// Return UIA's live IsOffscreen state for a retained element pointer.
///
/// `None` means the property could not be read. Callers fail open in that case
/// so a transient UIA query never blocks an otherwise valid coordinate action.
pub unsafe fn element_is_offscreen(element_ptr: usize) -> Option<bool> {
    if element_ptr == 0 {
        return None;
    }
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let elem: IUIAutomationElement = IUIAutomationElement::from_raw(element_ptr as *mut _);
    let result = elem.CurrentIsOffscreen().ok().map(|value| value.as_bool());
    std::mem::forget(elem);
    result
}

/// Ask the UIA element behind `element_ptr` to scroll itself into view, then
/// return its fresh on-screen center (from the element's *live* bounding rect,
/// since the cached center is stale once the control has moved).
///
/// Tries `ScrollItemPattern::ScrollIntoView` first; if that leaves the element
/// off-screen (unsupported, or the container ignored it) it falls back to
/// scrolling the nearest vertically-scrollable ancestor via `ScrollPattern`.
///
/// Returns `None` when neither strategy lands the element inside its host
/// window - callers fall back to the existing off-screen failure in that case.
///
/// # Safety
/// `element_ptr` must be a live `IUIAutomationElement` vtable pointer. Callers
/// hold it alive through a `RetainedElement` guard (its COM `AddRef`) for the
/// duration of this call. We borrow the pointer without consuming the cache's
/// refcount (the constructed handle is `forget`-ten before return).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RevealOutcome {
    Revealed((i32, i32)),
    NotRevealed { writes_sent: bool },
    Uncertain,
}

pub(crate) unsafe fn scroll_into_view_and_recenter(
    host_hwnd: u64,
    element_ptr: usize,
) -> RevealOutcome {
    if element_ptr == 0 {
        return RevealOutcome::NotRevealed { writes_sent: false };
    }
    // Ensure COM is live on THIS thread before the first UIA call. Strategy 1
    // (`scroll_item_into_view`) touches UIA directly, so on a worker thread that
    // never called `CoInitializeEx` it would otherwise fail with
    // `CO_E_NOTINITIALIZED`. Fire-and-forget + idempotent (the crate convention,
    // see `uia::mod` / `tools::page`): a redundant init just returns S_FALSE /
    // RPC_E_CHANGED_MODE, which the `let _ =` swallows.
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

    let elem = std::mem::ManuallyDrop::new(IUIAutomationElement::from_raw(element_ptr as *mut _));

    // A recentred point only counts if it actually lands inside the host window
    // - otherwise the coordinate tap would still miss. This mirrors the caller's
    // own bounds check so an ineffective ScrollIntoView falls through to the
    // ancestor ScrollPattern fallback instead of returning a bogus center.
    let actionable = |c: (i32, i32)| -> Option<(i32, i32)> {
        let visible = elem
            .CurrentIsOffscreen()
            .map(|value| !value.as_bool())
            .unwrap_or(true);
        if visible && crate::input::point_in_window_bounds(host_hwnd, c.0, c.1) {
            Some(c)
        } else {
            None
        }
    };

    let mut writes_sent = false;
    match scroll_item_into_view(host_hwnd, &elem) {
        Err(RevealFailure::Sent) => return RevealOutcome::Uncertain,
        Err(RevealFailure::PreSend) => return RevealOutcome::NotRevealed { writes_sent: false },
        Ok((point, sent)) => {
            writes_sent |= sent;
            if let Some(point) = point.and_then(actionable) {
                return RevealOutcome::Revealed(point);
            }
        }
    }
    match scroll_ancestors_into_view(host_hwnd, &elem) {
        Err(()) => RevealOutcome::Uncertain,
        Ok((point, sent)) => {
            writes_sent |= sent;
            match point.and_then(actionable) {
                Some(point) => RevealOutcome::Revealed(point),
                None => RevealOutcome::NotRevealed { writes_sent },
            }
        }
    }
}

/// Scroll a cached UIA scroll container in the requested direction.
///
/// WebView2/Tauri content can expose a real `ScrollPattern` even though its
/// top-level host HWND ignores `WM_VSCROLL`. Keeping this on the UIA channel
/// makes indexed background scrolls reach that container without activating
/// the host window.
#[derive(Debug)]
pub(crate) struct ScrollWrite {
    pub completed: u32,
    pub requested: u32,
    pub stop: Option<crate::uia_mutation::MutationStop>,
}

pub(crate) unsafe fn scroll_element(
    element_ptr: usize,
    direction: &str,
    amount: u32,
) -> anyhow::Result<ScrollWrite> {
    use crate::uia_mutation::{pattern_absent, scroll_preflight, MutationStop};
    if element_ptr == 0 {
        anyhow::bail!("cached UIA scroll element is null");
    }
    let requested = amount.max(1);
    let mut outcome = ScrollWrite {
        completed: 0,
        requested,
        stop: None,
    };
    let (horizontal, increment, read) = scroll_preflight(direction, || {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let elem =
            std::mem::ManuallyDrop::new(IUIAutomationElement::from_raw(element_ptr as *mut _));
        elem.GetCurrentPattern(UIA_ScrollPatternId)
            .and_then(|pattern| pattern.cast::<IUIAutomationScrollPattern>())
    })
    .ok_or_else(|| anyhow::anyhow!("unknown scroll direction {direction:?}"))?;
    let scroll = match read {
        Ok(scroll) => scroll,
        Err(error) => {
            let hr = error.code().0;
            outcome.stop = Some(if pattern_absent(hr) {
                MutationStop::Absent
            } else {
                MutationStop::AcquireFailed(hr)
            });
            return Ok(outcome);
        }
    };
    let step = if increment {
        ScrollAmount_LargeIncrement
    } else {
        ScrollAmount_LargeDecrement
    };
    for _ in 0..requested {
        let result = if horizontal {
            super::sent_call::scroll(&scroll, step, ScrollAmount_NoAmount)
        } else {
            super::sent_call::scroll(&scroll, ScrollAmount_NoAmount, step)
        };
        if result == 0 {
            outcome.completed += 1;
        } else {
            outcome.stop = Some(MutationStop::Write(result));
            break;
        }
    }
    Ok(outcome)
}

enum RevealFailure { PreSend, Sent }

/// Strategy 1: `ScrollItemPattern::ScrollIntoView` on the element itself.
unsafe fn scroll_item_into_view(
    host_hwnd: u64,
    elem: &IUIAutomationElement,
) -> Result<(Option<(i32, i32)>, bool), RevealFailure> {
    let scroll_item = match elem.GetCurrentPattern(UIA_ScrollItemPatternId)
        .and_then(|pattern| pattern.cast::<IUIAutomationScrollItemPattern>()) {
        Ok(pattern) => pattern,
        Err(error) if crate::uia_mutation::pattern_absent(error.code().0) => return Ok((None, false)),
        Err(_) => return Err(RevealFailure::PreSend),
    };
    let hr = crate::uia::fg_bypass::run_with_uwp_bypass(host_hwnd as isize, || {
        super::sent_call::scroll_into_view(&scroll_item)
    });
    let point = crate::uia_mutation::after_sent_call(hr, || {
        elem.CurrentBoundingRectangle()
            .ok()
            .map(|rect| ((rect.left + rect.right) / 2, (rect.top + rect.bottom) / 2))
    })
    .map_err(|_| RevealFailure::Sent)?;
    Ok((point, true))
}

/// Strategy 2: walk every vertically-scrollable ancestor and use
/// `ScrollPattern::Scroll` until the target's live rect re-enters a visible
/// viewport.
unsafe fn scroll_ancestors_into_view(
    host_hwnd: u64,
    elem: &IUIAutomationElement,
) -> Result<(Option<(i32, i32)>, bool), ()> {
    let mut writes_sent = false;
    let mut uncertain = false;
    let point = (|| -> Option<(i32, i32)> {
        // COM is already initialized by `scroll_into_view_and_recenter` (this fn's
        // only caller), so no redundant `CoInitializeEx` here.
        let automation = super::client::new_uia_client().ok()?;
        let walker = automation.ControlViewWalker().ok()?;

        // Collect every vertically-scrollable ancestor. Nested controls are common:
        // a ListItem may already be visible inside its ListBox while that entire
        // ListBox is clipped by an outer page ScrollViewer. Trying only the nearest
        // pattern leaves the element hidden even though another ancestor can reveal it.
        let mut containers: Vec<(IUIAutomationElement, IUIAutomationScrollPattern)> = Vec::new();
        let mut cur = walker.GetParentElement(elem).ok()?;
        for _ in 0..16 {
            match cur.GetCurrentPattern(UIA_ScrollPatternId)
                .and_then(|p| p.cast::<IUIAutomationScrollPattern>()) {
                Ok(sp) => {
                    let scrollable = sp.CurrentVerticallyScrollable().ok()?.as_bool();
                    if scrollable { containers.push((cur.clone(), sp)); }
                }
                Err(error) if crate::uia_mutation::pattern_absent(error.code().0) => {}
                Err(_) => return None,
            }
            match walker.GetParentElement(&cur) {
                Ok(parent) => cur = parent,
                Err(_) => break,
            }
        }

        for (container, sp) in containers {
            let crect = container.CurrentBoundingRectangle().ok()?;
            // Iteratively scroll this container until the target's center sits inside
            // its viewport. If UIA still marks the target off-screen, continue with
            // the next outer scrollable ancestor.
            let mut last_dir = 0i32;
            for _ in 0..60 {
                let tr = elem.CurrentBoundingRectangle().ok()?;
                let tcy = (tr.top + tr.bottom) / 2;
                if tcy >= crect.top && tcy <= crect.bottom {
                    break;
                }
                let dir = if tcy < crect.top { -1 } else { 1 };
                let overshot = last_dir != 0 && dir != last_dir;
                let amount = match (dir, overshot) {
                    (-1, false) => ScrollAmount_LargeDecrement,
                    (-1, true) => ScrollAmount_SmallDecrement,
                    (_, false) => ScrollAmount_LargeIncrement,
                    (_, true) => ScrollAmount_SmallIncrement,
                };
                let scrolled =
                    crate::uia::fg_bypass::run_with_uwp_bypass(host_hwnd as isize, || {
                        super::sent_call::scroll(&sp, ScrollAmount_NoAmount, amount)
                    });
                writes_sent = true;
                if crate::uia_mutation::after_sent_call(scrolled, || ()).is_err() {
                    uncertain = true;
                    return None;
                }
                last_dir = dir;
                sleep(Duration::from_millis(15));
            }
            let rect = elem.CurrentBoundingRectangle().ok()?;
            let center = ((rect.left + rect.right) / 2, (rect.top + rect.bottom) / 2);
            let visible = elem
                .CurrentIsOffscreen()
                .map(|value| !value.as_bool())
                .unwrap_or(true);
            if visible && crate::input::point_in_window_bounds(host_hwnd, center.0, center.1) {
                return Some(center);
            }
        }

        let rect = elem.CurrentBoundingRectangle().ok()?;
        Some(((rect.left + rect.right) / 2, (rect.top + rect.bottom) / 2))
    })();
    if uncertain {
        Err(())
    } else {
        Ok((point, writes_sent))
    }
}
