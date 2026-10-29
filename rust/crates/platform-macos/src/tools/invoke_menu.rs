use crate::goat::action_latency as latency;

use async_trait::async_trait;
use core_foundation::base::{CFRelease, CFTypeRef};
use cua_driver_contract::InvokeMenuInput;
use cua_driver_core::{
    action_record::{
        ActionEffect, ActionEvidence, ActionExecutionRecord, ActionTransport, ActualDelivery,
        EvidenceKind, RequestedDelivery,
    },
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
        Arc,
    },
    time::Duration,
};

use crate::ax::bindings::{
    ax_get_window_id, copy_action_names, copy_ax_windows, copy_bool_attr, copy_children,
    copy_element_attr, copy_string_attr, kAXErrorSuccess, perform_action, set_bool_attr_true,
    AXError, AXUIElementCreateApplication, AXUIElementRef, AXUIElementSetMessagingTimeout,
};

pub struct InvokeMenuTool;

const AX_MESSAGING_TIMEOUT_SECONDS: f32 = 2.0;
const MAIN_QUEUE_TIMEOUT: Duration = Duration::from_secs(5);

unsafe fn set_messaging_timeout(element: AXUIElementRef) {
    let _ = AXUIElementSetMessagingTimeout(element, AX_MESSAGING_TIMEOUT_SECONDS);
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| {
        let contract =
            cua_driver_contract::tool_contract("invoke_menu").expect("invoke_menu contract");
        ToolDef {
            name: contract.name,
            description: contract.description,
            input_schema: contract.input_schema,
            read_only: contract.annotations.read_only,
            destructive: contract.annotations.destructive,
            idempotent: contract.annotations.idempotent,
            open_world: contract.annotations.open_world,
        }
    })
}

fn normalized_path(path: Vec<String>) -> Result<Vec<String>, String> {
    if path.is_empty() || path.len() > 16 {
        return Err("invoke_menu: path must contain between 1 and 16 segments".into());
    }
    path.into_iter()
        .enumerate()
        .map(|(index, segment)| {
            let segment = segment.trim();
            if segment.is_empty() {
                Err(format!("invoke_menu: path segment {index} is empty"))
            } else {
                Ok(segment.to_owned())
            }
        })
        .collect()
}

/// Return the semantic menu children of an AX node. AppKit inserts an
/// untitled `AXMenu` container between a menu-bar/submenu item and its items;
/// callers express paths in visible labels, so that container is transparent.
unsafe fn semantic_children(parent: AXUIElementRef) -> Vec<AXUIElementRef> {
    let mut out = Vec::new();
    for child in copy_children(parent) {
        if copy_string_attr(child, "AXRole").as_deref() == Some("AXMenu") {
            out.extend(copy_children(child));
            CFRelease(child as CFTypeRef);
        } else {
            out.push(child);
        }
    }
    out
}

/// A menu title as paths compare it: trimmed, with three periods read as
/// the ellipsis character macOS menu titles use ("Save As..." finds
/// "Save As…").
fn menu_title_key(title: &str) -> String {
    title.trim().replace("...", "\u{2026}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TitleMatch {
    One(usize),
    Ambiguous,
    None,
}

/// Find a path segment among the titles of one menu level: an exact title
/// first, then a title that differs only in letter case. Anything looser
/// (word overlap, prefixes) is left to the caller, who sees the titles in
/// the error, because a wrong guess would run a different command.
fn pick_title(titles: &[String], segment: &str) -> TitleMatch {
    let wanted = menu_title_key(segment);
    let pick = |same: &dyn Fn(&str) -> bool| {
        let mut hits = titles
            .iter()
            .enumerate()
            .filter(|(_, title)| same(&menu_title_key(title)))
            .map(|(index, _)| index);
        match (hits.next(), hits.next()) {
            (Some(index), None) => TitleMatch::One(index),
            (Some(_), Some(_)) => TitleMatch::Ambiguous,
            _ => TitleMatch::None,
        }
    };
    match pick(&|title| title == wanted) {
        TitleMatch::None => {
            let wanted = wanted.to_lowercase();
            pick(&|title| title.to_lowercase() == wanted)
        }
        found => found,
    }
}

/// Most items a missed segment lists, so a long menu cannot flood the reply.
const MAX_LISTED_TITLES: usize = 40;

/// A missed segment names what was asked for, where, and the items that are
/// there, so the next call can use the real title instead of a full tree
/// read (LibreOffice calls it "To Next Sheet", not "Next Sheet").
fn missing_segment_error(depth: usize, prefix: &[String], titles: &[String]) -> String {
    let wanted = &prefix[depth];
    let location = if depth == 0 {
        "in the menu bar".to_owned()
    } else {
        format!("under \"{}\"", prefix[..depth].join(" > "))
    };
    let mut items: Vec<&str> = titles
        .iter()
        .map(|title| title.trim())
        .filter(|title| !title.is_empty())
        .collect();
    items.dedup();
    if items.is_empty() {
        return format!(
            "invoke_menu: path segment {depth} \"{wanted}\" was not found {location}, which lists no titled items (the app may fill this menu only while it is open)"
        );
    }
    let more = items.len().saturating_sub(MAX_LISTED_TITLES);
    let listed = items
        .iter()
        .take(MAX_LISTED_TITLES)
        .map(|title| format!("\"{title}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let more = if more > 0 {
        format!(" and {more} more")
    } else {
        String::new()
    };
    format!(
        "invoke_menu: path segment {depth} \"{wanted}\" was not found {location}. Items there: {listed}{more}"
    )
}

unsafe fn resolve_exact_prefix(
    menu_bar: AXUIElementRef,
    prefix: &[String],
) -> Result<AXUIElementRef, String> {
    let mut current = menu_bar;
    let mut owns_current = false;

    for (depth, segment) in prefix.iter().enumerate() {
        let children = semantic_children(current);
        if owns_current {
            CFRelease(current as CFTypeRef);
        }

        let titles: Vec<String> = children
            .iter()
            .map(|&child| copy_string_attr(child, "AXTitle").unwrap_or_default())
            .collect();
        let picked = pick_title(&titles, segment);
        let mut chosen = None;
        for (index, child) in children.into_iter().enumerate() {
            if matches!(picked, TitleMatch::One(i) if i == index) {
                chosen = Some(child);
            } else {
                CFRelease(child as CFTypeRef);
            }
        }

        match (picked, chosen) {
            (TitleMatch::One(_), Some(child)) => current = child,
            (TitleMatch::Ambiguous, _) => {
                return Err(format!("invoke_menu: path segment {depth} is ambiguous"));
            }
            _ => return Err(missing_segment_error(depth, prefix, &titles)),
        }
        owns_current = true;
    }

    if owns_current {
        Ok(current)
    } else {
        Err("invoke_menu: path is empty".into())
    }
}

/// Close the menu a failed path opened from the menu bar item `top` with
/// AXCancel on that item's menu (which also ends its submenus), and read the
/// result back from WindowServer's window list. Menu windows in
/// `menus_before` were on screen before this call and do not count.
unsafe fn close_opened_menu(
    app: AXUIElementRef,
    pid: i32,
    top: &str,
    menus_before: &[u32],
) -> Option<bool> {
    // An action that reported an error can still open its menu a moment
    // later; give it the same settle time as a hop before looking.
    std::thread::sleep(Duration::from_millis(80));
    if crate::windows::new_menu_windows(pid, menus_before)? == 0 {
        return Some(true);
    }
    let item = copy_element_attr(app, "AXMenuBar").and_then(|bar| {
        set_messaging_timeout(bar);
        let item = resolve_exact_prefix(bar, std::slice::from_ref(&top.to_owned())).ok();
        CFRelease(bar as CFTypeRef);
        item
    });
    let Some(item) = item else {
        return Some(false);
    };
    set_messaging_timeout(item);
    for child in copy_children(item) {
        set_messaging_timeout(child);
        if copy_string_attr(child, "AXRole").as_deref() == Some("AXMenu") {
            let _ = perform_action(child, "AXCancel");
        }
        CFRelease(child as CFTypeRef);
    }
    CFRelease(item as CFTypeRef);
    crate::windows::wait_for_new_menus_closed(pid, menus_before)
}

/// The refusal for a path that failed after pressing a menu item, saying
/// whether the menu it opened was seen to close.
fn failure_after_press(error: String, top: &str, closed: Option<bool>) -> String {
    match closed {
        Some(true) => format!("{error}. No menu window this call opened is still on screen."),
        Some(false) | None => format!(
            "{error}. The {top} menu this call opened may still be open: press escape on the window before other input."
        ),
    }
}

fn choose_action(actions: &[String], final_segment: bool) -> Option<&'static str> {
    let supports = |name: &str| actions.iter().any(|action| action == name);
    let order: &[&str] = if final_segment {
        &["AXPress", "AXPick", "AXConfirm"]
    } else {
        &["AXPress", "AXPick", "AXShowMenu", "AXOpen"]
    };
    order.iter().copied().find(|action| supports(action))
}

unsafe fn invoke_path(pid: i32, path: &[String]) -> Result<(), String> {
    let app = AXUIElementCreateApplication(pid);
    if app.is_null() {
        return Err("invoke_menu: target application is unavailable".into());
    }
    set_messaging_timeout(app);

    // Whether this call pressed a menu item (a press that reports an error
    // can still open its menu), so a failure must close what it opened.
    let mut pressed = false;
    // Unreadable now counts every menu window later as this call's, which can
    // only make the closed claim more cautious.
    let menus_before = crate::windows::menu_window_ids(pid).unwrap_or_default();
    let result = (|| {
        for depth in 0..path.len() {
            // Resolve from the live app root for every hop. Opening a menu can
            // replace its AX objects and reorder unrelated snapshot indices.
            let menu_bar = copy_element_attr(app, "AXMenuBar")
                .ok_or_else(|| "invoke_menu: target exposes no AXMenuBar".to_owned())?;
            set_messaging_timeout(menu_bar);
            let target = resolve_exact_prefix(menu_bar, &path[..=depth]);
            CFRelease(menu_bar as CFTypeRef);
            let target = target?;
            set_messaging_timeout(target);

            if copy_bool_attr(target, "AXEnabled") == Some(false) {
                CFRelease(target as CFTypeRef);
                return Err(format!("invoke_menu: path segment {depth} is disabled"));
            }
            let actions = copy_action_names(target);
            let action = choose_action(&actions, depth + 1 == path.len()).ok_or_else(|| {
                format!("invoke_menu: path segment {depth} has no usable native menu action")
            });
            let action = match action {
                Ok(action) => action,
                Err(error) => {
                    CFRelease(target as CFTypeRef);
                    return Err(error);
                }
            };
            pressed = true;
            let error = perform_action(target, action);
            CFRelease(target as CFTypeRef);
            if error != kAXErrorSuccess {
                return Err(format!(
                    "invoke_menu: native action for path segment {depth} failed with AX error {error}"
                ));
            }
            if depth + 1 != path.len() {
                std::thread::sleep(Duration::from_millis(80));
            }
        }
        Ok(())
    })();

    // A failure after a hop opened a menu: close it, so the app is not left
    // tracking a menu (where the next menu command reports success and does
    // nothing).
    let result = result.map_err(|error| {
        if !pressed {
            return error;
        }
        let closed = close_opened_menu(app, pid, &path[0], &menus_before);
        failure_after_press(error, &path[0], closed)
    });

    CFRelease(app as CFTypeRef);
    result
}

fn select_frontmost_pid(
    front_process_matches: Option<bool>,
    workspace_fallback: impl FnOnce() -> Option<i32>,
    pid: i32,
) -> Option<i32> {
    match front_process_matches {
        Some(true) => Some(pid),
        Some(false) => None,
        None => workspace_fallback(),
    }
}

fn live_frontmost_pid(pid: i32, window_id: u32) -> Option<i32> {
    select_frontmost_pid(
        crate::input::skylight::front_process_matches(pid, window_id),
        crate::apps::frontmost_pid,
        pid,
    )
}

fn live_frontmost_app() -> Option<i32> {
    crate::windows::visible_windows()
        .into_iter()
        .find(|window| {
            crate::input::skylight::front_process_matches(window.pid, window.window_id)
                == Some(true)
        })
        .map(|window| window.pid)
}

/// Make one exact application window key before resolving focus-sensitive
/// native menu state.
///
/// `SLPSSetFrontProcessWithOptions(..., kCPSNoWindows)` makes the application
/// active without broadly raising its windows. That is the right default for
/// input delivery, but it can leave the requested window non-key. macOS then
/// exposes contextual Window-menu commands (including Move & Resize) as
/// disabled even though the application itself is frontmost. Raise and mark
/// only the requested AX window, then require an exact focused-window readback
/// before menu resolution proceeds.
fn focus_ax_window(pid: i32, window_id: u32) -> Result<(), String> {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return Err("invoke_menu: target application is unavailable".into());
        }
        set_messaging_timeout(app);

        let mut target = None;
        for window in copy_ax_windows(app) {
            if target.is_none() && ax_get_window_id(window) == Some(window_id) {
                target = Some(window);
            } else {
                CFRelease(window as CFTypeRef);
            }
        }
        CFRelease(app as CFTypeRef);

        let Some(target) = target else {
            return Err("invoke_menu: target accessibility window is unavailable".into());
        };
        set_messaging_timeout(target);

        // SkyLight has requested native key status for this exact window. AX
        // raise/main/focus completes the corresponding visible and semantic
        // state; these writes remain best-effort for applications that expose
        // only a subset of the attributes.
        let _ = perform_action(target, "AXRaise");
        let _ = set_bool_attr_true(target, "AXMain");
        let _ = set_bool_attr_true(target, "AXFocused");
        CFRelease(target as CFTypeRef);
    }
    Ok(())
}

struct AxWindowFocusRequest {
    pid: i32,
    window_id: u32,
    tx: SyncSender<Result<(), String>>,
    cancelled: Arc<AtomicBool>,
}

#[link(name = "System", kind = "framework")]
extern "C" {
    static _dispatch_main_q: u8;
    fn dispatch_async_f(
        queue: *const c_void,
        context: *mut c_void,
        work: unsafe extern "C" fn(*mut c_void),
    );
}

unsafe extern "C" fn focus_ax_window_on_main(context: *mut c_void) {
    let request = unsafe { Box::from_raw(context.cast::<AxWindowFocusRequest>()) };
    if request.cancelled.load(Ordering::Acquire) {
        return;
    }
    let result = focus_ax_window(request.pid, request.window_id);
    let _ = request.tx.send(result);
}

fn focus_ax_window_with_thread_affinity(pid: i32, window_id: u32) -> Result<(), String> {
    let is_main_thread = objc2_foundation::MainThreadMarker::new().is_some();
    if pid != std::process::id() as i32 || is_main_thread {
        return focus_ax_window(pid, window_id);
    }

    // AX actions against another process execute in that process. An embedded
    // driver targeting its own window is different: AppKit services AXRaise
    // in this process, and window ordering is main-thread-only. Queue just the
    // self-process AX mutation onto AppKit's main queue, then return to the
    // blocking worker for readiness polling and menu traversal.
    let (tx, rx) = mpsc::sync_channel(1);
    let cancelled = Arc::new(AtomicBool::new(false));
    let request = Box::new(AxWindowFocusRequest {
        pid,
        window_id,
        tx,
        cancelled: Arc::clone(&cancelled),
    });
    unsafe {
        let main_queue = &raw const _dispatch_main_q as *const c_void;
        dispatch_async_f(
            main_queue,
            Box::into_raw(request).cast::<c_void>(),
            focus_ax_window_on_main,
        );
    }
    match rx.recv_timeout(MAIN_QUEUE_TIMEOUT) {
        Ok(result) => result,
        Err(error) => {
            // If AppKit never serviced the request, prevent a stale focus
            // change from firing after this tool call has already failed.
            cancelled.store(true, Ordering::Release);
            Err(format!(
                "invoke_menu: failed waiting for the embedded host window on the AppKit main queue: {error}"
            ))
        }
    }
}

fn focus_exact_window(pid: i32, window_id: u32) -> Result<(), String> {
    let native_key_requested = crate::input::skylight::make_exact_window_key(pid, window_id);
    if !native_key_requested
        && live_frontmost_pid(pid, window_id) != Some(pid)
        && !crate::apps::activate_pid(pid)
    {
        return Err("invoke_menu: target application could not be activated".into());
    }
    if !native_key_requested {
        std::thread::sleep(Duration::from_millis(120));
    }

    focus_ax_window_with_thread_affinity(pid, window_id)?;

    // AXFocusedWindow can lead AppKit's native `isKeyWindow` state while the
    // WindowServer activation is still settling. Menu validation observes the
    // latter. Require the exact app/window pair to remain stable briefly so a
    // loaded desktop cannot expose a transiently disabled contextual item.
    let deadline = std::time::Instant::now() + Duration::from_millis(800);
    let mut stable_since = None;
    loop {
        let now = std::time::Instant::now();
        if exact_window_is_ready(
            live_frontmost_pid(pid, window_id),
            pid,
            crate::ax::bindings::focused_window_id_of_pid(pid),
            window_id,
        ) {
            let since = stable_since.get_or_insert(now);
            if now.duration_since(*since) >= Duration::from_millis(120) {
                return Ok(());
            }
        } else {
            stable_since = None;
        }
        if now >= deadline {
            return Err(format!(
                "invoke_menu: target window {window_id} did not become stably key and frontmost"
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn exact_window_is_ready(
    frontmost_pid: Option<i32>,
    target_pid: i32,
    focused_window_id: Option<u32>,
    target_window_id: u32,
) -> bool {
    frontmost_pid == Some(target_pid) && focused_window_id == Some(target_window_id)
}

/// The item could not be invoked with its app in the background. Activating
/// the app would take the user's front window, so refuse and name the route.
fn background_unavailable(reason: String) -> ToolResult {
    let message = format!(
        "{reason}. Nothing was sent: macOS disables this menu item while the app is behind the \
         user's window, and invoke_menu does not activate a background app. Use the window's \
         own control instead when it has one, by its element_token from get_window_state (the \
         window's close button for Close Window, the toolbar's view buttons for a view change). \
         A background hotkey does not help: the disabled item ignores its shortcut too. If the \
         user allows taking the screen, call bring_to_front for this window first, then \
         invoke_menu, or send the app's keyboard shortcut with hotkey and \
         delivery_mode:\"foreground\"."
    );
    ToolResult::error(message.clone()).with_structured(serde_json::json!({
        "status": "refused",
        "code": "background_unavailable",
        "effect": "refused",
        "dispatched": false,
        "refusal": { "code": "background_unavailable", "message": message }
    }))
}

/// Why a background menu item could not be pressed.
#[derive(Debug)]
enum BackgroundPressError {
    /// macOS keeps the item disabled while the app is in the background.
    Disabled {
        message: String,
        is_save: bool,
    },
    /// Background menu validation was attempted, but the item still could
    /// not be confirmed enabled. This never enters the legacy Save fallback.
    ValidationFailed {
        message: String,
        dispatch: Dispatch,
    },
    Other(String),
    /// The final item's action was sent and AX reported an error. An AX
    /// error or timeout does not prove the app did not run the command, so
    /// this is never reported as nothing sent.
    SentWithError(String),
    UnboundWindow {
        requested: u32,
        focused: Option<u32>,
        main: Option<u32>,
    },
}

/// What the background route knows about the pressed item.
#[derive(Debug, Clone, Copy)]
struct PressedItem {
    is_save: bool,
}

/// Whether a menu item's key equivalent is plain Command-S, the Save
/// shortcut in every language.
unsafe fn item_is_save(item: AXUIElementRef) -> bool {
    copy_string_attr(item, "AXMenuItemCmdChar").is_some_and(|ch| ch.eq_ignore_ascii_case("s"))
        && crate::ax::bindings::copy_number_attr(item, "AXMenuItemCmdModifiers").unwrap_or(0.0)
            == 0.0
}

/// How long a disabled item gets to enable after its window becomes key, and
/// how often it is re-read.
const ENABLE_WAIT: Duration = Duration::from_millis(600);
const ENABLE_POLL: Duration = Duration::from_millis(40);

/// The item-404 AppKit validation trigger gets a separate short budget. The
/// constants above also govern the slower initial window-key wait.
const MENU_VALIDATION_WAIT: Duration = Duration::from_millis(40);
const MENU_VALIDATION_POLL: Duration = Duration::from_millis(10);

/// Validate an initially disabled item once, then require an actual enabled
/// readback before dispatch. `None` is not enabled. The budget counts sleep
/// durations only; a blocking AX read cannot turn it into a wall-clock SLA.
fn validate_menu_enablement(
    initial: Option<bool>,
    prime: impl FnOnce() -> bool,
    mut enabled: impl FnMut() -> Option<bool>,
    mut sleep: impl FnMut(Duration),
) -> Result<(), Dispatch> {
    if initial != Some(false) {
        return Ok(());
    }
    if !prime() {
        // The public bool cannot identify a partial priming failure, so the
        // result must leave validation delivery unknown rather than claim
        // that no events were sent.
        return Err(Dispatch::Possible);
    }
    let mut waited = Duration::ZERO;
    let mut current = enabled();
    while current != Some(true) && waited < MENU_VALIDATION_WAIT {
        sleep(MENU_VALIDATION_POLL);
        waited += MENU_VALIDATION_POLL;
        current = enabled();
    }
    if current == Some(true) {
        Ok(())
    } else {
        Err(Dispatch::Sent)
    }
}

/// Map a fresh disabled read after validation without losing whether the new
/// priming path ran. Keeping this decision in one helper lets tests drive the
/// false → true → false race and prove an initially disabled Save cannot
/// enter the legacy Apple Event fallback.
fn disabled_menu_error(
    initial: Option<bool>,
    current: Option<bool>,
    message: String,
    is_save: bool,
) -> Option<BackgroundPressError> {
    if current != Some(false) {
        return None;
    }
    if initial == Some(false) {
        Some(BackgroundPressError::ValidationFailed {
            message,
            dispatch: Dispatch::Sent,
        })
    } else {
        Some(BackgroundPressError::Disabled { message, is_save })
    }
}

/// Classify the final item's AX action. Every error after the action was
/// sent is SentWithError (the command may still run), never Other, which
/// callers report as a nothing-sent refusal.
fn dispatch_final_item(
    perform: impl FnOnce() -> AXError,
    is_save: bool,
) -> Result<PressedItem, BackgroundPressError> {
    let error = perform();
    if error == kAXErrorSuccess {
        Ok(PressedItem { is_save })
    } else {
        Err(BackgroundPressError::SentWithError(format!(
            "the native action for the final menu item returned AX error {error}"
        )))
    }
}

/// AppKit resolves document menus against its key/main window, not the ID
/// supplied by a caller. Unknown or sibling context must fail before lookup.
fn require_menu_window(
    requested: u32,
    focused: Option<u32>,
    main: Option<u32>,
) -> Result<(), BackgroundPressError> {
    if focused == Some(requested) && main == Some(requested) {
        Ok(())
    } else {
        Err(BackgroundPressError::UnboundWindow {
            requested,
            focused,
            main,
        })
    }
}

unsafe fn require_live_menu_window(
    app: AXUIElementRef,
    requested: u32,
) -> Result<(), BackgroundPressError> {
    let read = |name| {
        let window = copy_element_attr(app, name)?;
        let id = ax_get_window_id(window);
        CFRelease(window as CFTypeRef);
        id
    };
    require_menu_window(requested, read("AXFocusedWindow"), read("AXMainWindow"))
}

/// Make the requested window the app's key window from the background (the
/// focus record only, as a background press does: the app is not activated
/// or raised and the user's window keeps key), then require the exact
/// focused and main window read-back before any menu is resolved.
unsafe fn make_menu_window_key(
    app: AXUIElementRef,
    pid: i32,
    window_id: u32,
) -> Result<(), BackgroundPressError> {
    if crate::input::skylight::activate_without_raise_with(pid, window_id, false) {
        crate::input::skylight::note_primed_key(pid, window_id);
        let mut waited = Duration::ZERO;
        while waited < ENABLE_WAIT {
            std::thread::sleep(ENABLE_POLL);
            waited += ENABLE_POLL;
            if require_live_menu_window(app, window_id).is_ok() {
                return Ok(());
            }
        }
    }
    require_live_menu_window(app, window_id)
}

fn unbound_menu_window(
    pid: i32,
    requested: u32,
    focused: Option<u32>,
    main: Option<u32>,
) -> ToolResult {
    let mut result = super::background_refusal_result(
        pid,
        requested,
        &cua_driver_core::background_input::BackgroundRefusal {
            code: super::WINDOW_NOT_KEY,
            reason: format!(
                "invoke_menu: requested window {requested} is not the confirmed document menu context \
                 (focused={focused:?}, main={main:?}); no menu action was dispatched"
            ),
            advice: Some("foreground"),
        },
    );
    if let Some(structured) = result.structured_content.as_mut() {
        structured["hint"] = serde_json::Value::String(UNBOUND_MENU_HINT.to_owned());
    }
    result
}

const UNBOUND_MENU_HINT: &str = "The app runs its menu commands against its key window, and the \
    requested window could not be made the app's key window from the background, so the command \
    could have gone to another of its windows. Act on the window's own control instead, by its \
    element_token from get_window_state (its close button, a toolbar button). If the user allows \
    taking the screen, call bring_to_front for this window first, then invoke_menu.";

/// Resolve the whole path from the app's live menu bar without opening any
/// level, then perform the final item's action. Used for a background app:
/// opening a menu-bar menu would take key focus and draw over the user's
/// front app.
unsafe fn invoke_path_in_background(
    pid: i32,
    window_id: u32,
    path: &[String],
) -> Result<PressedItem, BackgroundPressError> {
    let app = AXUIElementCreateApplication(pid);
    if app.is_null() {
        return Err(BackgroundPressError::Other(
            "invoke_menu: target application is unavailable".into(),
        ));
    }
    set_messaging_timeout(app);
    if let Err(error) = require_live_menu_window(app, window_id)
        .or_else(|_| make_menu_window_key(app, pid, window_id))
    {
        CFRelease(app as CFTypeRef);
        return Err(error);
    }
    let menu_bar = copy_element_attr(app, "AXMenuBar");
    CFRelease(app as CFTypeRef);
    let menu_bar = menu_bar.ok_or_else(|| {
        BackgroundPressError::Other("invoke_menu: target exposes no AXMenuBar".to_owned())
    })?;
    set_messaging_timeout(menu_bar);
    let target = resolve_exact_prefix(menu_bar, path);
    CFRelease(menu_bar as CFTypeRef);
    let target = target.map_err(BackgroundPressError::Other)?;
    set_messaging_timeout(target);
    let is_save = item_is_save(target);
    let result = (|| {
        // Preserve the initial state through validation so a later race back
        // to disabled cannot enter the legacy Save fallback or produce a
        // nothing-sent receipt after Cmd+F20 was posted.
        let initial_enablement = copy_bool_attr(target, "AXEnabled");
        if let Err(dispatch) = validate_menu_enablement(
            initial_enablement,
            || crate::input::skylight::prime_background_menu_validation(pid, window_id),
            || copy_bool_attr(target, "AXEnabled"),
            std::thread::sleep,
        ) {
            return Err(BackgroundPressError::ValidationFailed {
                message: format!(
                    "invoke_menu: path segment {} could not be enabled after background menu validation",
                    path.len().saturating_sub(1)
                ),
                dispatch,
            });
        }
        if let Some(error) = disabled_menu_error(
            initial_enablement,
            copy_bool_attr(target, "AXEnabled"),
            format!(
                "invoke_menu: path segment {} is disabled while the app is in the background",
                path.len().saturating_sub(1)
            ),
            is_save,
        ) {
            return Err(error);
        }
        let actions = copy_action_names(target);
        let action = choose_action(&actions, true).ok_or_else(|| {
            BackgroundPressError::Other(
                "invoke_menu: the final menu item has no usable native menu action".to_owned(),
            )
        })?;
        // Revalidate after path resolution, immediately before the only
        // dispatch. Never fix a mismatch by silently switching documents.
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return Err(BackgroundPressError::Other(
                "invoke_menu: target application is unavailable before dispatch".into(),
            ));
        }
        set_messaging_timeout(app);
        let bound = require_live_menu_window(app, window_id);
        CFRelease(app as CFTypeRef);
        bound?;
        dispatch_final_item(|| perform_action(target, action), is_save)
    })();
    CFRelease(target as CFTypeRef);
    result
}

/// The file behind a document window (its `AXDocument` URL), if any.
fn document_path_of_window(pid: i32, window_id: u32) -> Option<std::path::PathBuf> {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return None;
        }
        set_messaging_timeout(app);
        let mut url = None;
        for window in copy_ax_windows(app) {
            if url.is_none() && ax_get_window_id(window) == Some(window_id) {
                url = copy_string_attr(window, "AXDocument");
            }
            CFRelease(window as CFTypeRef);
        }
        CFRelease(app as CFTypeRef);
        url.as_deref()
            .and_then(crate::apple_events::file_url_to_path)
    }
}

/// How a background menu command was asked for, so the result can say it.
#[derive(Debug, Clone)]
pub(super) enum MenuRequest {
    /// `invoke_menu` with a path.
    Path,
    /// `hotkey` with this combo, which is the item's key equivalent.
    KeyEquivalent(String),
}

impl MenuRequest {
    fn preface(&self, path: &[String]) -> String {
        match self {
            MenuRequest::Path => String::new(),
            MenuRequest::KeyEquivalent(combo) => format!(
                "{combo} is the key equivalent of {}. The requested chord is not sent on the \
                 native menu route. ",
                path.join(" > ")
            ),
        }
    }
}

/// Run a menu command in a background app without activating it. Presses
/// the item through accessibility when macOS keeps it enabled; a disabled
/// Save of a document window goes through the app's scripting interface
/// instead (TextEdit disables Save while it is behind the user's window).
/// A Save is verified on disk. Anything else that is disabled is refused
/// with `background_unavailable`.
pub(super) async fn run_in_background(
    pid: i32,
    window_id: u32,
    path: Vec<String>,
    request: MenuRequest,
) -> ToolResult {
    let mutation = super::acquire_background_mutation(pid).await;
    run_in_background_with_lease(&mutation, window_id, path, request).await
}

/// run_in_background for a caller that already holds the app's per-PID
/// background mutation lease (it is not reentrant).
pub(super) async fn run_in_background_with_lease(
    lease: &super::BackgroundMutationLease,
    window_id: u32,
    path: Vec<String>,
    request: MenuRequest,
) -> ToolResult {
    let pid = lease.pid();
    let document = latency::spawn_action_blocking(move || document_path_of_window(pid, window_id))
        .await
        .ok()
        .flatten();
    let before = document
        .as_deref()
        .and_then(crate::apple_events::modified_time);
    let press_path = path.clone();
    let outcome = latency::spawn_blocking(move || {
        let _key_focus = crate::key_focus_guard::protect(pid);
        let _action = latency::span(latency::Stage::ActionBlocking);
        unsafe { invoke_path_in_background(pid, window_id, &press_path) }
    })
    .await;
    let preface = request.preface(&path);
    match outcome {
        Ok(Ok(PressedItem { is_save: true })) if document.is_some() => {
            let file = document.clone().expect("checked");
            let written = latency::spawn_action_blocking(move || {
                crate::apple_events::wait_for_new_mtime(&file, before, Duration::from_secs(2))
            })
            .await
            .unwrap_or(false);
            let file = document.expect("checked");
            if written {
                background_success(
                    format!(
                        "{preface}Pressed {} through accessibility with the app in the \
                         background (not activated). Verified: {} was written to disk.",
                        path.join(" > "),
                        file.display()
                    ),
                    ActionEffect::Confirmed,
                    ActionTransport::MacosAxAction,
                    "The document's file modification time moved after the press",
                )
            } else {
                background_success(
                    format!(
                        "{preface}Pressed {} through accessibility with the app in the \
                         background (not activated), but {} did not change on disk within 2 s. \
                         The document may have had no unsaved changes, or the save did not run; \
                         verify from fresh state.",
                        path.join(" > "),
                        file.display()
                    ),
                    ActionEffect::Unverifiable,
                    ActionTransport::MacosAxAction,
                    "AX accepted the press; the file modification time did not move",
                )
            }
        }
        Ok(Ok(_)) => background_success(
            format!(
                "{preface}Invoked {} through accessibility with the app in the background (not \
                 activated); verify the command's semantic effect from fresh state.",
                path.join(" > ")
            ),
            ActionEffect::Unverifiable,
            ActionTransport::MacosAxAction,
            "Every menu hop resolved uniquely without opening it and AX accepted the final action",
        ),
        Ok(Err(BackgroundPressError::Disabled {
            is_save: true,
            message,
        })) if document.is_some() => {
            let file = document.expect("checked");
            let bundle_id = crate::apps::bundle_id_for_pid(pid).unwrap_or_default();
            let app_name =
                crate::apps::get_app_name_for_pid(pid).unwrap_or_else(|| "the app".into());
            let save_file = file.clone();
            let save_app = app_name.clone();
            let saved = latency::spawn_action_blocking(move || {
                crate::apple_events::save_document(&bundle_id, &save_app, &save_file)
            })
            .await;
            match saved {
                Ok(Ok(saved)) => background_success(
                    format!(
                        "{preface}{} is disabled while {app_name} is in the background, so the \
                         document was saved through {app_name}'s scripting interface instead \
                         (not activated). Verified: {app_name} reports no unsaved changes{}.",
                        path.join(" > "),
                        if saved.file_written {
                            format!(" and {} was written to disk", file.display())
                        } else {
                            format!(
                                "; {} did not change on disk because it already held the \
                                 document's content",
                                file.display()
                            )
                        }
                    ),
                    ActionEffect::Confirmed,
                    ActionTransport::MacosAppleEvent,
                    "The app's scripting interface reported the document saved",
                ),
                Ok(Err(failure)) => {
                    save_failure_result(&preface, &path, &message, &app_name, failure)
                }
                Err(error) => uncertain_dispatch(
                    &preface,
                    &path,
                    &format!("the background save task ended without a result ({error})"),
                    Dispatch::Possible,
                    ActionTransport::MacosAppleEvent,
                ),
            }
        }
        Ok(Err(BackgroundPressError::ValidationFailed { message, dispatch })) => {
            menu_validation_failure(&preface, &path, &message, dispatch)
        }
        Ok(Err(BackgroundPressError::Disabled { message, .. })) => {
            background_unavailable(format!("{preface}{message}"))
        }
        Ok(Err(BackgroundPressError::UnboundWindow {
            requested,
            focused,
            main,
        })) => unbound_menu_window(pid, requested, focused, main),
        Ok(Err(BackgroundPressError::Other(error))) => refusal(error),
        Ok(Err(BackgroundPressError::SentWithError(error))) => uncertain_dispatch(
            &preface,
            &path,
            &error,
            Dispatch::Sent,
            ActionTransport::MacosAxAction,
        ),
        // The press task ended without a result; it may have stopped before
        // or after the action was sent, so this is not a nothing-sent
        // refusal either.
        Err(error) => uncertain_dispatch(
            &preface,
            &path,
            &format!("the background press task ended without a result ({error})"),
            Dispatch::Possible,
            ActionTransport::MacosAxAction,
        ),
    }
}

/// Whether a command whose outcome is unknown reached the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dispatch {
    /// It was sent and reported an error.
    Sent,
    /// It may or may not have been sent.
    Possible,
}

/// A disabled background Save whose scripting-interface save did not
/// confirm. Only a save that never ran is a nothing-sent refusal.
fn save_failure_result(
    preface: &str,
    path: &[String],
    message: &str,
    app_name: &str,
    failure: crate::apple_events::SaveFailure,
) -> ToolResult {
    use crate::apple_events::SaveStage;
    let detail = format!("{}: {}", failure.refusal.code, failure.refusal.reason);
    let dispatch = match failure.stage {
        SaveStage::NotSent => {
            return background_unavailable(format!(
                "{preface}{message}, and saving through {app_name}'s scripting interface did not \
                 run ({detail}). Nothing was saved"
            ));
        }
        SaveStage::MaybeSent => Dispatch::Possible,
        SaveStage::Sent => Dispatch::Sent,
    };
    uncertain_dispatch(
        preface,
        path,
        &format!(
            "it is disabled in the background, and saving through {app_name}'s scripting \
             interface did not confirm ({detail})"
        ),
        dispatch,
        ActionTransport::MacosAppleEvent,
    )
}

/// A validation pair and a requested menu action are different dispatches.
/// Keep this as an error so run_steps stops, and outside
/// `background_unavailable` so its producer fields survive direct MCP and
/// the hotkey/press_key wrapper cannot claim that no keys were sent.
fn menu_validation_failure(
    preface: &str,
    path: &[String],
    error: &str,
    dispatch: Dispatch,
) -> ToolResult {
    let (lead, actual_delivery, validation_events_sent) = match dispatch {
        Dispatch::Sent => (
            "Posted Cmd+F20 menu-validation events (accepted for posting)",
            ActualDelivery::Background,
            Some(true),
        ),
        Dispatch::Possible => (
            "Menu-validation events may have been posted",
            ActualDelivery::Unknown,
            None,
        ),
    };
    let text = format!(
        "{preface}{lead} for {}, but {error}. The requested menu action was not dispatched. \
         Read fresh state before retrying.",
        path.join(" > ")
    );
    let mut record = ActionExecutionRecord::builder(
        ActionEffect::Unverifiable,
        ActionTransport::MacosCgEventPid,
        RequestedDelivery::Background,
    )
    .actual_delivery(actual_delivery);
    if dispatch == Dispatch::Sent {
        record = record.evidence(ActionEvidence {
            kind: EvidenceKind::NativeApiResult,
            detail: "Cmd+F20 keyDown/keyUp were accepted for SkyLight posting; AppKit consumption and AXEnabled enablement were not confirmed".into(),
        });
    }
    ToolResult::error(text)
        .with_action_record(record.build().expect("menu validation record is valid"))
        .with_structured(serde_json::json!({
            "status": "unverifiable",
            "effect": "unverifiable",
            "code": "menu_validation_unconfirmed",
            "error": error,
            "menu_action_dispatched": false,
            "validation_events_sent": validation_events_sent,
        }))
}

/// The command was, or may have been, sent and its outcome is not known.
/// Retrying could run it twice, so the result says to read fresh state
/// first.
fn uncertain_dispatch(
    preface: &str,
    path: &[String],
    error: &str,
    dispatch: Dispatch,
    transport: ActionTransport,
) -> ToolResult {
    let (lead, detail, dispatched) = match dispatch {
        Dispatch::Sent => (
            "Sent",
            "The command was sent and reported an error, which does not show it did not run",
            serde_json::json!(true),
        ),
        Dispatch::Possible => (
            "May have sent",
            "The command may have been sent before the failure; its outcome is unknown",
            serde_json::Value::Null,
        ),
    };
    let mut result = background_success(
        format!(
            "{preface}{lead} {} with the app in the background (not activated), but {error}. The \
             command may have run: read fresh state before retrying it.",
            path.join(" > ")
        ),
        ActionEffect::Unverifiable,
        transport,
        detail,
    );
    if dispatch == Dispatch::Possible {
        // Nothing shows the app received the command: no delivery mode and
        // no native-API evidence is claimed.
        result.action_record = Some(
            ActionExecutionRecord::builder(
                ActionEffect::Unverifiable,
                transport,
                RequestedDelivery::Background,
            )
            .actual_delivery(ActualDelivery::Unknown)
            .build()
            .expect("invoke_menu record is valid"),
        );
    }
    let mut structured = serde_json::json!({
        "status": "unverifiable",
        "effect": "unverifiable",
        "error": error,
    });
    // dispatched stays a boolean: it is left out when it is not known.
    if !dispatched.is_null() {
        structured["dispatched"] = dispatched;
    }
    result.structured_content = Some(structured);
    result
}

fn background_success(
    text: String,
    effect: ActionEffect,
    transport: ActionTransport,
    detail: &str,
) -> ToolResult {
    // A confirmed result rests on a readback of the effect (the file on disk
    // or the app's own saved state), not on the AX return code.
    let kind = if matches!(effect, ActionEffect::Confirmed) {
        EvidenceKind::ValueReadback
    } else {
        EvidenceKind::NativeApiResult
    };
    ToolResult::text(text).with_action_record(
        ActionExecutionRecord::builder(effect, transport, RequestedDelivery::Background)
            .actual_delivery(ActualDelivery::Background)
            .evidence(ActionEvidence {
                kind,
                detail: detail.into(),
            })
            .build()
            .expect("invoke_menu record is valid"),
    )
}

fn refusal(message: String) -> ToolResult {
    ToolResult::error(message.clone()).with_structured(serde_json::json!({
        "status": "refused",
        "refusal": { "code": "menu_path_unavailable", "message": message }
    }))
}

#[async_trait]
impl Tool for InvokeMenuTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        // The menu bar belongs to the app: with only a pid, use its focused
        // window.
        let mut args = args;
        super::default_window_for_pid(&mut args);
        let input: InvokeMenuInput =
            match cua_driver_core::tool_args::parse_typed_input("invoke_menu", args) {
                Ok(input) => input,
                Err(result) => return result,
            };
        let path = match normalized_path(input.path) {
            Ok(path) => path,
            Err(error) => return refusal(error),
        };
        let pid = match i32::try_from(input.pid) {
            Ok(pid) => pid,
            Err(_) => return refusal("invoke_menu: pid is out of range".into()),
        };
        let window_id = match u32::try_from(input.window_id) {
            Ok(window_id) => window_id,
            Err(_) => return refusal("invoke_menu: window_id is out of range".into()),
        };
        if !crate::windows::all_windows()
            .iter()
            .any(|window| window.pid == pid && window.window_id == window_id)
        {
            return refusal("invoke_menu: window_id does not belong to pid".into());
        }

        // A background target: invoke the item without activating the app or
        // raising its window. When macOS keeps the item unavailable to a
        // background app, refuse and say so instead of activating it.
        let target_in_front = latency::spawn_action_blocking(move || {
            live_frontmost_app().or_else(crate::apps::frontmost_pid) == Some(pid)
        })
        .await
        .unwrap_or(false);
        if !target_in_front {
            return run_in_background(pid, window_id, path, MenuRequest::Path).await;
        }

        let outcome = latency::spawn_action_blocking(move || {
            let prior_frontmost = live_frontmost_app().or_else(crate::apps::frontmost_pid);
            let prior_frontmost_window =
                prior_frontmost.and_then(crate::ax::bindings::focused_window_id_of_pid);
            let needs_activation = prior_frontmost != Some(pid);

            let result = focus_exact_window(pid, window_id)
                .and_then(|()| unsafe { invoke_path(pid, &path) });

            // Restore the exact prior key window when one was observable,
            // including across applications. Falling back to app activation
            // preserves the previous behavior for apps without an AX window.
            if let Some(prior_pid) = prior_frontmost {
                let already_restored =
                    prior_pid == pid && prior_frontmost_window == Some(window_id);
                if !already_restored {
                    let restored_exact = prior_frontmost_window.is_some_and(|prior_window_id| {
                        focus_exact_window(prior_pid, prior_window_id).is_ok()
                    });
                    if needs_activation && !restored_exact {
                        let _ = crate::apps::activate_pid(prior_pid);
                    }
                }
            }
            result
        })
        .await;

        match outcome {
            Ok(Ok(())) => ToolResult::text(
                "Resolved the live native menu path and dispatched its final accessibility action; verify the command's semantic effect from fresh state.",
            )
            .with_action_record(
                ActionExecutionRecord::builder(
                    ActionEffect::Unverifiable,
                    ActionTransport::MacosAxAction,
                    RequestedDelivery::Foreground,
                )
                .actual_delivery(ActualDelivery::Foreground)
                .evidence(ActionEvidence {
                    kind: EvidenceKind::NativeApiResult,
                    detail: "Every menu hop resolved uniquely and AX accepted the final action".into(),
                })
                .build()
                .expect("invoke_menu record is valid"),
            ),
            Ok(Err(error)) => refusal(error),
            Err(error) => refusal(format!("invoke_menu: blocking task failed: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_validation_bypasses_items_that_were_not_initially_disabled() {
        for initial in [Some(true), None] {
            let result = validate_menu_enablement(
                initial,
                || panic!("prime must not run"),
                || panic!("enablement read must not run"),
                |_| panic!("sleep must not run"),
            );
            assert_eq!(result, Ok(()));
        }
    }

    #[test]
    fn menu_validation_accepts_immediate_and_delayed_enablement() {
        let immediate = validate_menu_enablement(
            Some(false),
            || true,
            || Some(true),
            |_| panic!("an enabled item must not sleep"),
        );
        assert_eq!(immediate, Ok(()));

        let reads = std::cell::Cell::new(0usize);
        let mut sleeps = Vec::new();
        let delayed = validate_menu_enablement(
            Some(false),
            || true,
            || {
                reads.set(reads.get() + 1);
                (reads.get() > 1).then_some(true)
            },
            |duration| sleeps.push(duration),
        );
        assert_eq!(delayed, Ok(()));
        assert_eq!(sleeps, [MENU_VALIDATION_POLL]);
    }

    #[test]
    fn menu_validation_bounds_disabled_and_unreadable_polls() {
        for value in [Some(false), None] {
            let mut sleeps = Vec::new();
            let result = validate_menu_enablement(
                Some(false),
                || true,
                || value,
                |duration| sleeps.push(duration),
            );
            assert_eq!(result, Err(Dispatch::Sent));
            assert_eq!(sleeps, [MENU_VALIDATION_POLL; 4]);
            assert_eq!(
                sleeps.iter().sum::<Duration>(),
                MENU_VALIDATION_WAIT,
                "four polling sleeps are the whole budget"
            );
        }
    }

    #[test]
    fn menu_validation_prime_failure_does_not_poll() {
        let result = validate_menu_enablement(
            Some(false),
            || false,
            || panic!("enablement must not be read after prime failure"),
            |_| panic!("sleep must not run after prime failure"),
        );
        assert_eq!(result, Err(Dispatch::Possible));
    }

    #[test]
    fn initially_disabled_save_race_cannot_enter_the_legacy_fallback() {
        // Drive the production sequence: initial false, validated true, then
        // a fresh race read of false. Even with is_save=true, this must be a
        // validation failure, not Disabled's Apple Event Save path.
        let validated = validate_menu_enablement(
            Some(false),
            || true,
            || Some(true),
            |_| panic!("already-enabled item must not sleep"),
        );
        assert_eq!(validated, Ok(()));
        let error = disabled_menu_error(Some(false), Some(false), "raced disabled".into(), true)
            .expect("race must reject before AX action selection");
        match error {
            BackgroundPressError::ValidationFailed { dispatch, .. } => {
                assert_eq!(dispatch, Dispatch::Sent);
            }
            BackgroundPressError::Disabled { .. } => {
                panic!("initially disabled Save entered the Apple Event fallback variant")
            }
            _ => panic!("race produced an unrelated error"),
        }

        // Without the new priming path, the same disabled read keeps the
        // legacy behavior and its Save fallback variant.
        let legacy = disabled_menu_error(Some(true), Some(false), "disabled".into(), true)
            .expect("unprimed race");
        assert!(matches!(
            legacy,
            BackgroundPressError::Disabled { is_save: true, .. }
        ));
        assert!(disabled_menu_error(Some(false), Some(true), "enabled".into(), true).is_none());
        assert!(disabled_menu_error(Some(true), None, "unreadable".into(), true).is_none());
    }

    #[test]
    fn validation_failure_is_a_truthful_error_receipt() {
        let path = vec!["Edit".into(), "Select All".into()];
        let sent = menu_validation_failure("", &path, "still disabled", Dispatch::Sent);
        assert_eq!(sent.is_error, Some(true));
        let text = text_of(&sent);
        assert!(text.contains("accepted for posting"), "{text}");
        assert!(text.contains("The requested menu action was not dispatched"));
        assert!(!text.contains("Nothing was sent"));
        assert!(!text.contains("command may have run"));
        let structured = sent.structured_content.clone().expect("structured");
        assert_eq!(structured["status"], "unverifiable");
        assert_eq!(structured["effect"], "unverifiable");
        assert_eq!(structured["code"], "menu_validation_unconfirmed");
        assert_eq!(structured["menu_action_dispatched"], false);
        assert_eq!(structured["validation_events_sent"], true);
        assert!(structured.get("dispatched").is_none());
        assert!(structured.get("escalation").is_none());
        let record = sent.action_record.expect("record");
        assert_eq!(record.actual_delivery, Some(ActualDelivery::Background));
        assert_eq!(record.transport, ActionTransport::MacosCgEventPid);

        let possible = menu_validation_failure("", &path, "prime failed", Dispatch::Possible);
        let structured = possible.structured_content.clone().expect("structured");
        assert!(structured["validation_events_sent"].is_null());
        assert_eq!(
            possible
                .action_record
                .as_ref()
                .expect("record")
                .actual_delivery,
            Some(ActualDelivery::Unknown)
        );
        let passed = super::super::hotkey::menu_refusal_without_keys(possible);
        let structured = passed.structured_content.expect("structured");
        assert_eq!(structured["code"], "menu_validation_unconfirmed");
        assert!(structured.get("keys_sent").is_none());
    }

    #[test]
    fn key_equivalent_preface_promises_only_the_requested_chord_unsent() {
        let preface = MenuRequest::KeyEquivalent("cmd+a".into())
            .preface(&["Edit".into(), "Select All".into()]);
        assert!(preface.contains("The requested chord is not sent"));
        assert!(!preface.contains("no keys were sent"));
    }

    #[test]
    fn background_menu_requires_both_exact_document_contexts() {
        assert!(require_menu_window(7, Some(7), Some(7)).is_ok());
        for (focused, main) in [
            (Some(8), Some(8)),
            (Some(7), Some(8)),
            (Some(8), Some(7)),
            (None, Some(7)),
            (Some(7), None),
            (None, None),
        ] {
            assert!(matches!(
                require_menu_window(7, focused, main),
                Err(BackgroundPressError::UnboundWindow { requested: 7, focused: f, main: m })
                    if f == focused && m == main
            ));
        }
    }

    #[test]
    fn unbound_menu_refuses_before_dispatch_with_the_exact_target() {
        let result = unbound_menu_window(42, 7, Some(8), None);
        assert_eq!(result.is_error, Some(true));
        let data = result.structured_content.unwrap();
        assert_eq!(data["code"], "window_not_key");
        assert_eq!(data["effect"], "refused");
        assert_eq!(data["pid"], 42);
        assert_eq!(data["window_id"], 7);
        assert!(data["reason"]
            .as_str()
            .unwrap()
            .contains("no menu action was dispatched"));
        // The cause and the routes that work without the menu.
        let hint = data["hint"].as_str().unwrap();
        assert!(
            hint.contains("could not be made the app's key window"),
            "{hint}"
        );
        assert!(hint.contains("element_token"), "{hint}");
        assert!(hint.contains("bring_to_front"), "{hint}");
    }

    #[test]
    fn a_background_menu_refusal_names_the_code_and_the_route() {
        let result = background_unavailable("invoke_menu: path segment 1 is disabled".into());
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.expect("structured refusal");
        assert_eq!(structured["code"], "background_unavailable");
        // Nothing was sent: the effect says so, not only the code.
        assert_eq!(structured["effect"], "refused");
        assert_eq!(structured["dispatched"], false);
        let message = structured["refusal"]["message"].as_str().unwrap();
        assert!(message.contains("macOS disables"), "{message}");
        // The window's own control (its close button for Close Window).
        assert!(message.contains("close button"), "{message}");
        assert!(message.contains("does not activate a background app"));
        assert!(message.contains("bring_to_front"));
        assert!(message.contains("delivery_mode:\"foreground\""));
        assert!(message.contains("A background hotkey does not help"));
    }

    #[test]
    fn path_normalization_rejects_empty_segments() {
        assert_eq!(
            normalized_path(vec![" Window ".into(), " Left ".into()]).unwrap(),
            vec!["Window", "Left"]
        );
        assert!(normalized_path(vec!["Window".into(), "  ".into()]).is_err());
    }

    fn text_of(result: &ToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| match c {
                cua_driver_core::protocol::Content::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_save_is_refused_only_when_it_never_ran() {
        use crate::apple_events::{SaveFailure, SaveStage, ScriptRefusal};
        let path = vec!["File".to_owned(), "Save".to_owned()];
        let failure = |code, stage| SaveFailure {
            refusal: ScriptRefusal {
                code,
                reason: "reason".into(),
            },
            stage,
        };
        // Automation denied before any event: nothing sent.
        let refused = save_failure_result(
            "",
            &path,
            "Save is disabled",
            "TextEdit",
            failure("automation_denied", SaveStage::NotSent),
        );
        let structured = refused.structured_content.expect("structured");
        assert_eq!(structured["effect"], "refused");
        assert_eq!(structured["dispatched"], false);
        // A script-level denial can be the modified-flag readback after the
        // save was accepted: the save may have run.
        let maybe = save_failure_result(
            "",
            &path,
            "Save is disabled",
            "TextEdit",
            failure("automation_denied", SaveStage::MaybeSent),
        );
        let text = text_of(&maybe);
        let structured = maybe.structured_content.expect("structured");
        assert_eq!(structured["effect"], "unverifiable");
        assert!(structured.get("dispatched").is_none(), "{structured}");
        assert!(text.contains("May have sent File > Save"), "{text}");
        assert!(!text.contains("Nothing was saved"), "{text}");
        // Accepted but still dirty: sent, outcome unknown.
        let sent = save_failure_result(
            "",
            &path,
            "Save is disabled",
            "TextEdit",
            failure("save_unconfirmed", SaveStage::Sent),
        );
        let structured = sent.structured_content.expect("structured");
        assert_eq!(structured["effect"], "unverifiable");
        assert_eq!(structured["dispatched"], true);
    }

    #[test]
    fn a_task_that_ended_without_a_result_may_or_may_not_have_sent() {
        let path = vec!["Edit".to_owned(), "Select All".to_owned()];
        let result = uncertain_dispatch(
            "",
            &path,
            "the background press task ended without a result (panic)",
            Dispatch::Possible,
            ActionTransport::MacosAxAction,
        );
        let text = text_of(&result);
        assert!(
            text.starts_with("May have sent Edit > Select All"),
            "{text}"
        );
        let record = result.action_record.clone();
        let structured = result.structured_content.expect("structured");
        assert_eq!(structured["effect"], "unverifiable");
        assert!(structured.get("dispatched").is_none(), "{structured}");
        // The public result claims neither a delivery mode nor evidence.
        let public = serde_json::to_value(
            record
                .as_ref()
                .expect("record")
                .public_result()
                .expect("valid record"),
        )
        .unwrap();
        assert_eq!(public["effect"], "unverifiable", "{public}");
        assert_eq!(public["delivery"]["mode"], "unknown", "{public}");
        assert!(
            public.get("evidence").is_none_or(|e| e.is_null()),
            "{public}"
        );
    }

    #[test]
    fn an_ax_error_after_the_final_action_is_sent_is_not_a_refusal() {
        // A fake dispatch: the action reports an error but may have run.
        let mut sent = 0;
        let outcome = dispatch_final_item(
            || {
                sent += 1;
                -25204 // kAXErrorCannotComplete (a messaging timeout)
            },
            false,
        );
        assert_eq!(sent, 1);
        let Err(BackgroundPressError::SentWithError(error)) = outcome else {
            panic!("expected SentWithError, got {outcome:?}");
        };
        assert!(error.contains("-25204"), "{error}");
        assert!(matches!(
            dispatch_final_item(|| kAXErrorSuccess, true),
            Ok(PressedItem { is_save: true })
        ));

        let path = vec!["Edit".to_owned(), "Select All".to_owned()];
        let result = uncertain_dispatch(
            &MenuRequest::KeyEquivalent("cmd+a".into()).preface(&path),
            &path,
            &error,
            Dispatch::Sent,
            ActionTransport::MacosAxAction,
        );
        assert_ne!(result.is_error, Some(true));
        let structured = result.structured_content.clone().expect("structured");
        assert_eq!(structured["effect"], "unverifiable");
        assert_eq!(structured["dispatched"], true);
        assert_ne!(structured["status"], "refused");
        assert_eq!(
            result.action_record.as_ref().expect("record").effect,
            ActionEffect::Unverifiable
        );
        // press_key and hotkey pass it through unchanged: it is not a
        // background_unavailable refusal.
        let passed = super::super::hotkey::menu_refusal_without_keys(result);
        let text = text_of(&passed);
        let structured = passed.structured_content.expect("structured");
        assert_eq!(structured["effect"], "unverifiable");
        assert!(structured.get("keys_sent").is_none());
        assert!(text.contains("may have run"), "{text}");
        assert!(text.contains("Edit > Select All"), "{text}");
    }

    #[test]
    fn action_selection_is_explicit_and_ordered() {
        let actions = vec!["AXShowMenu".into(), "AXPress".into()];
        assert_eq!(choose_action(&actions, false), Some("AXPress"));
        assert_eq!(choose_action(&actions, true), Some("AXPress"));
        assert_eq!(choose_action(&["AXShowMenu".into()], true), None);
    }

    #[test]
    fn a_failed_path_says_whether_its_menu_was_closed() {
        let error = || "invoke_menu: path segment 1 was not found".to_owned();
        assert_eq!(
            failure_after_press(error(), "File", Some(true)),
            "invoke_menu: path segment 1 was not found. No menu window this call opened is still on screen."
        );
        for unconfirmed in [Some(false), None] {
            assert!(failure_after_press(error(), "File", unconfirmed)
                .ends_with("The File menu this call opened may still be open: press escape on the window before other input."));
        }
    }

    #[test]
    fn segments_match_exactly_then_ignoring_case() {
        let titles: Vec<String> = ["To Next Sheet", "To Previous Sheet", "", "Go to Sheet..."]
            .map(String::from)
            .to_vec();
        assert_eq!(pick_title(&titles, "To Next Sheet"), TitleMatch::One(0));
        assert_eq!(pick_title(&titles, "to next sheet"), TitleMatch::One(0));
        assert_eq!(pick_title(&titles, "Go to Sheet…"), TitleMatch::One(3));
        assert_eq!(pick_title(&titles, "Next Sheet"), TitleMatch::None);
        let twins: Vec<String> = ["Copy", "copy"].map(String::from).to_vec();
        assert_eq!(pick_title(&twins, "Copy"), TitleMatch::One(0));
        assert_eq!(pick_title(&twins, "COPY"), TitleMatch::Ambiguous);
    }

    #[test]
    fn a_missed_segment_lists_the_items_at_its_level() {
        let path: Vec<String> = ["Sheet", "Navigate", "Next Sheet"]
            .map(String::from)
            .to_vec();
        let titles: Vec<String> = ["To Next Sheet", "", "To Previous Sheet"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            missing_segment_error(2, &path, &titles),
            "invoke_menu: path segment 2 \"Next Sheet\" was not found under \"Sheet > Navigate\". Items there: \"To Next Sheet\", \"To Previous Sheet\""
        );
        assert!(missing_segment_error(0, &path, &["File".into()]).contains("in the menu bar"));
        assert!(missing_segment_error(1, &path, &[]).contains("lists no titled items"));
        let many: Vec<String> = (0..45).map(|i| format!("Item {i}")).collect();
        assert!(missing_segment_error(1, &path, &many).ends_with("and 5 more"));
    }

    #[test]
    fn three_periods_match_the_ellipsis_in_menu_titles() {
        assert_eq!(
            menu_title_key(" Save As... "),
            menu_title_key("Save As\u{2026}")
        );
        assert_eq!(menu_title_key("Save As..."), menu_title_key("Save As..."));
        assert_ne!(menu_title_key("Save As"), menu_title_key("Save As\u{2026}"));
        assert_ne!(
            menu_title_key("save as..."),
            menu_title_key("Save As\u{2026}")
        );
    }

    #[test]
    fn menu_focus_requires_the_exact_frontmost_app_and_window() {
        assert!(exact_window_is_ready(Some(7), 7, Some(42), 42));
        assert!(!exact_window_is_ready(Some(8), 7, Some(42), 42));
        assert!(!exact_window_is_ready(Some(7), 7, Some(41), 42));
        assert!(!exact_window_is_ready(Some(7), 7, None, 42));
    }

    fn workspace_frontmost_must_not_be_read() -> Option<i32> {
        panic!("stale workspace state must not be consulted");
    }

    #[test]
    fn windowserver_mismatch_never_falls_back_to_stale_workspace_state() {
        assert_eq!(
            select_frontmost_pid(Some(true), workspace_frontmost_must_not_be_read, 7),
            Some(7)
        );
        assert_eq!(
            select_frontmost_pid(Some(false), workspace_frontmost_must_not_be_read, 7),
            None
        );
        assert_eq!(select_frontmost_pid(None, || Some(8), 7), Some(8));
        assert_eq!(select_frontmost_pid(None, || None, 7), None);
    }

    struct ValidationResultProbe {
        def: ToolDef,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        fail_validation: bool,
    }

    #[async_trait]
    impl Tool for ValidationResultProbe {
        fn def(&self) -> &ToolDef {
            &self.def
        }

        async fn invoke(&self, _args: Value) -> ToolResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_validation {
                menu_validation_failure(
                    "",
                    &["Edit".into(), "Select All".into()],
                    "still disabled",
                    Dispatch::Sent,
                )
            } else {
                ToolResult::text("dependent action ran")
            }
        }
    }

    #[tokio::test]
    async fn validation_error_survives_direct_dispatch_and_stops_batch_steps() {
        use cua_driver_core::{
            authorization::PermissionMode,
            session_authorization::{SessionAuthorizationRegistry, SessionModeCeiling},
            tool::ToolRegistry,
        };
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let calls = std::collections::HashMap::from([
            ("invoke_menu", Arc::new(AtomicUsize::new(0))),
            ("press_key", Arc::new(AtomicUsize::new(0))),
            ("type_text", Arc::new(AtomicUsize::new(0))),
        ]);
        let mut registry = ToolRegistry::new();
        for (name, fail_validation) in [
            ("invoke_menu", true),
            ("press_key", true),
            ("type_text", false),
        ] {
            registry.register(Box::new(ValidationResultProbe {
                def: ToolDef {
                    name: name.into(),
                    description: "validation result probe".into(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "additionalProperties": true
                    }),
                    read_only: false,
                    destructive: false,
                    idempotent: false,
                    open_world: false,
                },
                calls: calls[name].clone(),
                fail_validation,
            }));
        }
        registry.register_session_tools();
        let registry = Arc::new(registry);
        registry.init_self_weak();
        let ceiling = SessionModeCeiling::for_trusted_sessions(
            [PermissionMode::Unrestricted],
            true,
            Duration::from_secs(60),
            Duration::from_secs(30),
        )
        .expect("ceiling");
        let context = SessionAuthorizationRegistry::with_ceiling(ceiling)
            .compatibility_context(PermissionMode::Unrestricted, None)
            .expect("context");

        let direct = registry
            .invoke_with_context(
                "invoke_menu",
                serde_json::json!({
                    "pid": 42,
                    "window_id": 7,
                    "path": ["Edit", "Select All"]
                }),
                context.clone(),
            )
            .await;
        assert_eq!(direct.is_error, Some(true));
        let structured = direct.structured_content.expect("direct structured");
        assert_eq!(structured["code"], "menu_validation_unconfirmed");
        assert_eq!(structured["menu_action_dispatched"], false);
        assert_eq!(structured["validation_events_sent"], true);

        let batch = registry
            .invoke_with_context(
                "run_steps",
                serde_json::json!({"steps": [
                    {"tool": "press_key", "args": {"pid": 42, "key": "a", "modifiers": ["cmd"]}},
                    {"tool": "type_text", "args": {"pid": 42, "text": "must not run"}}
                ]}),
                context,
            )
            .await;
        let structured = batch.structured_content.expect("batch structured");
        assert_eq!(structured["ok"], false);
        assert_eq!(structured["executed"], 1);
        assert_eq!(structured["failed_step"], 0);
        assert_eq!(calls["invoke_menu"].load(Ordering::SeqCst), 1);
        assert_eq!(calls["press_key"].load(Ordering::SeqCst), 1);
        assert_eq!(calls["type_text"].load(Ordering::SeqCst), 0);
    }
}
