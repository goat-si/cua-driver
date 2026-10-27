//! Shared delivery-mode logic for cua-driver-rs Windows input tools.
//!
//! Mirrors the macOS `DeliveryMode` model (see platform-macos
//! `tools::DeliveryMode`): each input tool accepts an optional
//! `delivery_mode` field with exactly two modes — the agent-selected rung
//! of the best-effort-background ladder, passed per call (never a stored
//! setting):
//!
//! - `background` (DEFAULT) — PostMessage / UIA path only, never fronts. If
//!   `would_be_silently_dropped(target, event_kind)` returns true, the tool
//!   returns a structured `background_unavailable` error so the caller can
//!   retry that action with `delivery_mode:"foreground"`. This is
//!   the default because cua-driver's value proposition is that input never
//!   steals foreground — surfacing an honest error beats silently fronting.
//!
//! - `foreground` — SendInput with a brief `SetForegroundWindow(target)`
//!   swap, restoring the prior foreground after the events flush. Brief
//!   flash unless the target was already foreground. The agent's explicit
//!   last resort, and the only rung that reaches Chromium-content / GTK-
//!   button / VCL-accelerator targets reliably.
//!
//! The legacy Windows-only `auto` mode (silent SendInput fallback) was
//! removed in the macOS-alignment pass: it could front without the caller
//! opting in, breaking the no-foreground contract macOS guarantees. Any
//! unrecognised value now resolves to `background` (see [`DeliveryMode::parse`]).
//!
//! The matrix of "which (window class, event kind) pairs are silently
//! dropped by PostMessage" lives here as `would_be_silently_dropped` — a
//! Windows-specific safeguard with no macOS analogue (CGEvent posting does
//! not have the same per-framework silent-drop problem). Tools call into
//! this helper instead of inlining the class checks.

use serde_json::Value;

/// Family of synthetic input events. Used together with the target HWND
/// to decide whether PostMessage will be silently dropped.
#[derive(Copy, Clone, Debug)]
pub enum EventKind {
    /// `WM_LBUTTONDOWN/UP`, `WM_RBUTTONDOWN/UP`, `WM_MBUTTONDOWN/UP`.
    MouseClick,
    /// `WM_MOUSEMOVE` (also covers drag intermediate steps).
    MouseMove,
    /// `WM_VSCROLL` / `WM_HSCROLL`.
    MouseScroll,
    /// `WM_KEYDOWN/UP` with no modifiers — plain key tap.
    Keystroke,
    /// `WM_KEYDOWN/UP` with modifiers — accelerator candidate (Ctrl+S etc).
    KeyCombo,
    /// `WM_CHAR` text input.
    TextInput,
}

impl EventKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::MouseClick => "mouse_click",
            Self::MouseMove => "mouse_move",
            Self::MouseScroll => "mouse_scroll",
            Self::Keystroke => "keystroke",
            Self::KeyCombo => "key_combo",
            Self::TextInput => "text_input",
        }
    }
}

/// The shared per-call delivery mode; see [`cua_driver_core::delivery`].
pub use cua_driver_core::delivery::DeliveryMode;

/// JSON-schema fragment for the `delivery_mode` field. Include this in every
/// input tool's `input_schema.properties.delivery_mode`. Two modes, matching
/// the macOS surface.
pub fn delivery_mode_schema() -> Value {
    // Borrow the canonical SHAPE (type/enum) from the shared cross-platform
    // builder so it can never drift from the consistency gate, while keeping the
    // Windows-specific prose (UIA Invoke / PostMessage / background_unavailable).
    cua_driver_core::tool_schema::delivery_mode_schema_with(
        "Input delivery mode. 'background' (default) never swaps foreground: \
         it routes through UIA Invoke / PostMessage and the window is never \
         raised. For targets whose input stack silently drops posted events \
         (Chromium/Electron content, GTK buttons, VCL/LibreOffice accelerators) \
         the tool returns a structured background_unavailable error rather than \
         fronting. 'foreground' is the explicit escalation: a brief \
         SetForegroundWindow swap + SendInput, restoring the prior foreground \
         afterward. \
         IMPORTANT: 'background' is not a hint to weigh — it is the mandatory \
         first attempt. Do NOT pass 'foreground' preemptively because a target \
         'looks like' GTK/Chromium/Electron; the DRIVER decides when background \
         is impossible and tells you so via a background_unavailable error (or a \
         verified no-op). Only THEN re-issue the same action with 'foreground'. \
         The lists above are the driver's detectors, not a checklist for you to \
         front on a guess — fronting up-front needlessly steals the user's focus \
         and is a bug, not a shortcut. Matches the macOS delivery_mode surface.",
    )
}

/// Returns true if PostMessage on `hwnd` for this `event_kind` is empirically
/// known to be silently dropped by the target's input stack.
///
/// Combines the existing `is_chromium_target_window` detector with new GTK
/// (`gdkWindowToplevel` / `gdkSurfaceToplevel`) detection.
#[cfg(target_os = "windows")]
pub fn would_be_silently_dropped(hwnd: u64, kind: EventKind) -> bool {
    use EventKind::*;
    if crate::input::is_chromium_target_window(hwnd) {
        // Chromium's input thread architecture requires SendInput-queue
        // origin for pointer and keyboard events (#1623). Posted WM_CHAR and
        // plain key messages can return success while a background renderer
        // receives nothing, so they must be refused as honestly as chords.
        return matches!(
            kind,
            MouseClick | MouseMove | MouseScroll | Keystroke | KeyCombo | TextInput
        );
    }
    if crate::input::has_chromium_descendant(hwnd) {
        // Embedded WebView2 hosts retain useful UIA/top-level routes for
        // clicks and ValuePattern text. Their drag, wheel and modifier-chord
        // paths still depend on the renderer's system input queue.
        return matches!(kind, MouseMove | MouseScroll | KeyCombo);
    }
    if is_wpf_target_window(hwnd) {
        // WPF ignores posted pointer messages unless the live system cursor is
        // over the window. Its InputManager also ignores posted key messages
        // while another native window owns foreground; PostMessage still
        // returns success, so both routes need an honest refusal.
        //
        // Do not classify WM_VSCROLL/WM_HSCROLL here: the scroll tool posts the
        // scrollbar messages directly to the top-level HWND, and WPF hosts that
        // explicitly handle those messages (including our harness hook) can
        // consume them without a foreground swap.
        return wpf_drops_event(kind, target_is_foreground(hwnd));
    }
    if is_tk_target_window(hwnd) {
        // Tk derives button transitions from GetKeyState rather than trusting
        // a posted WM_*BUTTON message. PostMessage therefore reports success
        // without producing a Tk ButtonPress, just as posted key messages can
        // be accepted without reaching the focused widget.
        return tk_drops_event(kind);
    }
    // NB: WinUI3 (`WinUIDesktopWin32WindowClass`) is deliberately NOT flagged
    // here. It looks WPF-like, but its composition input-site does NOT consume
    // the synthetic pen/touch the way WPF's stylus stack does — routing WinUI3
    // background clicks through the coordinate injector neither lands double/
    // right-click NOR preserves the no-foreground contract (measured: it
    // regressed ax-bg from 0/8 to 8/8 stolen). Driving WinUI3 double/right-click
    // in the background needs a WinUI3-specific input path (composition input-
    // site target), tracked separately; single left-click already works via UIA
    // Invoke and the contract holds.
    if is_gtk_target_window(hwnd) {
        // Conservative flag for GTK: button widgets ignore PostMessage
        // clicks, drawing-area widgets accept them. We cannot distinguish
        // at the HWND level (single HWND for the whole GTK window) so we
        // flag mouse clicks broadly. Canvas-style drag works in practice;
        // caller can still opt to retry with delivery_mode:"background" on the
        // drag path if the click error wasn't actually load-bearing.
        return matches!(kind, MouseClick);
    }
    if is_vcl_target_window(hwnd) {
        // VCL (LibreOffice / OpenOffice family) routes accelerator keys
        // through TranslateAccelerator, which reads `GetKeyState`.
        // PostMessage(WM_KEYDOWN) delivers the key but does NOT update
        // GetKeyState, so single-key dialog accelerators (Y/N for
        // confirmations, Esc / Enter / Tab) and modifier combos (Ctrl+S,
        // Alt+F4) silently fail. Plain WM_CHAR text input through the
        // document widgets still works (verified end-to-end against
        // Writer's main editing area). Flag the keystroke-class events
        // so delivery_mode:"background" surfaces a structured error instead
        // of pretending to succeed.
        return matches!(kind, Keystroke | KeyCombo);
    }
    false
}

#[cfg(not(target_os = "windows"))]
pub fn would_be_silently_dropped(_hwnd: u64, _kind: EventKind) -> bool {
    false
}

fn wpf_drops_event(kind: EventKind, target_is_foreground: bool) -> bool {
    matches!(kind, EventKind::MouseClick | EventKind::MouseMove)
        || (!target_is_foreground && matches!(kind, EventKind::Keystroke | EventKind::KeyCombo))
}

fn tk_drops_event(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::MouseClick | EventKind::Keystroke | EventKind::KeyCombo | EventKind::TextInput
    )
}

fn target_is_foreground(hwnd: u64) -> bool {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, GetForegroundWindow, GA_ROOT};
        if hwnd == 0 {
            return false;
        }
        unsafe {
            let target = GetAncestor(HWND(hwnd as *mut _), GA_ROOT);
            let foreground = GetForegroundWindow();
            let foreground_root = GetAncestor(foreground, GA_ROOT);
            !target.0.is_null() && target == foreground_root
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = hwnd;
        false
    }
}

/// Detect LibreOffice / OpenOffice (VCL framework) windows.
///
/// VCL on Windows registers window classes with a `SAL` prefix (StarOffice's
/// "Service Abstraction Layer"): `SALFRAME` (top-level), `SALSUBFRAME`
/// (dialogs / popups), `SALOBJECT` (embedded), `SALMENU` (menu host),
/// `SALTMPSUBFRAME` (transient). The `SAL` prefix is unique enough to
/// match by leading characters.
pub fn is_vcl_target_window(hwnd: u64) -> bool {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
        if hwnd == 0 {
            return false;
        }
        let mut buf = [0u16; 64];
        let n = unsafe { GetClassNameW(HWND(hwnd as *mut _), &mut buf) };
        if n <= 0 {
            return false;
        }
        let class = String::from_utf16_lossy(&buf[..n as usize]);
        class.starts_with("SAL")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = hwnd;
        false
    }
}

/// Detect WPF top-level windows. WPF hosts its visual tree in an HWND whose
/// class is `HwndWrapper[<module>;;<guid>]`. WPF TextBoxes consume only real
/// keyboard input routed through WPF's input manager, so a posted `WM_CHAR`
/// (the `post_type_text` path) is silently dropped — so in background, type_text
/// with no element_index returns `background_unavailable` (escalate to
/// `delivery_mode:"foreground"`); with an element_index it uses UIA ValuePattern.
pub fn is_wpf_target_window(hwnd: u64) -> bool {
    read_class_name(hwnd).starts_with("HwndWrapper")
}

/// Detect Tk/Tkinter top-level windows. Tk registers this stable class name
/// for its root and child toplevels on Windows.
pub fn is_tk_target_window(hwnd: u64) -> bool {
    is_tk_class_name(&read_class_name(hwnd))
}

fn is_tk_class_name(class: &str) -> bool {
    class == "TkTopLevel" || class.starts_with("TkTopLevel.")
}

/// Detect WinUI3 / Windows-App-SDK desktop top-level windows. The frame is a
/// Win32 HWND of class `WinUIDesktopWin32WindowClass`, but — unlike WPF — that
/// frame does NOT host the visual tree or consume pointer input. The XAML
/// content renders into a child *content island* (DirectComposition / a
/// `Microsoft.UI.Content.DesktopChildSiteBridge` HWND) whose input stack only
/// processes real pointer input off the system input queue — it ignores both
/// posted `WM_*BUTTON` messages AND the synthetic pen/touch injector (the
/// latter additionally click-activates the frame: measured 8/8 foreground
/// steals). The only background-safe actuator that lands on a WinUI3 element is
/// a UIA pattern call (see `winui3_uia_multi_invoke` in the tools layer), and
/// that has no right-click / drag analogue — so those gestures cannot both land
/// and hold the no-foreground contract and surface a structured
/// `background_unavailable` error instead.
pub fn is_winui3_target_window(hwnd: u64) -> bool {
    read_class_name(hwnd) == "WinUIDesktopWin32WindowClass"
}

/// Detect GTK/GDK top-level windows.
///
/// GTK 3 on Windows uses class `gdkWindowToplevel`; GTK 4 uses
/// `gdkSurfaceToplevel`. Match by prefix in case future GTK versions
/// append a suffix the way Chromium does (`Chrome_WidgetWin_0` /
/// `Chrome_WidgetWin_1`).
pub fn is_gtk_target_window(hwnd: u64) -> bool {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
        if hwnd == 0 {
            return false;
        }
        let mut buf = [0u16; 64];
        let n = unsafe { GetClassNameW(HWND(hwnd as *mut _), &mut buf) };
        if n <= 0 {
            return false;
        }
        let class = String::from_utf16_lossy(&buf[..n as usize]);
        class.starts_with("gdkWindow") || class.starts_with("gdkSurface")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = hwnd;
        false
    }
}

/// Read a window's class name into an owned String. Best-effort; returns
/// `"<unknown>"` on any failure so it can drop straight into a diagnostic.
pub fn read_class_name(hwnd: u64) -> String {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
        if hwnd == 0 {
            return "<unknown>".into();
        }
        let mut buf = [0u16; 256];
        let n = unsafe { GetClassNameW(HWND(hwnd as *mut _), &mut buf) };
        if n <= 0 {
            return "<unknown>".into();
        }
        String::from_utf16_lossy(&buf[..n as usize])
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = hwnd;
        "<unknown>".into()
    }
}

/// Build the structured `background_unavailable` error returned when
/// `delivery_mode:"background"` would silently drop.
pub fn background_unavailable_error(
    hwnd: u64,
    kind: EventKind,
) -> cua_driver_core::protocol::ToolResult {
    let class = read_class_name(hwnd);
    let text = format!(
        "Background delivery is not available for target window class \
         '{class}' on this event kind ({}). Retry this action with \
         delivery_mode:\"foreground\"; Cua Driver will activate the target for \
         the action and restore the previous foreground afterward.",
        kind.name()
    );
    // Windows analog of the macOS escalation signal: this surface drops
    // background input, so the deliberate next rung is foreground delivery.
    cua_driver_core::delivery::background_unavailable_result(
        text,
        "background_unavailable",
        "background input is dropped by this surface; retry this \
         action with delivery_mode:\"foreground\".",
        serde_json::json!({
            "target_class": class,
            "event_kind": kind.name(),
        }),
    )
}

/// Build a structured background refusal while preserving the concrete
/// actuator failure. This is used after a background coordinate-injection
/// fallback was attempted and failed, so callers can distinguish a true
/// occlusion miss from a generic class-level refusal.
pub fn background_unavailable_error_with_cause(
    hwnd: u64,
    kind: EventKind,
    cause: impl Into<String>,
) -> cua_driver_core::protocol::ToolResult {
    let class = read_class_name(hwnd);
    let cause = cause.into();
    let cause_lower = cause.to_ascii_lowercase();
    let code = if cause_lower.contains("occluded") {
        "background_occluded"
    } else if cause_lower.contains("uipi") || cause_lower.contains("higher-integrity") {
        "background_uipi_blocked"
    } else {
        "background_unavailable"
    };
    let text = format!(
        "Background delivery is not available for target window class \
         '{class}' on this event kind ({}): {cause}. Retry this action with \
         delivery_mode:\"foreground\"; Cua Driver will activate the target for \
         the action and restore the previous foreground afterward.",
        kind.name()
    );
    cua_driver_core::delivery::background_unavailable_result(
        text,
        code,
        "background input could not be delivered by the coordinate \
         actuator; retry this action with delivery_mode:\"foreground\".",
        serde_json::json!({
            "target_class": class,
            "event_kind": kind.name(),
            "cause": cause,
        }),
    )
}

/// Query the Win32 `GetLastInputInfo` sensor for the elapsed milliseconds
/// since the user last generated physical keyboard or mouse input.
pub fn physical_input_idle_ms() -> Option<u64> {
    #[cfg(target_os = "windows")]
    unsafe {
        use windows::Win32::System::SystemInformation::GetTickCount;
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        if !GetLastInputInfo(&mut info).as_bool() {
            return None;
        }
        let now_ticks = GetTickCount();
        // `GetLastInputInfo` counts the driver's own `SendInput` too.
        OWN_INPUT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .idle_ms(now_ticks, info.dwTime)
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

/// The driver's own `SendInput` spans and the person's latest input seen
/// outside them.
#[cfg(target_os = "windows")]
static OWN_INPUT: std::sync::Mutex<cua_driver_core::delivery::OwnInputTracker> =
    std::sync::Mutex::new(cua_driver_core::delivery::OwnInputTracker::new());

/// `SendInput` for every driver injection. It records when the driver
/// injected, so the foreground idle gate does not read the driver's own last
/// action as the person's input and refuse the next one.
///
/// # Safety
///
/// The same as `SendInput`.
#[cfg(target_os = "windows")]
pub(crate) unsafe fn send_input(
    events: &[windows::Win32::UI::Input::KeyboardAndMouse::INPUT],
) -> u32 {
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetLastInputInfo, SendInput, INPUT, LASTINPUTINFO,
    };
    let mut tracker = OWN_INPUT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Keep the person's latest input before this injection overwrites the
    // OS's last-input tick.
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    let start = GetTickCount();
    if GetLastInputInfo(&mut info).as_bool() {
        tracker.observe(start, info.dwTime);
    }
    let sent = SendInput(events, std::mem::size_of::<INPUT>() as i32);
    if sent > 0 {
        tracker.record_injection(start, GetTickCount());
    }
    sent
}

/// Pure preflight evaluation for a Windows foreground delivery request.
pub fn gate_foreground_physical_idle_with_sample(
    hwnd: u64,
    kind: EventKind,
    observed_idle_ms: Option<u64>,
    threshold_ms: u64,
) -> Result<(), cua_driver_core::protocol::ToolResult> {
    use cua_driver_core::delivery::{
        evaluate_physical_input_idle, foreground_user_active_result, PhysicalInputIdleGate,
    };
    match evaluate_physical_input_idle(observed_idle_ms, threshold_ms) {
        PhysicalInputIdleGate::Idle { .. } => Ok(()),
        PhysicalInputIdleGate::UserActive {
            idle_ms,
            threshold_ms,
        } => Err(foreground_user_active_result(
            idle_ms,
            threshold_ms,
            serde_json::json!({
                "window_id": hwnd,
                "event_kind": kind.name(),
            }),
        )),
    }
}

/// Preflight gate before a `delivery_mode:"foreground"` swap + `SendInput`
/// on Windows: refuses with `foreground_user_active` when physical user
/// input occurred within the configured idle threshold.
pub fn gate_foreground_physical_idle(
    hwnd: u64,
    kind: EventKind,
) -> Result<(), cua_driver_core::protocol::ToolResult> {
    gate_foreground_physical_idle_with_sample(
        hwnd,
        kind,
        physical_input_idle_ms(),
        cua_driver_core::delivery::foreground_idle_threshold_ms(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_tk_toplevel_classes_without_matching_unrelated_windows() {
        assert!(is_tk_class_name("TkTopLevel"));
        assert!(is_tk_class_name("TkTopLevel.1"));
        assert!(!is_tk_class_name("TkChild"));
        assert!(!is_tk_class_name("Chrome_WidgetWin_1"));
    }

    #[test]
    fn wpf_refuses_posted_pointer_and_keyboard_events() {
        assert!(wpf_drops_event(EventKind::MouseClick, true));
        assert!(wpf_drops_event(EventKind::MouseMove, true));
        assert!(wpf_drops_event(EventKind::Keystroke, false));
        assert!(wpf_drops_event(EventKind::KeyCombo, false));
        assert!(!wpf_drops_event(EventKind::Keystroke, true));
        assert!(!wpf_drops_event(EventKind::KeyCombo, true));
        assert!(!wpf_drops_event(EventKind::TextInput, false));
        assert!(!wpf_drops_event(EventKind::MouseScroll, false));
    }

    #[test]
    fn tk_refuses_posted_clicks_and_keyboard_events() {
        assert!(tk_drops_event(EventKind::MouseClick));
        assert!(tk_drops_event(EventKind::Keystroke));
        assert!(tk_drops_event(EventKind::KeyCombo));
        assert!(tk_drops_event(EventKind::TextInput));
        assert!(!tk_drops_event(EventKind::MouseMove));
        assert!(!tk_drops_event(EventKind::MouseScroll));
    }

    #[test]
    fn delivery_mode_schema_keeps_persistent_activation_out_of_the_input_ladder() {
        let schema = delivery_mode_schema();
        let description = schema["description"]
            .as_str()
            .expect("delivery_mode description");
        assert!(description.contains("Only THEN re-issue the same action with 'foreground'"));
        assert!(!description.contains("bring_to_front"));
    }

    #[test]
    fn background_unavailable_with_cause_preserves_actuator_failure() {
        let result = background_unavailable_error_with_cause(
            0,
            EventKind::MouseClick,
            "target point is occluded by another window",
        );
        let structured = result.structured_content.as_ref().expect("structured");
        assert_eq!(result.is_error, Some(true));
        assert_eq!(structured["code"].as_str(), Some("background_occluded"));
        assert_eq!(structured["event_kind"].as_str(), Some("mouse_click"));
        assert!(structured["cause"]
            .as_str()
            .unwrap_or_default()
            .contains("occluded"));

        let text = match &result.content[0] {
            cua_driver_core::protocol::Content::Text { text, .. } => text,
            _ => panic!("expected text content"),
        };
        assert!(text.contains("occluded"), "result text lost cause: {text}");
        assert!(text.contains("Retry this action with delivery_mode:\"foreground\""));
        assert!(!text.contains("bring_to_front"));
        assert!(!structured["suggestion"]
            .as_str()
            .unwrap_or_default()
            .contains("bring_to_front"));

        let uipi = background_unavailable_error_with_cause(
            0,
            EventKind::MouseClick,
            "blocked by UIPI higher-integrity target",
        );
        assert_eq!(
            uipi.structured_content
                .as_ref()
                .and_then(|v| v["code"].as_str()),
            Some("background_uipi_blocked")
        );
    }

    #[test]
    fn windows_foreground_idle_gate_refuses_active_user_and_allows_idle() {
        let err = gate_foreground_physical_idle_with_sample(
            0x1234,
            EventKind::Keystroke,
            Some(220),
            1_000,
        )
        .unwrap_err();
        let structured = err.structured_content.expect("structured");
        assert_eq!(structured["code"], "foreground_user_active");
        assert_eq!(structured["effect"], "refused");
        assert_eq!(structured["dispatched"], false);
        assert_eq!(structured["window_id"], 0x1234);
        assert_eq!(structured["event_kind"], "keystroke");

        assert!(gate_foreground_physical_idle_with_sample(
            0x1234,
            EventKind::Keystroke,
            Some(1_200),
            1_000,
        )
        .is_ok());
    }

    /// The foreground idle gate reads `GetLastInputInfo`, which also counts
    /// `SendInput`. A foreground action right after the driver's own
    /// injection used to be refused as if the person were typing.
    #[cfg(target_os = "windows")]
    #[test]
    fn the_drivers_own_injection_does_not_trip_the_foreground_idle_gate() {
        let _serial = IDLE_GATE_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            GetLastInputInfo, INPUT, INPUT_0, INPUT_MOUSE, LASTINPUTINFO, MOUSEEVENTF_MOVE,
            MOUSEINPUT,
        };
        let last_input = || {
            let mut info = LASTINPUTINFO {
                cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
                dwTime: 0,
            };
            unsafe { GetLastInputInfo(&mut info) }
                .as_bool()
                .then_some(info.dwTime)
        };
        // A zero-distance relative move: input the OS counts, with no effect.
        let nudge = [INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dwFlags: MOUSEEVENTF_MOVE,
                    ..Default::default()
                },
            },
        }];
        let before = last_input();
        let sent = unsafe { send_input(&nudge) };
        let after = last_input();
        if sent != 1 || after.is_none() || after == before {
            eprintln!(
                "skipped: this session has no input desktop (SendInput {sent}, last input {before:?} -> {after:?})"
            );
            return;
        }
        // The idle time may not be shorter than the time since the input
        // before the driver's injection: the injection is not the person's.
        let idle = physical_input_idle_ms();
        let since_before = before.map(|tick| {
            let now = unsafe { windows::Win32::System::SystemInformation::GetTickCount() };
            cua_driver_core::delivery::elapsed_idle_ms_from_ticks(now, tick)
        });
        if let (Some(idle), Some(since_before)) = (idle, since_before) {
            assert!(
                idle + cua_driver_core::delivery::OWN_INPUT_TICK_SLACK_MS >= since_before,
                "the driver's own injection must not count as physical input \
                 (idle {idle} ms, last earlier input {since_before} ms ago)"
            );
        }
    }

    /// The idle-gate tests share the process-wide injection record and the
    /// OS's last-input tick.
    #[cfg(target_os = "windows")]
    static IDLE_GATE_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// `GetLastInputInfo` keeps only the latest input. A person's input
    /// between two driver injections must still refuse the next foreground
    /// action, although the second injection overwrote it.
    #[cfg(target_os = "windows")]
    #[test]
    fn input_between_two_driver_injections_still_trips_the_foreground_idle_gate() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            GetLastInputInfo, SendInput, INPUT, INPUT_0, INPUT_MOUSE, LASTINPUTINFO,
            MOUSEEVENTF_MOVE, MOUSEINPUT,
        };
        let _serial = IDLE_GATE_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let last_input = || {
            let mut info = LASTINPUTINFO {
                cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
                dwTime: 0,
            };
            unsafe { GetLastInputInfo(&mut info) }
                .as_bool()
                .then_some(info.dwTime)
        };
        // A zero-distance relative move: input the OS counts, with no effect.
        let nudge = [INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dwFlags: MOUSEEVENTF_MOVE,
                    ..Default::default()
                },
            },
        }];
        let before = last_input();
        let first = unsafe { send_input(&nudge) };
        let after = last_input();
        if first != 1 || after.is_none() || after == before {
            eprintln!(
                "skipped: this session has no input desktop (SendInput {first}, last input {before:?} -> {after:?})"
            );
            return;
        }
        // The person's input: a `SendInput` the driver did not make. Let
        // the tick counter move past the driver's span first.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let person = unsafe { SendInput(&nudge, std::mem::size_of::<INPUT>() as i32) };
        assert_eq!(person, 1, "the stand-in for physical input must be sent");
        std::thread::sleep(std::time::Duration::from_millis(200));
        // The driver's next batch overwrites the OS's last-input tick.
        assert_eq!(unsafe { send_input(&nudge) }, 1);
        let idle = physical_input_idle_ms();
        assert!(
            gate_foreground_physical_idle_with_sample(0, EventKind::Keystroke, idle, 5_000)
                .is_err(),
            "input between the driver's injections must still count (idle {idle:?})"
        );
    }
}
