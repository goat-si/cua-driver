use crate::goat::action_latency as latency;
use async_trait::async_trait;
use core_foundation::base::{CFEqual, CFRelease, CFTypeRef};
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use objc2_app_kit::NSRunningApplication;
use objc2_foundation::NSString;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use crate::ax::bindings::*;

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();
const SYSTEM_UI_SERVER: &str = "com.apple.systemuiserver";
const POLL: Duration = Duration::from_millis(40);

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "menu_bar_extras".into(),
        description: "Inspect and operate NSStatusItem/AXExtrasMenuBar menus without a window_id. operation=system discovers SystemUIServer's pid; list reads one explicitly authorized host pid; open/click/close use that same host and an opaque open_handle. Target-app and SystemUIServer hosts are separate calls. Open uses a bounded worker, click requires live AXEnabled, and close only cancels the opener's own menu and verifies disappearance.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["operation"],
            "properties": {
                "session": { "type": "string", "description": "For multi-call work, prefer a short public session label and repeat it on every call that accepts it. Open handles are valid only within this session episode and are invalidated by end or revival." },
                "operation": { "type": "string", "enum": ["system", "list", "open", "click", "close"], "description": "system discovers SystemUIServer pid; list enumerates one host; open opens a status item; click presses an enabled menu item; close cancels only the opener-owned menu." },
                "pid": { "type": "integer", "description": "Exact host process. Required except operation=system." },
                "owner_pid": { "type": "integer", "description": "Optional independently evidenced status-item owner filter." },
                "item": { "type": "string", "description": "Exact status-item title or AXIdentifier." },
                "menu_item": { "type": "string", "description": "Exact enabled menu-item title for click." },
                "open_handle": { "type": "string", "description": "Opaque handle returned by open, required for click/close." },
                "timeout_ms": { "type": "integer", "minimum": 100, "maximum": 10000, "description": "Observation and readback timeout in milliseconds; default 2500, allowed range 100-10000." }
            },
            "additionalProperties": false
        }),
        read_only: false,
        destructive: true,
        idempotent: false,
        open_world: true,
    })
}

pub struct MenuBarExtrasTool;

#[derive(Debug)]
struct Retained(usize);
impl Drop for Retained {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as CFTypeRef) }
    }
}
impl Clone for Retained {
    fn clone(&self) -> Self {
        unsafe { core_foundation::base::CFRetain(self.0 as CFTypeRef) };
        Self(self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcessIdentity {
    pid: i32,
    start_time: (u64, u64),
}

fn identity(pid: i32) -> Result<ProcessIdentity, String> {
    crate::ax::enablement::process_start_stamp(pid)
        .map(|start_time| ProcessIdentity { pid, start_time })
        .ok_or_else(|| format!("process {pid} identity is unavailable"))
}

struct OwnedMenu {
    session: String,
    generation: u64,
    host: ProcessIdentity,
    owner: ProcessIdentity,
    item: Retained,
    menu: Retained,
}

#[derive(Default)]
struct Registry {
    menus: HashMap<String, OwnedMenu>,
    generations: HashMap<String, u64>,
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::default()));

fn session_id(args: &Value) -> String {
    args.get("_session_id")
        .or_else(|| args.get("session"))
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_owned()
}

fn generation(registry: &Registry, session: &str) -> u64 {
    registry.generations.get(session).copied().unwrap_or(0)
}

pub(crate) fn end_session(session: &str) {
    let mut registry = REGISTRY.lock().unwrap();
    *registry.generations.entry(session.to_owned()).or_default() += 1;
    registry.menus.retain(|_, menu| menu.session != session);
}

pub(crate) fn revive_session(session: &str) {
    let mut registry = REGISTRY.lock().unwrap();
    *registry.generations.entry(session.to_owned()).or_default() += 1;
}

struct ExtraItem {
    element: Retained,
    title: String,
    identifier: Option<String>,
    host: ProcessIdentity,
    owner: Option<ProcessIdentity>,
}

fn system_ui_server_pid() -> Option<i32> {
    let bundle = NSString::from_str(SYSTEM_UI_SERVER);
    let apps = unsafe { NSRunningApplication::runningApplicationsWithBundleIdentifier(&bundle) };
    (0..apps.count()).find_map(|index| {
        let app = unsafe { apps.objectAtIndex(index) };
        let pid = unsafe { app.processIdentifier() };
        (pid > 0 && !unsafe { app.isTerminated() }).then_some(pid)
    })
}

fn owner_for_item(host: ProcessIdentity, item: AXUIElementRef) -> Option<ProcessIdentity> {
    if host.pid != system_ui_server_pid().unwrap_or(-1) {
        return Some(host);
    }
    let window_id = unsafe { ax_get_window_id(item) }?;
    let owner = crate::windows::resolve_window_owner(host.pid, window_id);
    match owner {
        crate::windows::WindowOwner::SamePid => Some(host),
        crate::windows::WindowOwner::ForeignPid { owner_pid, .. } => identity(owner_pid).ok(),
        _ => None,
    }
}

unsafe fn list_items(pid: i32) -> Result<Vec<ExtraItem>, String> {
    let host = identity(pid)?;
    let app = AXUIElementCreateApplication(pid);
    if app.is_null() {
        return Err(format!("could not create AXApplication for pid {pid}"));
    }
    let app = Retained(app as usize);
    let Some(bar) = copy_element_attr(app.0 as AXUIElementRef, "AXExtrasMenuBar") else {
        return Ok(Vec::new());
    };
    let bar = Retained(bar as usize);
    let Some(children) = copy_children_checked(bar.0 as AXUIElementRef) else {
        return Err("AXExtrasMenuBar children are unreadable".into());
    };
    let mut items = Vec::new();
    for child in children {
        let title = copy_string_attr(child, "AXTitle")
            .or_else(|| copy_string_attr(child, "AXDescription"))
            .unwrap_or_default();
        let identifier = copy_string_attr(child, "AXIdentifier");
        let owner = owner_for_item(host, child);
        items.push(ExtraItem {
            element: Retained(child as usize),
            title,
            identifier,
            host,
            owner,
        });
    }
    Ok(items)
}

fn item_matches(item: &ExtraItem, wanted: &str) -> bool {
    item.title.trim() == wanted || item.identifier.as_deref().map(str::trim) == Some(wanted)
}

unsafe fn menus_for(host: i32, item: AXUIElementRef) -> Option<Vec<Retained>> {
    let app = AXUIElementCreateApplication(host);
    if app.is_null() {
        return None;
    }
    let app = Retained(app as usize);
    let mut menus = Vec::new();
    for root in [app.0 as AXUIElementRef, item] {
        let children = copy_children_checked(root)?;
        for child in children {
            if copy_string_attr(child, "AXRole").as_deref() == Some("AXMenu") {
                if !menus
                    .iter()
                    .any(|old: &Retained| CFEqual(old.0 as CFTypeRef, child as CFTypeRef) != 0)
                {
                    menus.push(Retained(child as usize));
                } else {
                    CFRelease(child as CFTypeRef);
                }
            } else {
                CFRelease(child as CFTypeRef);
            }
        }
    }
    Some(menus)
}

fn menu_titles(menu: AXUIElementRef) -> Vec<String> {
    unsafe { copy_children_checked(menu) }
        .unwrap_or_default()
        .into_iter()
        .filter_map(|child| {
            let title = unsafe {
                (copy_string_attr(child, "AXRole").as_deref() == Some("AXMenuItem"))
                    .then(|| copy_string_attr(child, "AXTitle"))
                    .flatten()
            };
            unsafe { CFRelease(child as CFTypeRef) };
            title
        })
        .collect()
}

fn same_menu(left: &Retained, right: &Retained) -> bool {
    unsafe { CFEqual(left.0 as CFTypeRef, right.0 as CFTypeRef) != 0 }
}

enum OpenSurface {
    Menu(Retained),
    Popover(Retained),
}

unsafe fn popovers_for(host: i32, item: AXUIElementRef) -> Option<Vec<Retained>> {
    let app = AXUIElementCreateApplication(host);
    if app.is_null() {
        return None;
    }
    let app = Retained(app as usize);
    let mut popovers = Vec::new();
    for root in [app.0 as AXUIElementRef, item] {
        let children = copy_children_checked(root)?;
        for child in children {
            if copy_string_attr(child, "AXRole").as_deref() == Some("AXPopover") {
                popovers.push(Retained(child as usize));
            } else {
                CFRelease(child as CFTypeRef);
            }
        }
    }
    Some(popovers)
}

fn wait_for_new_menu(
    host: i32,
    item: AXUIElementRef,
    before_menus: Vec<Retained>,
    before_popovers: Vec<Retained>,
    timeout: Duration,
) -> Option<OpenSurface> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let menus = unsafe { menus_for(host, item) }?;
        let new: Vec<Retained> = menus
            .into_iter()
            .filter(|candidate| !before_menus.iter().any(|old| same_menu(old, candidate)))
            .collect();
        if new.len() == 1 {
            let candidate = new.into_iter().next().expect("one menu");
            let parent = unsafe { copy_element_attr(candidate.0 as AXUIElementRef, "AXParent") };
            let direct = parent.is_some_and(|parent| unsafe {
                CFEqual(parent as CFTypeRef, item as CFTypeRef) != 0
            });
            if let Some(parent) = parent {
                unsafe { CFRelease(parent as CFTypeRef) };
            }
            if direct {
                return Some(OpenSurface::Menu(candidate));
            }
            return None;
        }
        if new.is_empty() {
            let popovers = unsafe { popovers_for(host, item) }?;
            let mut new_popovers: Vec<Retained> = popovers
                .into_iter()
                .filter(|candidate| !before_popovers.iter().any(|old| same_menu(old, candidate)))
                .collect();
            if new_popovers.len() == 1 {
                let popover = new_popovers.pop().expect("one popover");
                let parent = unsafe { copy_element_attr(popover.0 as AXUIElementRef, "AXParent") };
                let direct = parent.is_some_and(|parent| unsafe {
                    CFEqual(parent as CFTypeRef, item as CFTypeRef) != 0
                });
                if let Some(parent) = parent {
                    unsafe { CFRelease(parent as CFTypeRef) };
                }
                if direct {
                    return Some(OpenSurface::Popover(popover));
                }
                return None;
            }
        }
        std::thread::sleep(POLL);
    }
    None
}

fn wait_for_gone(host: i32, item: AXUIElementRef, menu: &Retained, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let Some(menus) = (unsafe { menus_for(host, item) }) else {
            return false;
        };
        if !menus.iter().any(|candidate| same_menu(candidate, menu)) {
            return true;
        }
        std::thread::sleep(POLL);
    }
    false
}

fn output_items(items: &[ExtraItem], owner_filter: Option<i32>) -> Value {
    serde_json::json!(
        items
            .iter()
            .filter(|item| owner_filter
                .is_none_or(|pid| item.owner.map(|owner| owner.pid) == Some(pid)))
            .map(|item| serde_json::json!({
                "title": item.title,
                "identifier": item.identifier,
                "host_pid": item.host.pid,
                "owner_pid": item.owner.map(|owner| owner.pid),
                "owner_known": item.owner.is_some()
            }))
            .collect::<Vec<_>>()
    )
}

#[async_trait]
impl Tool for MenuBarExtrasTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let operation = match args.require_str("operation") {
            Ok(value) => value,
            Err(error) => return error,
        };
        if operation == "system" {
            return match system_ui_server_pid() {
                Some(pid) => ToolResult::text(format!("SystemUIServer pid={pid}"))
                    .with_structured(serde_json::json!({"system_ui_server_pid": pid})),
                None => ToolResult::error("SystemUIServer is not running"),
            };
        }
        let pid = match args.require_i32("pid") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let timeout = Duration::from_millis(args.opt_u64("timeout_ms").unwrap_or(2500).min(10_000));
        let session = session_id(&args);
        let owner_filter = args.opt_i64("owner_pid").map(|pid| pid as i32);
        let wanted = args.opt_str("item");
        let handle = args.opt_str("open_handle");
        let result = latency::spawn_action_blocking(move || -> Result<Value, String> {
            match operation.as_str() {
                "list" => {
                    let items = unsafe { list_items(pid)? };
                    Ok(serde_json::json!({"items": output_items(&items, owner_filter)}))
                }
                "open" => {
                    let wanted = wanted.ok_or_else(|| "open requires item".to_owned())?;
                    let items = unsafe { list_items(pid)? };
                    let mut matches: Vec<&ExtraItem> = items
                        .iter()
                        .filter(|item| item_matches(item, &wanted)
                            && owner_filter.is_none_or(|pid| item.owner.map(|owner| owner.pid) == Some(pid)))
                        .collect();
                    if matches.len() != 1 {
                        return Err(format!("status item {wanted:?} has {} matching entries", matches.len()));
                    }
                    let item = matches.remove(0);
                    let owner = item.owner.ok_or_else(|| "status-item owner is unproven".to_owned())?;
                    let item_element = item.element.clone();
                    let before_menus = unsafe { menus_for(pid, item_element.0 as AXUIElementRef) }
                        .ok_or_else(|| "menu baseline is incomplete".to_owned())?;
                    let before_popovers = unsafe { popovers_for(pid, item_element.0 as AXUIElementRef) }
                        .ok_or_else(|| "popover baseline is incomplete".to_owned())?;
                    let episode = {
                        let registry = REGISTRY.lock().unwrap();
                        generation(&registry, &session)
                    };
                    let press_item = item_element.clone();
                    let (tx, rx) = std::sync::mpsc::channel();
                    std::thread::spawn(move || {
                        let result = unsafe {
                            AXUIElementSetMessagingTimeout(press_item.0 as AXUIElementRef, 0.5);
                            perform_action(press_item.0 as AXUIElementRef, "AXPress")
                        };
                        let _ = tx.send(result);
                    });
                    let surface = wait_for_new_menu(
                        pid,
                        item_element.0 as AXUIElementRef,
                        before_menus,
                        before_popovers,
                        timeout,
                    );
                    let Some(surface) = surface else {
                        let ax_error = rx.try_recv().ok();
                        return Ok(serde_json::json!({
                            "effect": "unverifiable",
                            "verified": false,
                            "dispatched": true,
                            "ax_error": ax_error,
                            "reason": "no attributable new AXMenu or popover window appeared before timeout"
                        }));
                    };
                    let menu = match surface {
                        OpenSurface::Menu(menu) => menu,
                        OpenSurface::Popover(popover) => {
                        let window_id = unsafe { ax_get_window_id(popover.0 as AXUIElementRef) };
                        return Ok(serde_json::json!({
                            "effect": "confirmed",
                            "verified": true,
                            "popover_window_id": window_id,
                            "cancellable": false,
                            "reason": "a new directly attributed AXPopover was observed without an AXMenu"
                        }))
                        }
                    };
                    let handle_value = uuid::Uuid::new_v4().to_string();
                    let titles = menu_titles(menu.0 as AXUIElementRef);
                    let mut registry = REGISTRY.lock().unwrap();
                    if generation(&registry, &session) != episode {
                        return Err("session episode changed before menu registration".into());
                    }
                    registry.menus.insert(handle_value.clone(), OwnedMenu {
                        session: session.clone(),
                        generation: episode,
                        host: item.host,
                        owner,
                        item: item_element,
                        menu,
                    });
                    let _ = rx.recv_timeout(Duration::ZERO);
                    Ok(serde_json::json!({
                        "open_handle": handle_value,
                        "host_pid": pid,
                        "owner_pid": owner.pid,
                        "menu_items": titles,
                        "verified": true,
                        "effect": "confirmed"
                    }))
                }
                "click" | "close" => {
                    let handle = handle.ok_or_else(|| format!("{operation} requires open_handle"))?;
                    let generation = {
                        let registry = REGISTRY.lock().unwrap();
                        generation(&registry, &session)
                    };
                    let registry = REGISTRY.lock().unwrap();
                    let owned = registry.menus.get(&handle).ok_or_else(|| "open_handle is stale or unknown".to_owned())?;
                    if owned.session != session || owned.generation != generation {
                        return Err("open_handle belongs to another session episode".into());
                    }
                    if owned.host.pid != pid {
                        return Err(format!("open_handle host_pid {} does not match pid {pid}", owned.host.pid));
                    }
                    if identity(owned.host.pid)? != owned.host || identity(owned.owner.pid)? != owned.owner {
                        return Err("host or owner process identity changed".into());
                    }
                    let item = owned.item.clone();
                    let menu = owned.menu.clone();
                    drop(registry);
                    let live = unsafe { menus_for(pid, item.0 as AXUIElementRef) }
                        .ok_or_else(|| "live menu observation failed".to_owned())?;
                    if !live.iter().any(|candidate| same_menu(candidate, &menu)) {
                        return Err("the opener-owned menu is no longer live".into());
                    }
                    if operation == "close" {
                        let error = unsafe { perform_action(menu.0 as AXUIElementRef, "AXCancel") };
                        if error != kAXErrorSuccess {
                            return Err(format!("AXCancel failed with AXError {error}"));
                        }
                        if !wait_for_gone(pid, item.0 as AXUIElementRef, &menu, timeout) {
                            return Err("menu did not disappear after AXCancel".into());
                        }
                        REGISTRY.lock().unwrap().menus.remove(&handle);
                        return Ok(serde_json::json!({"closed": true, "verified": true, "effect": "confirmed"}));
                    }
                    let wanted = args.opt_str("menu_item").ok_or_else(|| "click requires menu_item".to_owned())?;
                    let children = unsafe { copy_children_checked(menu.0 as AXUIElementRef) }
                        .ok_or_else(|| "menu items are unreadable".to_owned())?;
                    let mut targets = Vec::new();
                    for child in children {
                        let matches = unsafe { copy_string_attr(child, "AXRole") }.as_deref()
                            == Some("AXMenuItem")
                            && unsafe { copy_string_attr(child, "AXTitle") }.as_deref()
                                == Some(wanted.as_str());
                        if matches {
                            targets.push(Retained(child as usize));
                        } else {
                            unsafe { CFRelease(child as CFTypeRef) };
                        }
                    }
                    if targets.len() != 1 {
                        return Err(format!("menu item {wanted:?} has {} exact live matches", targets.len()));
                    }
                    let target = targets.remove(0);
                    if unsafe { copy_bool_attr(target.0 as AXUIElementRef, "AXEnabled") } != Some(true) {
                        return Err("menu item is not live AXEnabled=true".into());
                    }
                    let error = unsafe { perform_action(target.0 as AXUIElementRef, "AXPress") };
                    if error != kAXErrorSuccess {
                        return Err(format!("menu item AXPress failed with AXError {error}"));
                    }
                    let closed = wait_for_gone(pid, item.0 as AXUIElementRef, &menu, timeout);
                    if closed {
                        REGISTRY.lock().unwrap().menus.remove(&handle);
                    }
                    Ok(serde_json::json!({"pressed": wanted, "menu_closed": closed, "verified": closed, "effect": if closed { "confirmed" } else { "unverifiable" }}))
                }
                _ => Err(format!("unknown menu_bar_extras operation {operation}")),
            }
        })
        .await;
        match result {
            Ok(Ok(structured)) => ToolResult::text(
                structured
                    .get("effect")
                    .and_then(Value::as_str)
                    .unwrap_or("completed")
                    .to_owned(),
            )
            .with_structured(structured),
            Ok(Err(error)) => ToolResult::error(format!("menu_bar_extras failed: {error}")),
            Err(error) => ToolResult::error(format!("menu_bar_extras task failed: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_item_matching_does_not_use_substrings() {
        let item = ExtraItem {
            element: Retained(0),
            title: "Tailscale".into(),
            identifier: Some("io.tailscale.status".into()),
            host: ProcessIdentity {
                pid: 1,
                start_time: (0, 0),
            },
            owner: None,
        };
        assert!(item_matches(&item, "Tailscale"));
        assert!(item_matches(&item, "io.tailscale.status"));
        assert!(!item_matches(&item, "Tail"));
        std::mem::forget(item);
    }

    #[test]
    fn episode_generation_rejects_end_and_revival() {
        let session = "test-400f2-extras-generation";
        end_session(session);
        let ended = {
            let registry = REGISTRY.lock().unwrap();
            generation(&registry, session)
        };
        revive_session(session);
        let revived = {
            let registry = REGISTRY.lock().unwrap();
            generation(&registry, session)
        };
        assert!(revived > ended);
    }
}
