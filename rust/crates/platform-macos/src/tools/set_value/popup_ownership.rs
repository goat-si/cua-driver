//! State machine for an AppKit popup selection. All observations and effects
//! are supplied by the caller, so tests do not need a running desktop.

use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct OptionEntry<I> {
    pub id: I,
    pub title: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ListRead<I> {
    Complete(Vec<OptionEntry<I>>),
    Incomplete,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum MenuState<I> {
    Open(I),
    ClosedOrEmpty,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PressReturn {
    Success,
    Timeout,
    Error(i32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CloseStatus {
    Confirmed,
    Unconfirmed,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum PopupOutcome {
    Refused(String),
    StoppedAfterOpen {
        reason: String,
        close: CloseStatus,
    },
    Selected {
        title: String,
        preexisting: bool,
        close: Option<CloseStatus>,
    },
    SentUnverified {
        reason: String,
        preexisting: bool,
        close: Option<CloseStatus>,
    },
}

#[derive(Debug)]
pub(super) struct PopupError(pub PopupOutcome);

impl std::fmt::Display for PopupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "popup selection outcome")
    }
}

impl std::error::Error for PopupError {}

pub(super) fn tool_result(
    outcome: &PopupOutcome,
    suffix: &str,
    ax_echo_surface: bool,
) -> cua_driver_core::protocol::ToolResult {
    use cua_driver_core::protocol::ToolResult;
    let close_note = |close: CloseStatus| match close {
        CloseStatus::Confirmed => "The owned menu was confirmed closed.",
        CloseStatus::Unconfirmed => "The menu may still be open; closure was not confirmed.",
    };
    match outcome {
        PopupOutcome::Refused(reason) => super::super::pre_dispatch_refusal(
            "popup_selection_refused",
            format!("{reason}; no input was sent.{suffix}"),
        ),
        PopupOutcome::StoppedAfterOpen { reason, close } => {
            let note = close_note(*close);
            ToolResult::error(format!("{reason}. {note}{suffix}")).with_structured(
                serde_json::json!({
                    "dispatched": true, "effect": "unverifiable", "note": note,
                }),
            )
        }
        PopupOutcome::Selected {
            title,
            preexisting,
            close,
        } => {
            let note = if *preexisting {
                "Used a previously open menu."
            } else {
                close_note(close.unwrap_or(CloseStatus::Unconfirmed))
            };
            let mut structured = serde_json::json!({"path":"ax", "verified":false, "effect":"unverifiable", "note":note});
            if ax_echo_surface {
                structured["escalation"] = serde_json::json!({
                    "recommended":"px",
                    "reason":"AXValue read-back is not trusted for web content. Verify through the renderer; use browser page tools for a tab or manipulate the control through its pixel action."
                });
            }
            ToolResult::text(format!(
                "Selected '{title}' via AX menu item AXPress. {note}{suffix}"
            ))
            .with_structured(structured)
        }
        PopupOutcome::SentUnverified {
            reason,
            preexisting,
            close,
        } => {
            let note = if *preexisting {
                "Used a previously open menu."
            } else if let Some(close) = close {
                close_note(*close)
            } else {
                "An earlier focus or type-ahead action was sent."
            };
            ToolResult::error(format!(
                "{reason}; selection may have been sent. {note}{suffix}"
            ))
            .with_structured(serde_json::json!({
                "dispatched":true, "effect":"unverifiable", "note":note,
            }))
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum OptionMatch {
    None,
    Unique(usize),
    Ambiguous(Vec<usize>),
}

pub(super) fn match_option<I>(entries: &[OptionEntry<I>], requested: &str) -> OptionMatch {
    let wanted = requested.trim().to_lowercase();
    let matches: Vec<_> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, entry)| {
            (entry.title.trim().to_lowercase() == wanted
                || entry.value.trim().to_lowercase() == wanted)
                .then_some(i)
        })
        .collect();
    match matches.len() {
        0 => OptionMatch::None,
        1 => OptionMatch::Unique(matches[0]),
        _ => OptionMatch::Ambiguous(matches),
    }
}

pub(super) trait PopupEnv {
    type Id: Clone;
    fn same(&self, a: &Self::Id, b: &Self::Id) -> bool;
    fn menu(&mut self) -> MenuState<Self::Id>;
    fn options(&mut self, menu: &Self::Id) -> ListRead<Self::Id>;
    fn idle_ms(&mut self) -> Option<u64>;
    fn now(&mut self) -> Duration;
    fn pause(&mut self);
    fn open(&mut self);
    fn opener_finished(&mut self) -> bool;
    fn join(&mut self) -> PressReturn;
    fn press(&mut self, item: &Self::Id) -> i32;
    fn cancel(&mut self, menu: &Self::Id) -> i32;
}

fn same_list<E: PopupEnv>(env: &E, a: &[OptionEntry<E::Id>], b: &[OptionEntry<E::Id>]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(a, b)| env.same(&a.id, &b.id) && a.title == b.title && a.value == b.value)
}

fn cleanup<E: PopupEnv>(env: &mut E, owned: &E::Id) -> CloseStatus {
    let eligible = match env.menu() {
        MenuState::Open(current) if env.same(owned, &current) => env.cancel(owned) == 0,
        MenuState::ClosedOrEmpty => true,
        _ => false,
    };
    let result = env.join();
    if eligible && result == PressReturn::Success && matches!(env.menu(), MenuState::ClosedOrEmpty)
    {
        CloseStatus::Confirmed
    } else {
        CloseStatus::Unconfirmed
    }
}

struct OwnedMenu<'a, E: PopupEnv> {
    env: &'a mut E,
    id: E::Id,
    finished: bool,
}

impl<E: PopupEnv> OwnedMenu<'_, E> {
    fn finish(&mut self) -> CloseStatus {
        self.finished = true;
        cleanup(self.env, &self.id)
    }
}

impl<E: PopupEnv> Drop for OwnedMenu<'_, E> {
    fn drop(&mut self) {
        if !self.finished {
            let _ = cleanup(self.env, &self.id);
        }
    }
}

pub(super) fn select<E: PopupEnv>(env: &mut E, requested: &str) -> PopupOutcome {
    let (menu, owned) = match env.menu() {
        MenuState::Unknown => return PopupOutcome::Refused("Menu state could not be read".into()),
        MenuState::Open(menu) => (menu, false),
        MenuState::ClosedOrEmpty => {
            let t0 = env.now();
            let baseline = env.idle_ms();
            env.open();
            let deadline = t0 + Duration::from_millis(2500);
            loop {
                let state = env.menu();
                let pending = !env.opener_finished();
                if let MenuState::Open(menu) = state {
                    let observed = env.idle_ms();
                    let elapsed = env.now().saturating_sub(t0).as_millis();
                    if pending
                        && !env.opener_finished()
                        && baseline.is_some()
                        && observed.is_some_and(|v| u128::from(v) > elapsed)
                    {
                        break (menu, true);
                    }
                    let _ = env.join();
                    return PopupOutcome::StoppedAfterOpen {
                        reason: "Menu ownership was not established; it may still be open".into(),
                        close: CloseStatus::Unconfirmed,
                    };
                }
                if !pending || env.now() >= deadline {
                    let _ = env.join();
                    return PopupOutcome::StoppedAfterOpen {
                        reason: "Menu ownership was not established; it may still be open".into(),
                        close: CloseStatus::Unconfirmed,
                    };
                }
                env.pause();
            }
        }
    };
    let (selection, close) = if owned {
        let mut guard = OwnedMenu {
            env,
            id: menu.clone(),
            finished: false,
        };
        let selection = select_in_menu(guard.env, &menu, requested);
        let close = guard.finish();
        (selection, Some(close))
    } else {
        (select_in_menu(env, &menu, requested), None)
    };
    match selection {
        Ok((title, 0)) => PopupOutcome::Selected {
            title,
            preexisting: !owned,
            close,
        },
        Ok((_, code)) => PopupOutcome::SentUnverified {
            reason: format!("Option press returned {code}"),
            preexisting: !owned,
            close,
        },
        Err(reason) if owned => PopupOutcome::StoppedAfterOpen {
            reason,
            close: close.unwrap_or(CloseStatus::Unconfirmed),
        },
        Err(reason) => PopupOutcome::Refused(reason),
    }
}

fn select_in_menu<E: PopupEnv>(
    env: &mut E,
    menu: &E::Id,
    requested: &str,
) -> Result<(String, i32), String> {
    let deadline = env.now() + Duration::from_millis(2500);
    let mut previous: Option<Vec<OptionEntry<E::Id>>> = None;
    let entries = loop {
        if !matches!(env.menu(), MenuState::Open(ref current) if env.same(menu, current)) {
            break None;
        }
        match env.options(menu) {
            ListRead::Complete(list) => {
                if previous
                    .as_ref()
                    .is_some_and(|old| same_list(env, old, &list))
                {
                    break Some(list);
                }
                previous = Some(list);
            }
            ListRead::Incomplete | ListRead::Failed => previous = None,
        }
        if env.now() >= deadline {
            break None;
        }
        env.pause();
    };
    match entries {
        None => Err("Menu options could not be read completely and consistently".to_owned()),
        Some(entries) => {
            match match_option(&entries, requested) {
                OptionMatch::None => Err(format!(
                    "No matching option; available titles: {}",
                    entries
                        .iter()
                        .map(|entry| entry.title.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
                OptionMatch::Ambiguous(indices) => Err(format!(
                    "Ambiguous option; matching titles: {}",
                    indices
                        .iter()
                        .map(|&i| entries[i].title.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
                OptionMatch::Unique(i) => {
                    let unchanged_menu = matches!(env.menu(), MenuState::Open(ref current) if env.same(menu, current));
                    let unchanged_list = matches!(env.options(menu), ListRead::Complete(ref fresh) if same_list(env, &entries, fresh));
                    let still_open = matches!(env.menu(), MenuState::Open(ref current) if env.same(menu, current));
                    if unchanged_menu && unchanged_list && still_open {
                        let code = env.press(&entries[i].id);
                        Ok((entries[i].title.clone(), code))
                    } else {
                        Err("Menu changed before selection".to_owned())
                    }
                }
            }
        }
    }
}

type Retained = crate::ax::snapshot::RetainedElement;

pub(super) struct NativePopupEnv {
    popup: Retained,
    opener: Option<std::thread::JoinHandle<i32>>,
    start: std::time::Instant,
}

impl NativePopupEnv {
    pub(super) unsafe fn new(popup: crate::ax::bindings::AXUIElementRef) -> Self {
        Self {
            popup: unsafe { Retained::retain(popup as usize) },
            opener: None,
            start: std::time::Instant::now(),
        }
    }
}

fn children(element: &Retained) -> Option<Vec<Retained>> {
    use crate::ax::bindings::{
        kAXErrorNoValue, kAXErrorSuccess, AXUIElementCopyAttributeValue, AXUIElementGetTypeID,
    };
    use core_foundation::{
        array::CFArray,
        base::{CFGetTypeID, CFRelease, CFTypeRef, TCFType},
        string::CFString,
    };
    let attr = CFString::new("AXChildren");
    let mut value: CFTypeRef = std::ptr::null();
    let result = unsafe {
        AXUIElementCopyAttributeValue(
            element.as_ptr() as _,
            attr.as_concrete_TypeRef(),
            &mut value,
        )
    };
    if result == kAXErrorNoValue {
        if !value.is_null() {
            unsafe { CFRelease(value) };
        }
        return Some(Vec::new());
    }
    if result != kAXErrorSuccess || value.is_null() {
        if !value.is_null() {
            unsafe { CFRelease(value) };
        }
        return None;
    }
    if unsafe { CFGetTypeID(value) } != CFArray::<CFTypeRef>::type_id() {
        unsafe { CFRelease(value) };
        return None;
    }
    let array = unsafe { CFArray::<CFTypeRef>::wrap_under_create_rule(value as _) };
    let mut result = Vec::new();
    for index in 0..array.len() {
        let item = *array.get(index)?;
        if item.is_null() || unsafe { CFGetTypeID(item) } != unsafe { AXUIElementGetTypeID() } {
            return None;
        }
        result.push(unsafe { Retained::retain(item as usize) });
    }
    Some(result)
}

impl PopupEnv for NativePopupEnv {
    type Id = Retained;

    fn same(&self, a: &Retained, b: &Retained) -> bool {
        unsafe { core_foundation::base::CFEqual(a.as_ptr() as _, b.as_ptr() as _) != 0 }
    }

    fn menu(&mut self) -> MenuState<Retained> {
        let Some(popup_children) = children(&self.popup) else {
            return MenuState::Unknown;
        };
        let mut menu = None;
        for child in popup_children {
            match unsafe { crate::ax::bindings::copy_string_attr(child.as_ptr() as _, "AXRole") } {
                Some(role) if role == "AXMenu" => {
                    if menu.is_some() {
                        return MenuState::Unknown;
                    }
                    menu = Some(child);
                }
                Some(_) => {}
                None => return MenuState::Unknown,
            }
        }
        let Some(menu) = menu else {
            return MenuState::ClosedOrEmpty;
        };
        match children(&menu) {
            Some(items) if items.is_empty() => MenuState::ClosedOrEmpty,
            Some(_) => MenuState::Open(menu),
            None => MenuState::Unknown,
        }
    }

    fn options(&mut self, menu: &Retained) -> ListRead<Retained> {
        use crate::ax::write_outcome::{read_value_if_allowed, ValueObs, ValueRead};
        let Some(items) = children(menu) else {
            return ListRead::Failed;
        };
        let mut result = Vec::new();
        for item in items {
            let element = item.as_ptr() as _;
            let Some(role) = (unsafe { crate::ax::bindings::copy_string_attr(element, "AXRole") })
            else {
                return ListRead::Incomplete;
            };
            let Some(title) =
                (unsafe { crate::ax::bindings::copy_string_attr(element, "AXTitle") })
            else {
                return ListRead::Incomplete;
            };
            let value = match read_value_if_allowed(
                &mut crate::ax::label_reader::NativeLabelReader,
                element,
            ) {
                ValueRead::Allowed(ValueObs::Text(value)) => value,
                ValueRead::Allowed(ValueObs::Failed(_)) => return ListRead::Incomplete,
                _ => String::new(),
            };
            if role == "AXMenuItem" {
                result.push(OptionEntry {
                    id: item,
                    title,
                    value,
                });
            }
        }
        ListRead::Complete(result)
    }

    fn idle_ms(&mut self) -> Option<u64> {
        super::super::physical_input_idle_ms()
    }
    fn now(&mut self) -> Duration {
        self.start.elapsed()
    }
    fn pause(&mut self) {
        std::thread::sleep(super::POPUP_POLL_INTERVAL);
    }
    fn open(&mut self) {
        let popup = self.popup.clone();
        self.opener = Some(std::thread::spawn(move || unsafe {
            let ptr = popup.as_ptr() as crate::ax::bindings::AXUIElementRef;
            crate::ax::bindings::AXUIElementSetMessagingTimeout(
                ptr,
                super::POPUP_PRESS_TIMEOUT_SECONDS,
            );
            let result = crate::ax::bindings::perform_action(ptr, "AXPress");
            crate::ax::bindings::AXUIElementSetMessagingTimeout(ptr, 0.0);
            result
        }));
    }
    fn opener_finished(&mut self) -> bool {
        self.opener
            .as_ref()
            .is_none_or(|thread| thread.is_finished())
    }
    fn join(&mut self) -> PressReturn {
        match self.opener.take().map(|thread| thread.join()) {
            Some(Ok(crate::ax::bindings::kAXErrorSuccess)) => PressReturn::Success,
            Some(Ok(crate::ax::bindings::kAXErrorCannotComplete)) => PressReturn::Timeout,
            Some(Ok(code)) => PressReturn::Error(code),
            _ => PressReturn::Error(crate::ax::bindings::kAXErrorFailure),
        }
    }
    fn press(&mut self, item: &Retained) -> i32 {
        unsafe { crate::ax::bindings::perform_action(item.as_ptr() as _, "AXPress") }
    }
    fn cancel(&mut self, menu: &Retained) -> i32 {
        unsafe { crate::ax::bindings::perform_action(menu.as_ptr() as _, "AXCancel") }
    }
}
