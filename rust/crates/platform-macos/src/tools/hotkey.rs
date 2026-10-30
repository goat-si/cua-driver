use crate::goat::action_latency as latency;

use async_trait::async_trait;
use cua_driver_contract::HotkeyInput;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
    tool_args::parse_typed_projection,
};
use libc;
use serde_json::Value;
use std::sync::Arc;

use crate::apps;
use crate::focus_guard;
use crate::window_change_detector::WindowChangeDetector;

use super::ToolState;

pub struct HotkeyTool {
    state: Arc<ToolState>,
    #[cfg(test)]
    test_delivery: Option<Arc<TestDelivery>>,
}

#[cfg(test)]
type TestDelivery = dyn Fn(
        &str,
        &[&str],
        Option<std::time::Duration>,
        &crate::input::key_hold::HoldControl,
    ) -> anyhow::Result<()>
    + Send
    + Sync;

impl HotkeyTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self {
            state,
            #[cfg(test)]
            test_delivery: None,
        }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "hotkey".into(),
        description:
            "Press a key combination, e.g. `[\"cmd\", \"c\"]`: modifiers (cmd/command, shift, option/alt, \
                ctrl/control, fn) first, then one non-modifier key from the `press_key` vocabulary. It does not \
                raise the window.\n\
                • `background` (default): posts the combo to the target pid as trusted input (Chromium/Electron \
                accept it); with an AX target, that element is focused first. When the combo is a menu item's \
                key equivalent (cmd+s, cmd+w, ...) and the app is not in front, the menu item runs through \
                accessibility instead (a disabled Save is saved through the app's scripting interface and \
                verified on disk); a menu item macOS keeps disabled in the background is refused with \
                `background_unavailable` and no keys are sent.\n\
                • `foreground`: briefly fronts the window so native menu key equivalents dispatch, then restores \
                the prior frontmost app; with an AX target or x,y the focused field receives the chord (native \
                Chromium fields such as the omnibox need this). Requires window_id.\n\
                The result is `effect:\"unverifiable\"` (no read-back). A combo does not focus a text field: for \
                a backgrounded Electron input, pixel-click it first. Paste: `clipboard_write`, then \
                `clipboard_read` to check its types (and text) before selecting or replacing editor content; \
                send cmd+v only after that check succeeds."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["keys"],
            "properties": {
                "session": { "type": "string", "description": "For multi-call work, prefer a short public session label and repeat it on every call that accepts it. Omit it to use the authenticated transport's implicit lifecycle session." },
                "pid": { "type": "integer", "description": "Target process ID. Required with x, y; optional with element_token (the token carries the pid)." },
                "duration_ms": { "type": "integer", "minimum": 0, "maximum": 30000, "description": "Key-down hold in milliseconds; omitted uses the configured key gap." },
                "keys": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 2,
                    "description": "Modifier(s) and one non-modifier key, e.g. [\"cmd\", \"c\"]."
                },
                "x": { "type": "number", "description": "Screenshot-pixel X — the element px action form: pixel-click there to focus, then send the combo (so e.g. Cmd+V pastes into that field). Pass with y. Use for Chromium/Electron surfaces the background combo can't reach." },
                "y": { "type": "number", "description": "Screenshot-pixel Y (see x)." },
                "window_id": {
                    "type": "integer",
                    "description": "Target window. Required for delivery_mode:\"foreground\" (the NSMenu activation needs a window). Does NOT itself raise the window — raising is gated on delivery_mode."
                },
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "scope": { "type": "string", "enum": ["window", "desktop"], "default": "window", "description": "Use desktop with no pid/window_id to send the chord to the frontmost application." },
                "delivery_mode": cua_driver_core::tool_schema::delivery_mode_schema()
            },
            "additionalProperties": false
        }),
        read_only: false,
        destructive: true,
        idempotent: false,
        open_world: true,
    })
}

/// Cmd+V and its paste variants (Cmd+Shift+V, Cmd+Opt+Shift+V).
fn is_paste_combo(modifiers: &[String], key: &str) -> bool {
    key.eq_ignore_ascii_case("v")
        && modifiers.iter().any(|m| {
            matches!(
                m.to_ascii_lowercase().as_str(),
                "cmd" | "command" | "meta" | "super"
            )
        })
}

/// Modifier key names — split the keys array into modifiers + base key.
fn is_modifier(k: &str) -> bool {
    matches!(
        k.to_lowercase().as_str(),
        "cmd"
            | "command"
            | "super"
            | "meta"
            | "win"
            | "shift"
            | "option"
            | "alt"
            | "ctrl"
            | "control"
            | "fn"
    )
}

const HOTKEY_FOCUS_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);
const HOTKEY_FOCUS_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

pub(super) async fn run_hotkey_owned<L, S, F, R>(
    lease: L,
    scope: S,
    control: Arc<crate::input::key_hold::HoldControl>,
    work: F,
) -> Result<R, tokio::task::JoinError>
where
    L: Send + 'static,
    S: Send + 'static,
    F: FnOnce(Arc<crate::input::key_hold::HoldControl>, &mut S) -> R + Send + 'static,
    R: Send + 'static,
{
    super::key_delivery_guard::run_owned(lease, scope, control, work).await
}

fn focus_hotkey_element(pid: i32, element_ptr: usize) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + HOTKEY_FOCUS_TIMEOUT;
    loop {
        crate::input::ax_actions::focus_element(element_ptr)?;
        if crate::input::ax_actions::is_element_focused(pid, element_ptr) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("requested hotkey element did not become focused");
        }
        std::thread::sleep(HOTKEY_FOCUS_POLL_INTERVAL);
    }
}

fn screen_sharing_modifier_delivery_error(
    is_screen_sharing: bool,
    has_modifiers: bool,
    foreground: bool,
    window_id: Option<u32>,
) -> Option<ToolResult> {
    if !is_screen_sharing || !has_modifiers || (foreground && window_id.is_some()) {
        return None;
    }
    Some(
        ToolResult::error(
            "Screen Sharing modifier hotkeys require delivery_mode:\"foreground\" and window_id \
             so Cua Driver can deliver physical modifier transitions safely.",
        )
        .with_structured(serde_json::json!({
            "code": "SCREEN_SHARING_REQUIRES_FOREGROUND_HID",
            "effect": "refused",
            "escalation": {
                "recommended": "foreground",
                "reason": "Screen Sharing does not forward modifier state from background \
                           PID-routed base-key events.",
                "requires": ["window_id"]
            }
        })),
    )
}

/// The keys a call names, as typed, with the resolved key and modifiers, or
/// the refusal for missing or unknown ones. Pure: it reads only the arguments.
pub(super) fn requested_hotkey(
    args: &Value,
) -> Result<(Vec<String>, String, Vec<String>), ToolResult> {
    use cua_driver_core::tool_args::ArgsExt;
    if args.get("keys").and_then(|v| v.as_array()).is_none() {
        return Err(super::invalid_arguments("Missing required parameter: keys"));
    }
    let raw_keys = args.str_array("keys");

    if raw_keys.is_empty() {
        return Err(super::invalid_arguments(
            "keys must be a non-empty array of strings.",
        ));
    }

    // Split: modifiers are all entries that are modifier names; base key is everything else.
    // Typically: last non-modifier is the key; all others are modifiers.
    let modifiers: Vec<String> = raw_keys
        .iter()
        .filter(|k| is_modifier(k))
        .cloned()
        .collect();
    let non_modifiers: Vec<String> = raw_keys
        .iter()
        .filter(|k| !is_modifier(k))
        .cloned()
        .collect();

    if non_modifiers.is_empty() {
        return Err(super::invalid_arguments(
            "keys must include at least one non-modifier key (e.g. \"c\" in [\"cmd\", \"c\"]).",
        ));
    }

    // Use the last non-modifier key; if there are multiple, treat earlier ones as extra keys.
    // A symbol typed with Shift becomes its key plus shift, and an unknown key or modifier
    // is refused here, before any menu, focus or key work.
    let (key, modifiers) = match crate::input::keyboard::resolve_key_press(
        non_modifiers.last().unwrap(),
        &modifiers,
    ) {
        Ok(resolved) => resolved,
        Err(error) => return Err(super::press_key::invalid_key(error)),
    };
    Ok((raw_keys, key, modifiers))
}

#[async_trait]
impl Tool for HotkeyTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let duration = match super::key_delivery_guard::duration_from_args(&args) {
            Ok(duration) => duration,
            Err(refusal) => return refusal,
        };
        if args.opt_str("scope").as_deref() == Some("desktop")
            && args.get("pid").is_none()
            && args.get("window_id").is_none()
        {
            let input = match parse_typed_projection::<HotkeyInput>("hotkey", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let raw_keys = input.keys;
            if raw_keys.len() < 2 {
                return super::invalid_arguments("hotkey.keys must contain at least two keys.");
            }
            let modifiers: Vec<String> = raw_keys
                .iter()
                .filter(|key| is_modifier(key))
                .cloned()
                .collect();
            let Some(key) = raw_keys.iter().rev().find(|key| !is_modifier(key)).cloned() else {
                return super::invalid_arguments(
                    "keys must include at least one non-modifier key for desktop hotkey",
                );
            };
            let display = raw_keys.join("+");
            let control = Arc::new(crate::input::key_hold::HoldControl::default());
            #[cfg(test)]
            let test_delivery = self.test_delivery.clone();
            let result = run_hotkey_owned((), (), control, move |control, _scope| {
                if control.is_cancelled() {
                    anyhow::bail!("key delivery cancelled");
                }
                let modifier_refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                #[cfg(test)]
                if let Some(delivery) = test_delivery {
                    return delivery(&key, &modifier_refs, duration, &control);
                }
                crate::input::key_hold::deliver(
                    crate::input::key_hold::KeyRoute::Global,
                    &key,
                    &modifier_refs,
                    duration,
                    &control,
                )
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!("Pressed desktop hotkey {display}."))
                    .with_structured(serde_json::json!({
                        "scope": "desktop",
                        "path": "hid",
                        "effect": "unverifiable"
                    })),
                Ok(Err(error)) => ToolResult::error(format!("desktop hotkey failed: {error}")),
                Err(error) => ToolResult::error(format!("desktop hotkey task failed: {error}")),
            };
        }

        // Validate the keys first: an unknown key is refused before any
        // menu, focus, target or key work.
        let (raw_keys, key, modifiers) = match requested_hotkey(&args) {
            Ok(requested) => requested,
            Err(refusal) => return refusal,
        };
        let pid = match super::target_pid(&self.state, &args) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let key_display = raw_keys.join("+");
        let window_id_arg = args.opt_u64("window_id");
        let resolved = match latency::resolve(|| self.state.snapshots.resolve(pid, &args)) {
            Ok(resolved) => resolved,
            Err(error) => return error,
        };
        let (element_index, window_id, element_guard) = resolved.into_parts(window_id_arg);
        let window_id = match super::native_window_id(window_id) {
            Ok(window_id) => window_id,
            Err(error) => return error,
        };
        // delivery_mode gates whether we raise: background (default) never fronts
        // the window — passing window_id only targets the combo. foreground is the
        // explicit NSMenu-activation rung for menu shortcuts that ignore a
        // background combo (matches click/type_text).
        let delivery_mode = super::DeliveryMode::parse(args.opt_str("delivery_mode").as_deref());
        let fg = delivery_mode.is_foreground();
        // The person is working in this app: nothing is sent.
        if !fg {
            if let Err(refusal) = super::gate_user_takeover(pid, window_id) {
                return refusal;
            }
        }
        let px = args.get("x").and_then(|value| value.as_f64());
        let py = args.get("y").and_then(|value| value.as_f64());
        if px.is_some() && py.is_some() && element_index.is_some() {
            return super::invalid_arguments(
                "Pass either element_token (ax) or x,y (px) to hotkey, not both.",
            );
        }

        let element_ptr = element_guard.as_ref().map(|guard| guard.as_ptr());

        let screen_sharing_target = crate::input::keyboard::is_screen_sharing_pid(pid);
        if let Some(error) = screen_sharing_modifier_delivery_error(
            screen_sharing_target,
            !modifiers.is_empty(),
            fg,
            window_id,
        ) {
            return error;
        }

        // ── Menu key equivalents in a background app ──
        // Background key events reach a window's views but never its app's
        // menu key equivalents, so a background cmd+s to TextEdit saved
        // nothing while reporting "Pressed". When the combo is a menu item's
        // key equivalent in a native app that is not in front, run that menu
        // item in the background instead (verified for Save), or refuse when
        // macOS keeps it disabled. No key events are sent on this route.
        let mut key_scope: Option<crate::input::skylight::WindowKeyScope> = None;
        if !fg && px.is_none() {
            if let (Some(wid), Some(combo)) = (
                window_id,
                super::menu_equivalent::Combo::parse(&modifiers, &key),
            ) {
                let menu_item = latency::spawn_action_blocking(move || {
                    let native = !super::type_text::is_browser_pid(pid)
                        && !crate::browser::electron_js::ElectronJs::is_electron(pid);
                    let in_front = apps::frontmost_pid() == Some(pid);
                    if native && !in_front {
                        super::menu_equivalent::find(pid, &combo)
                    } else {
                        None
                    }
                })
                .await
                .ok()
                .flatten();
                if let Some(item) = menu_item {
                    // The menu command acts on the app's key window. Make the
                    // addressed window key first, confirmed, or refuse: never
                    // act on another window (CUA-1767).
                    // Held until the menu route answers.
                    let _menu_key_scope =
                        match super::acquire_key_scope(pid, Some(wid), false).await {
                            Ok(scope) => scope,
                            Err(refusal) => return refusal,
                        };
                    if let Some(ptr) = element_ptr {
                        // The menu command acts on the focused element.
                        let focused =
                            latency::spawn_action_blocking(move || focus_hotkey_element(pid, ptr))
                                .await;
                        if !matches!(focused, Ok(Ok(()))) {
                            return ToolResult::error(
                                "hotkey: the requested element did not take focus, so its menu \
                                 command was not run.",
                            );
                        }
                    }
                    let menu_result = super::invoke_menu::run_in_background(
                        pid,
                        wid,
                        item.path,
                        super::invoke_menu::MenuRequest::KeyEquivalent(key_display.clone()),
                        super::invoke_menu::KeyOwner::Caller,
                    )
                    .await;
                    // Background key events never reach a native app's menus, so a menu
                    // that cannot run in the background is refused without sending keys
                    // that would only look delivered.
                    return super::key_delivery_guard::semantic_duration(
                        menu_refusal_without_keys(menu_result),
                        duration,
                    );
                }
            }
        }

        // ── Paste must not land in browser chrome ──
        // A background Cmd+V goes to the window's first responder. In a
        // browser that can be the omnibox rather than the page field the
        // caller meant, which pastes (possibly secret) clipboard content into
        // the address bar. With no element/pixel target, refuse when the
        // window's focused field is positively browser chrome.
        if !fg && px.is_none() && element_index.is_none() && is_paste_combo(&modifiers, &key) {
            if let Some(wid) = window_id {
                let chrome = latency::spawn_action_blocking(move || {
                    super::type_text::is_browser_pid(pid)
                        && super::type_text::window_focus_is_browser_chrome_field(pid, wid)
                })
                .await
                .unwrap_or(false);
                if chrome {
                    return ToolResult::error(
                        "hotkey: the browser's focused field is its own address/search bar, \
                         not a page field, so the paste was not sent. Focus the page field \
                         first (type_text with x,y, or a snapshot element_token), or use \
                         browser_type for page text.",
                    )
                    .with_structured(serde_json::json!({
                        "code": "paste_target_is_browser_chrome",
                        "delivered": false
                    }));
                }
            }
        }

        // ── Exact-target background gate (macOS background input v1) ──
        // A window-addressed background combo is process-scoped transport: it
        // must prove exact delivery to the requested window (fresh AXWindows
        // membership, not minimized/hidden, no competing same-pid keyboard
        // destination) BEFORE anything is sent — including the px focus
        // click. delivery_mode:"foreground" stays the caller's explicit last
        // resort and is not gated here.
        let _mutation_lease = if !fg {
            if let Some(wid) = window_id {
                match super::gate_background_window_action(
                    pid,
                    wid,
                    element_ptr,
                    super::window_keyboard_action(
                        cua_driver_core::background_input::BackgroundAction::GenericKey,
                    ),
                )
                .await
                {
                    Ok(lease) => Some(lease),
                    Err(refusal_result) => return refusal_result,
                }
            } else {
                None
            }
        } else {
            None
        };

        // Key post: the addressed window must be the confirmed key window.
        if key_scope.is_none() {
            match super::acquire_key_scope_with_eligibility(
                pid,
                window_id,
                fg,
                element_ptr.is_none() && px.is_none() && py.is_none(),
            )
            .await
            {
                Ok(scope) => key_scope = scope,
                Err(refusal) => return refusal,
            }
        }

        // Web-content AX nodes can acknowledge AXFocused without moving the
        // renderer's real first responder. That makes a direct focus write an
        // unsafe oracle for Chromium/WebKit hotkeys: the following PID/HID
        // chord can still land on the renderer's remembered control. Resolve
        // the requested AX node's exact center and reuse the proven PX focus
        // ladder for web areas. The request remains snapshot-bound AX
        // targeting; only the focus transport falls back through a hit-test
        // and, on the explicit foreground rung, a real click when required.
        let web_ax_focus_xy = if let (Some(guard), Some(wid), Some(index)) =
            (element_guard.clone(), window_id, element_index)
        {
            let web_guard = guard.clone();
            let is_web = latency::spawn_action_blocking(move || {
                super::type_text::target_in_web_area(
                    pid,
                    Some((web_guard.as_ptr(), Some(index))),
                    Some(wid),
                )
            })
            .await
            .unwrap_or(true);
            if is_web {
                latency::spawn_action_blocking(move || unsafe {
                    let (screen_x, screen_y) = crate::ax::bindings::element_screen_center(
                        guard.as_ptr() as crate::ax::bindings::AXUIElementRef,
                    )?;
                    let frame = super::px_frame::resolve_window_px_frame(wid).ok()?;
                    Some((
                        (screen_x - frame.bounds.x) * frame.scale,
                        (screen_y - frame.bounds.y) * frame.scale,
                    ))
                })
                .await
                .unwrap_or(None)
            } else {
                None
            }
        } else {
            None
        };

        // PX form, plus the web-content AX fallback above: focus the field
        // before sending the combo. Foreground delivery still needs to front
        // the target for the chord itself because the focus helper restores
        // the previous app before returning.
        let coordinate_focus = {
            let focus_xy = match ((px, py), web_ax_focus_xy) {
                ((Some(cx), Some(cy)), _) => Some((cx, cy)),
                ((None, None), Some(center)) => Some(center),
                _ => None,
            };
            if let Some((cx, cy)) = focus_xy {
                let from_zoom = args
                    .get("from_zoom")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if let Err(e) = super::focus_by_pixel(
                    &self.state,
                    pid,
                    window_id,
                    cx,
                    cy,
                    fg,
                    args.opt_str("session"),
                    args.opt_str("_session_id"),
                    from_zoom,
                    _mutation_lease.as_ref(),
                    // A hotkey combo (e.g. Cmd+C on a selected cell) is
                    // not a printable insertion: no cell guard here.
                    false,
                )
                .await
                {
                    return e;
                }
                true
            } else {
                false
            }
        };

        // A focus click already moved the cursor. Otherwise place a named
        // session's cursor on the element, its remembered position, or the
        // window centre, so a keyboard-first session stays visible.
        if !coordinate_focus {
            let element_center = match element_guard.clone() {
                Some(guard) => latency::spawn_action_blocking(move || unsafe {
                    crate::ax::bindings::element_screen_center(
                        guard.as_ptr() as crate::ax::bindings::AXUIElementRef
                    )
                })
                .await
                .ok()
                .flatten(),
                None => None,
            };
            super::cursor_tools::position_keyboard_cursor(
                &self.state,
                &args,
                window_id,
                element_center,
            )
            .await;
        }

        // ── Focus-suppression wrap (Swift WindowChangeDetector + FocusGuard) ──
        // Hotkeys like Cmd+N, Cmd+W, Cmd+T explicitly open/close
        // windows. The NSMenu path also briefly activates the target via
        // SLPSSetFrontProcessWithOptions which can race the wildcard
        // suppressor — wrapping ensures both side-effects are observed
        // and the prior frontmost is restored if the activation lingers.
        let prior_front = apps::frontmost_pid();
        let snapshot = WindowChangeDetector::snapshot(prior_front);

        let control = Arc::new(crate::input::key_hold::HoldControl::default());
        let result = focus_guard::with_focus_suppressed(
            Some(pid),
            prior_front,
            "hotkey.CGEvent",
            || async move {
                run_hotkey_owned(_mutation_lease, key_scope, control, move |control, key_scope| {
                    if control.is_cancelled() { anyhow::bail!("key delivery cancelled"); }
                    let element_ptr = element_guard.as_ref().map(|guard| guard.as_ptr());
                    let m: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                    if !fg && element_ptr.is_none() {
                        if let Some(wid) = window_id {
                            let cached = key_scope.as_ref().is_some_and(|scope| scope.used_belief());
                            let sample = super::key_delivery_guard::final_window_sample(pid, wid, key_scope)?;
                            super::press_key::verify_final_key_sample_with(pid, wid, false, cached, sample, |sample| match sample {
                                Some(window) => crate::ax::exact_target::check_window_focus_conflict_with_sample(pid, wid, None, window),
                                None => crate::ax::exact_target::check_window_focus_conflict(pid, wid, None),
                            })?;
                        }
                    }
                    match (fg, coordinate_focus, window_id, element_ptr) {
                        // Chrome's native omnibox and Chromium/Electron inputs
                        // require a genuine foreground HID chord. Keep the exact
                        // target frontmost until both key events are consumed;
                        // otherwise Cmd+A/Cmd+V can be silently ignored.
                        (true, true, Some(wid), _) => {
                            crate::input::skylight::with_foreground_hid_activation(
                                pid as libc::pid_t,
                                wid,
                                || {
                                    if screen_sharing_target {
                                        crate::input::key_hold::deliver(crate::input::key_hold::KeyRoute::BareGlobal, &key, &m, duration, &control)
                                    } else {
                                        crate::input::key_hold::deliver(crate::input::key_hold::KeyRoute::Global, &key, &m, duration, &control)
                                    }
                                },
                            )?;
                            Ok(())
                        }
                        // An AX-addressed chord has the same renderer-focus
                        // requirement as the px form. Activate the exact window,
                        // establish and confirm the requested child focus after
                        // activation, then use the guarded global HID queue.
                        (true, false, Some(wid), Some(ptr)) => {
                            crate::input::skylight::with_foreground_hid_activation(
                                pid as libc::pid_t,
                                wid,
                                || {
                                    focus_hotkey_element(pid, ptr)?;
                                    crate::input::key_hold::deliver(crate::input::key_hold::KeyRoute::BareGlobal, &key, &m, duration, &control)
                                },
                            )?;
                            Ok(())
                        }
                        (true, false, Some(wid), None) => {
                            crate::input::skylight::with_foreground_hid_activation(
                                pid as libc::pid_t,
                                wid,
                                || {
                                    crate::input::key_hold::deliver(crate::input::key_hold::KeyRoute::BareGlobal, &key, &m, duration, &control)
                                },
                            )?;
                            Ok(())
                        }
                        // background (default): auth-envelope post to the pid, no
                        // raise — even when window_id was supplied for targeting.
                        (false, false, _, Some(ptr)) => {
                            focus_hotkey_element(pid, ptr)?;
                            crate::input::key_hold::deliver(crate::input::key_hold::KeyRoute::Pid(pid), &key, &m, duration, &control)
                        }
                        _ => crate::input::key_hold::deliver(crate::input::key_hold::KeyRoute::Pid(pid), &key, &m, duration, &control),
                    }
                })
                .await
            },
        ).await;

        let changes = super::finish_window_observation(snapshot).await;

        match result {
            Ok(Ok(())) => {
                let label = if fg {
                    " (delivery_mode:foreground)"
                } else {
                    ""
                };
                ToolResult::text(format!(
                    "Pressed {key_display} on pid {pid}{label}. Read the window before \
                     sending it again.{}",
                    changes.result_suffix()
                ))
                .with_structured(delivered_structured(fg))
            }
            Ok(Err(e)) if e.is::<super::press_key::WindowFocusConflictError>() => {
                super::press_key::delivery_failed(e)
            }
            Ok(Err(e)) => ToolResult::error(format!("hotkey failed: {e}")),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

/// The menu route's answer, with a menu macOS keeps unavailable in the
/// background marked as a refusal that sent nothing. Keys are not tried
/// instead: a background key event never reaches a native app's menus, so it
/// would report delivery and change nothing (a background cmd+[ to Finder).
pub(super) fn menu_refusal_without_keys(mut result: ToolResult) -> ToolResult {
    let unavailable = result.is_error == Some(true)
        && result
            .structured_content
            .as_ref()
            .and_then(|v| v["code"].as_str())
            == Some("background_unavailable");
    if !unavailable {
        return result;
    }
    if let Some(serde_json::Value::Object(fields)) = result.structured_content.as_mut() {
        fields.insert("effect".into(), serde_json::json!("refused"));
        fields.insert("dispatched".into(), serde_json::json!(false));
        fields
            .entry("keys_sent")
            .or_insert(serde_json::json!(false));
        fields.entry("escalation").or_insert(
            serde_json::json!({ "recommended": "foreground", "requires": ["window_id"] }),
        );
    }
    result
}

fn delivered_structured(foreground: bool) -> Value {
    serde_json::json!({
        "path": if foreground { "key_events_fg" } else { "key_events" },
        "verified": false,
        "effect": "unverifiable",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn cancel_real_desktop_hotkey(base_down: bool) {
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let ready = std::sync::Mutex::new(Some(ready_tx));
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let resume = std::sync::Mutex::new(resume_rx);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let done = std::sync::Mutex::new(Some(done_tx));
        let worker_events = events.clone();
        let delivery: Arc<TestDelivery> = Arc::new(move |key, modifiers, duration, control| {
            let result = crate::input::key_hold::deliver_with(
                key,
                modifiers,
                duration.unwrap(),
                |code, down, _| {
                    worker_events.lock().unwrap().push((code, down));
                    if down && ((code == 55 && !base_down) || (code != 55 && base_down)) {
                        ready.lock().unwrap().take().unwrap().send(()).unwrap();
                        resume.lock().unwrap().recv().unwrap();
                    }
                    Ok(())
                },
                || control.is_cancelled(),
                |_| control.is_cancelled(),
            );
            done.lock().unwrap().take().unwrap().send(()).unwrap();
            result
        });
        let mut tool = HotkeyTool::new(Arc::new(ToolState::new(false, false, None)));
        tool.test_delivery = Some(delivery);
        let invoking = tokio::spawn(async move {
            tool.invoke(
                serde_json::json!({"scope":"desktop", "keys":["cmd", "a"], "duration_ms":30000}),
            )
            .await
        });
        ready_rx.await.unwrap();
        invoking.abort();
        assert!(matches!(invoking.await, Err(error) if error.is_cancelled()));
        resume_tx.send(()).unwrap();
        done_rx.await.unwrap();
        let expected = if base_down {
            vec![(55, true), (0, true), (0, false), (55, false)]
        } else {
            vec![(55, true), (55, false)]
        };
        assert_eq!(*events.lock().unwrap(), expected);
    }

    #[tokio::test]
    async fn desktop_hotkey_cancellation_releases_real_dispatch_transitions() {
        cancel_real_desktop_hotkey(false).await;
        cancel_real_desktop_hotkey(true).await;
    }

    #[test]
    fn duration_schema_accepts_only_bounded_integer_holds() {
        let duration = &def().input_schema["properties"]["duration_ms"];
        assert_eq!(duration["type"], "integer");
        assert_eq!(duration["minimum"], 0);
        assert_eq!(duration["maximum"], 30_000);
    }

    #[tokio::test]
    async fn invalid_duration_is_refused_before_target_resolution() {
        let tool = HotkeyTool::new(Arc::new(ToolState::new(false, false, None)));
        let mut receipts = Vec::new();
        for duration in [
            serde_json::json!(-1),
            serde_json::json!(30001),
            serde_json::json!(1.5),
        ] {
            let result = tool
                .invoke(serde_json::json!({
                    "keys": ["cmd", "a"], "element_token": "s7fffffff:0", "duration_ms": duration
                }))
                .await;
            let structured = result.structured_content.expect("structured refusal");
            receipts.push(structured);
        }
        assert!(
            receipts.iter().all(|r| r["code"] == "invalid_arguments"
                && r["effect"] == "refused"
                && r["dispatched"] == false),
            "{receipts:?}"
        );
    }

    #[tokio::test]
    async fn valid_duration_bounds_reach_target_resolution_without_dispatch() {
        let tool = HotkeyTool::new(Arc::new(ToolState::new(false, false, None)));
        for duration in [0, 30_000] {
            let result = tool
                .invoke(serde_json::json!({
                    "keys": ["cmd", "a"], "element_token": "s7fffffff:0", "duration_ms": duration
                }))
                .await;
            let structured = result.structured_content.expect("structured refusal");
            assert_eq!(
                structured["refusal"]["code"], "stale_element_token",
                "{duration}: {structured}"
            );
        }
    }

    /// A refusal after menu validation posted Cmd+F20 keeps saying so.
    #[test]
    fn a_menu_refusal_keeps_the_validation_keys_it_sent() {
        let posted = menu_refusal_without_keys(ToolResult::error("disabled").with_structured(
            serde_json::json!({
                "code": "background_unavailable",
                "keys_sent": true,
                "dispatch": "validation_posted",
            }),
        ));
        let structured = posted.structured_content.expect("structured");
        assert_eq!(structured["keys_sent"], true);
        assert_eq!(structured["dispatch"], "validation_posted");
        assert_eq!(structured["dispatched"], false);
    }

    #[test]
    fn an_unavailable_background_menu_is_refused_without_keys() {
        let unavailable = menu_refusal_without_keys(
            ToolResult::error("menu disabled in the background")
                .with_structured(serde_json::json!({ "code": "background_unavailable" })),
        );
        let structured = unavailable.structured_content.expect("structured");
        assert_eq!(structured["effect"], "refused");
        assert_eq!(structured["dispatched"], false);
        assert_eq!(structured["keys_sent"], false);
        assert_eq!(structured["escalation"]["recommended"], "foreground");
        // Other menu answers pass through as they are.
        let other = menu_refusal_without_keys(
            ToolResult::error("refused")
                .with_structured(serde_json::json!({ "code": "window_not_key" })),
        );
        assert!(other.structured_content.unwrap().get("keys_sent").is_none());
        let done = menu_refusal_without_keys(ToolResult::text("done"));
        assert_ne!(done.is_error, Some(true));
    }

    #[test]
    fn a_delivered_combo_suggests_no_resend() {
        for foreground in [false, true] {
            let structured = delivered_structured(foreground);
            assert!(structured.get("escalation").is_none());
            assert_eq!(structured["effect"], "unverifiable");
            assert_eq!(structured["verified"], false);
        }
    }

    #[tokio::test]
    async fn a_token_only_hotkey_takes_its_pid_from_the_token() {
        let tool = HotkeyTool::new(Arc::new(ToolState::new(false, false, None)));
        let result = tool
            .invoke(serde_json::json!({
                "keys": ["cmd", "c"],
                "element_token": "s7fffffff:0",
            }))
            .await;
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.expect("structured refusal");
        assert_eq!(structured["refusal"]["code"], "stale_element_token");
    }

    #[tokio::test]
    async fn an_unknown_key_is_refused_before_the_target_is_resolved() {
        let tool = HotkeyTool::new(Arc::new(ToolState::new(false, false, None)));
        for args in [
            serde_json::json!({"keys": ["cmd", "notakey"], "element_token": "s7fffffff:0"}),
            serde_json::json!({"keys": ["cmd", "notakey"]}),
        ] {
            let result = tool.invoke(args.clone()).await;
            let structured = result.structured_content.expect("structured refusal");
            assert_eq!(
                structured["code"], "invalid_arguments",
                "{args}: {structured}"
            );
            assert!(
                structured["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("notakey")),
                "{args}: {structured}"
            );
        }
    }

    #[tokio::test]
    async fn malformed_keys_are_refused_before_target_lookup_or_dispatch() {
        let tool = HotkeyTool::new(Arc::new(ToolState::new(false, false, None)));
        let result = tool.invoke(serde_json::json!({"pid": 123})).await;
        let structured = result.structured_content.expect("structured refusal");
        assert_eq!(structured["code"], "invalid_arguments");
        assert_eq!(structured["effect"], "refused");
        assert_eq!(structured["dispatched"], false);
    }

    #[test]
    fn paste_combo_detection() {
        let m = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(is_paste_combo(&m(&["cmd"]), "v"));
        assert!(is_paste_combo(&m(&["Command", "shift"]), "V"));
        assert!(!is_paste_combo(&m(&["ctrl"]), "v"));
        assert!(!is_paste_combo(&m(&["cmd"]), "c"));
    }

    #[test]
    fn hotkey_contract_accepts_snapshot_bound_ax_targets() {
        let properties = def().input_schema["properties"]
            .as_object()
            .expect("hotkey properties");
        assert!(properties.contains_key("element_token"));
    }

    #[test]
    fn screen_sharing_modifier_hotkeys_fail_closed_without_foreground_window() {
        for (foreground, window_id) in [(false, None), (false, Some(7)), (true, None)] {
            let result = screen_sharing_modifier_delivery_error(true, true, foreground, window_id)
                .expect("unsafe Screen Sharing modifier route must be refused");
            assert_eq!(result.is_error, Some(true));
            let structured = result.structured_content.unwrap();
            assert_eq!(structured["code"], "SCREEN_SHARING_REQUIRES_FOREGROUND_HID");
            assert_eq!(structured["effect"], "refused");
            assert_eq!(structured["escalation"]["recommended"], "foreground");
            assert_eq!(structured["escalation"]["requires"][0], "window_id");
        }
        assert!(screen_sharing_modifier_delivery_error(true, true, true, Some(7)).is_none());
        assert!(screen_sharing_modifier_delivery_error(true, false, false, None).is_none());
        assert!(screen_sharing_modifier_delivery_error(false, true, false, None).is_none());
    }
}
