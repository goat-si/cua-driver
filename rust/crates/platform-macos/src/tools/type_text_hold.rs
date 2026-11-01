//! Untargeted type_text into a bound window that is not its app's key window
//! (400 follow-up).
//!
//! The untargeted route types into the window's focused element, and AppKit
//! reports a focused element only for the app's key window. For a window that
//! is not key, the window is held as key inside the app (the #109/#113 inline
//! scope: no activation, no raise, whether or not its app is frontmost), its
//! focused element is read and judged under the hold, and the text goes in as
//! one accessibility write. Right before that write the element must still be
//! the window's focused element and the window must still take keys. A document
//! is then marked edited with a net-zero key edit under the same checks before
//! each key, and key is handed
//! back. One blocking worker does all of it, so a dropped caller cannot cut
//! the hold short, and every outcome reports what the hold posted and how key
//! went back.

use cua_driver_core::protocol::ToolResult;

#[cfg(test)]
use crate::ax::write_outcome::ValueObs;
use crate::ax::write_outcome::{native_write_outcome, NativeWriteKind, Settable, ValueRead};
use crate::document_edit::HeldMark;
use crate::input::skylight::{scope_disclosure, HandBack, KeyEntry, ScopeEntryError};
use cua_driver_core::mutation::{ChainResult, Postcondition, PriorInput, WriteOutcome};

/// Whether an untargeted insert needs the inline hold: a window-addressed
/// background request (keyboard policy allowed, no-raise SPIs present) whose
/// window does not take keys yet. An already-key window keeps the ordinary
/// untargeted route.
pub(super) fn hold_needed(
    untargeted: bool,
    background_allowed: bool,
    spi_available: bool,
    window_routes_keys: bool,
) -> bool {
    untargeted && background_allowed && spi_available && !window_routes_keys
}

/// Whether a single accessibility insert is the right transport for the
/// app: a terminal takes text from its pty, which an AX write never
/// reaches, and a Chromium native field has its own submission route.
pub(super) fn ax_insert_route(is_terminal: bool, bundle_id: &str) -> bool {
    !is_terminal && !super::type_text::is_chromium_identity(bundle_id)
}

/// How the held window's focused element was judged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Assessment {
    /// A text input that is not a non-editing cell, in proven native content.
    Accepts,
    /// A cell of a row, outline or table that is not being edited.
    NotEditing,
    /// Focus is not on a text input: keys would drive the view.
    NotText,
    /// Web content, or ancestry that could not be read to the window: an
    /// AXValue read-back is no proof there.
    WebContent,
    /// The element could not be classified.
    Unreadable,
}

/// The pre-write checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Dispatch {
    Ok,
    /// The element is no longer the focused element of the held window, or
    /// the window no longer takes keys.
    FocusMoved,
}

/// The value and selection read right before the write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Before {
    pub value: String,
    /// UTF-16 location and length of the selection.
    pub range: (usize, usize),
}

/// What the read-back after the write shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Readback {
    /// The value is exactly the old value with the selection replaced.
    Confirmed,
    /// The value reads exactly as before the write.
    Unchanged,
    /// The write returned this AX error.
    AxError(i32),
    /// The value could not be read, or reads as something else.
    Unverifiable,
}

/// The old value with the UTF-16 range replaced by the text; None when the
/// range does not fall on character boundaries inside the value.
pub(super) fn spliced(before: &Before, text: &str) -> Option<String> {
    let (location, length) = before.range;
    let end = location.checked_add(length)?;
    let mut units = 0usize;
    let mut start_byte = None;
    let mut end_byte = None;
    for (byte, ch) in before.value.char_indices() {
        if units == location {
            start_byte = Some(byte);
        }
        if units == end {
            end_byte = Some(byte);
        }
        units += ch.len_utf16();
    }
    if units == location {
        start_byte = Some(before.value.len());
    }
    if units == end {
        end_byte = Some(before.value.len());
    }
    let (start, stop) = (start_byte?, end_byte?);
    Some(format!(
        "{}{}{}",
        &before.value[..start],
        text,
        &before.value[stop..]
    ))
}

/// Judge the write from its AX error and the value read after it. An
/// unchanged value wins over a match, so replacing a selection with the same
/// text is never confirmed.
pub(super) fn judge(before: &Before, text: &str, ax_error: i32, after: Option<&str>) -> Readback {
    if ax_error != 0 {
        return Readback::AxError(ax_error);
    }
    let Some(after) = after else {
        return Readback::Unverifiable;
    };
    if after == before.value {
        return Readback::Unchanged;
    }
    match spliced(before, text) {
        Some(expected) if expected == after => Readback::Confirmed,
        _ => Readback::Unverifiable,
    }
}

/// What the worker reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum HoldOutcome {
    /// The window could not be held as key.
    EntryRefused(String),
    /// No element of the held window has keyboard focus.
    NoFocus,
    /// The focused element is not a place to type.
    NotEditable(Assessment),
    /// The value or selection could not be read before the write.
    BeforeUnreadable,
    /// A pre-write check failed: nothing was written.
    Stopped(Dispatch),
    NotSettable,
    /// The write was sent; the document mark ran only after a confirmed one.
    Inserted {
        native: WriteOutcome,
        readback: Readback,
        mark: Option<HeldMark>,
        detail: String,
    },
}

/// The worker's result: the outcome and what the hold did inside the app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HoldStep {
    pub outcome: HoldOutcome,
    pub entry: KeyEntry,
    /// None only when entering touched nothing, so nothing was handed back.
    pub hand_back: Option<HandBack>,
}

/// The live reads and writes of the hold route (fakes in tests).
pub(super) trait HoldOps {
    type Scope;
    type Element;
    fn enter(&mut self) -> Result<Self::Scope, ScopeEntryError>;
    fn entry(&self, scope: &Self::Scope) -> KeyEntry;
    fn finish(&mut self, scope: Self::Scope) -> HandBack;
    fn focused(&mut self) -> Option<Self::Element>;
    fn assess(&mut self, element: &Self::Element) -> Assessment;
    fn before(&mut self, element: &Self::Element) -> Option<Before>;
    /// The last check, with nothing between it and the next post.
    fn can_dispatch(&mut self, element: &Self::Element) -> Dispatch;
    fn settable(&mut self, element: &Self::Element) -> Settable;
    /// The one AXSelectedText write; the AX error (0 on success).
    fn write(&mut self, element: &Self::Element, text: &str) -> i32;
    fn read_value(&mut self, element: &Self::Element) -> ValueRead;
    /// Mark the document edited, reading can_dispatch before each post.
    fn mark(&mut self, element: &Self::Element) -> HeldMark;
    /// Role and title for the result line.
    fn describe(&mut self, element: &Self::Element) -> String;
}

/// Hold, judge, write, mark and hand back, in that order, on this thread.
/// After a successful entry the scope is finished exactly once on every path.
pub(super) fn run_hold<O: HoldOps>(ops: &mut O, text: &str) -> HoldStep {
    let scope = match ops.enter() {
        Ok(scope) => scope,
        Err(error) => {
            return HoldStep {
                outcome: HoldOutcome::EntryRefused(error.reason),
                entry: error.entry,
                hand_back: error.hand_back,
            }
        }
    };
    let entry = ops.entry(&scope);
    let outcome = held(ops, text);
    let hand_back = ops.finish(scope);
    HoldStep {
        outcome,
        entry,
        hand_back: Some(hand_back),
    }
}

fn held<O: HoldOps>(ops: &mut O, text: &str) -> HoldOutcome {
    let Some(element) = ops.focused() else {
        return HoldOutcome::NoFocus;
    };
    match ops.assess(&element) {
        Assessment::Accepts => {}
        other => return HoldOutcome::NotEditable(other),
    }
    let Some(before) = ops.before(&element) else {
        return HoldOutcome::BeforeUnreadable;
    };
    match ops.can_dispatch(&element) {
        Dispatch::Ok => {}
        stopped => return HoldOutcome::Stopped(stopped),
    }
    let settable = ops.settable(&element);
    if settable == Settable::No {
        let outcome = native_write_outcome(NativeWriteKind::SetAttribute, false, &settable, 0);
        debug_assert!(matches!(outcome, WriteOutcome::FallThrough(_)));
        return HoldOutcome::NotSettable;
    }
    let ax_error = ops.write(&element, text);
    let native = native_write_outcome(NativeWriteKind::SetAttribute, true, &settable, ax_error);
    let after = ops.read_value(&element);
    let readback = match (native, after) {
        (WriteOutcome::Delivered, ValueRead::Withheld) => Readback::Unverifiable,
        (WriteOutcome::Delivered, value) => judge(&before, text, ax_error, value.text().as_deref()),
        (_, _) => Readback::AxError(ax_error),
    };
    let mark = (native == WriteOutcome::Delivered && readback == Readback::Confirmed)
        .then(|| ops.mark(&element));
    HoldOutcome::Inserted {
        native,
        readback,
        mark,
        detail: ops.describe(&element),
    }
}

const TOKEN_ADVICE: &str =
    "To type into this window, pass the text field's element_token from get_window_state.";

fn hand_back_label(hand_back: HandBack) -> &'static str {
    match hand_back {
        HandBack::NotMoved => "not_moved",
        HandBack::Restored => "restored",
        HandBack::Deferred => "deferred",
        HandBack::NoTarget => "no_target",
        HandBack::NotConfirmed => "not_confirmed",
    }
}

fn entry_touched(entry: KeyEntry) -> bool {
    entry.records_posted > 0 || entry.records_uncertain || entry.ax_write
}

fn key_hold_json(wid: u32, entry: KeyEntry, hand_back: Option<HandBack>) -> serde_json::Value {
    serde_json::json!({
        "window_id": wid,
        "records_posted": entry.records_posted,
        "records_uncertain": entry.records_uncertain,
        "focused_window_written": entry.ax_write,
        "hand_back": hand_back.map(hand_back_label),
    })
}

/// What holding the window did inside the app, or nothing when it touched
/// nothing.
fn hold_note(wid: u32, entry: KeyEntry, hand_back: Option<HandBack>) -> String {
    let disclosure = scope_disclosure(entry, hand_back);
    if !entry_touched(entry) || disclosure.is_empty() {
        return disclosure;
    }
    format!(
        "Window {wid} was held as key inside its app without activating or raising it: {disclosure}"
    )
}

/// The tool result for every hold-route outcome.
pub(super) fn hold_result(step: HoldStep, pid: i32, wid: u32, char_count: usize) -> ToolResult {
    let HoldStep {
        outcome,
        entry,
        hand_back,
    } = step;
    let note = hold_note(wid, entry, hand_back);
    let with_note = |text: String| {
        if note.is_empty() {
            text
        } else {
            format!("{text} {note}")
        }
    };
    let key_hold = key_hold_json(wid, entry, hand_back);
    let prior = if entry_touched(entry) || hand_back.is_some_and(|h| h != HandBack::NotMoved) {
        PriorInput::MayHaveDelivered
    } else {
        PriorInput::None
    };
    let recorded = match &outcome {
        HoldOutcome::Inserted { native, .. } => {
            Some(ChainResult::from_recorded(vec![*native], prior).expect("one held insert setter"))
        }
        HoldOutcome::NotSettable => Some(
            ChainResult::from_recorded(
                vec![native_write_outcome(
                    NativeWriteKind::SetAttribute,
                    false,
                    &Settable::No,
                    0,
                )],
                prior,
            )
            .expect("one skipped held insert setter"),
        ),
        _ => None,
    };
    let nothing = if prior != PriorInput::None {
        "No text was typed."
    } else {
        "Nothing was sent."
    };
    let refused = |code: &str, reason: String| {
        let message = with_note(format!("{reason} {nothing} {TOKEN_ADVICE}"));
        ToolResult::error(format!("type_text stopped ({code}): {message}"))
            .with_structured(serde_json::json!({
                "code": code,
                "effect": if prior == PriorInput::None { "refused" } else { "unverifiable" },
                "pid": pid,
                "window_id": wid,
                "keys_sent": false,
                "text_sent": false,
                "message": message,
                "key_hold": key_hold.clone(),
                "escalation": { "recommended": "element_token", "reason": TOKEN_ADVICE },
            }))
            .with_disposition(if prior == PriorInput::None {
                cua_driver_contract::ErrorDisposition::NOT_DELIVERED
            } else {
                cua_driver_contract::ErrorDisposition::SUSPECTED_NOOP
            })
    };
    let unverifiable = |message: String, extra: serde_json::Value| {
        let message = with_note(message);
        let mut structured = serde_json::json!({
            "code": "type_text_unverified",
            "path": "ax",
            "verified": false,
            "effect": "unverifiable",
            "pid": pid,
            "window_id": wid,
            "message": message,
            "key_hold": key_hold.clone(),
        });
        if let (Some(target), Some(extra)) = (structured.as_object_mut(), extra.as_object()) {
            target.extend(extra.clone());
        }
        ToolResult::text(format!("📨 Sent (unverified) {message}")).with_structured(structured)
    };
    match outcome {
        HoldOutcome::EntryRefused(reason) => refused(
            super::WINDOW_NOT_KEY,
            format!(
                "window {wid} is not its app's key window and could not be held as key: {reason}."
            ),
        ),
        HoldOutcome::NoFocus => refused(
            "no_focused_text_input",
            format!("with window {wid} held as key, no element in it has keyboard focus."),
        ),
        HoldOutcome::NotEditable(assessment) => {
            let (code, reason) = match assessment {
                Assessment::NotEditing => (
                    "not_editable",
                    "the focused element of the held window is a cell in a row, outline or table that is not being edited.",
                ),
                Assessment::WebContent => (
                    "no_focused_text_input",
                    "the focused element of the held window is web content, or its ancestry could not be read, so an accessibility read-back cannot confirm the text.",
                ),
                Assessment::NotText => (
                    "no_focused_text_input",
                    "the focused element of the held window is not a text input, so typed text would act on the window instead.",
                ),
                Assessment::Unreadable | Assessment::Accepts => (
                    "not_editable",
                    "the focused element of the held window could not be read, so editability is unknown.",
                ),
            };
            refused(code, reason.to_owned())
        }
        HoldOutcome::BeforeUnreadable => refused(
            "no_focused_text_input",
            "the focused text input's value or selection could not be read, so the insert could not be checked."
                .to_owned(),
        ),
        HoldOutcome::Stopped(_) => refused(
            "no_focused_text_input",
            "focus left the element, or the window stopped taking keys, right before the insert, so it was not written."
                .to_owned(),
        ),
        HoldOutcome::NotSettable => refused("no_focused_text_input", "the focused element does not support AXSelectedText.".into()),
        HoldOutcome::Inserted {
            readback: Readback::Confirmed,
            mark,
            detail,
            ..
        } => {
            let mark = mark.unwrap_or(HeldMark::NothingSent);
            let clean = matches!(mark, HeldMark::Marked | HeldMark::NotADocument);
            let message = with_note(format!(
                "Inserted {char_count} char(s) into {detail}.{}",
                mark.note()
            ));
            let structured = serde_json::json!({
                "path": "ax",
                "verified": clean,
                "effect": if clean { "confirmed" } else { "unverifiable" },
                "pid": pid,
                "window_id": wid,
                "delivered_chars": char_count,
                "document_marked": mark.label(),
                "message": message,
                "key_hold": key_hold.clone(),
            });
            let mark_sign = if clean { "✅" } else { "📨" };
            ToolResult::text(format!("{mark_sign} {message}")).with_structured(structured)
        }
        HoldOutcome::Inserted {
            readback: Readback::Unchanged,
            detail,
            ..
        } => unverifiable(
            format!(
                "{char_count} char(s) as one accessibility write into {detail}, but the value reads unchanged; read the window before sending it again."
            ),
            serde_json::json!({ "readback": "unchanged" }),
        ),
        HoldOutcome::Inserted {
            readback: Readback::AxError(error),
            detail,
            ..
        } => {
            let chain = recorded.expect("insert has a recorded native outcome");
            let message = with_note(format!("AXSelectedText into {detail} returned error {error}; read the window before retrying."));
            let mut result = ToolResult::error(message.clone()).with_structured(serde_json::json!({
                "code": "type_text_unverified", "path": "ax", "verified": false,
                "effect": if chain.no_input() { "refused" } else { "unverifiable" },
                "pid": pid, "window_id": wid, "message": message, "key_hold": key_hold,
                "readback": "ax_error", "ax_error": error,
            }));
            if let Some(disposition) = chain.error_disposition(Postcondition::Indeterminate, false) { result = result.with_disposition(disposition); }
            result
        },
        HoldOutcome::Inserted { detail, .. } => unverifiable(
            format!(
                "{char_count} char(s) as one accessibility write into {detail}; the value could not be read back as the expected insert, so read the window before sending it again."
            ),
            serde_json::json!({ "readback": "unverifiable" }),
        ),
    }
}

/// Run the hold route in its own task: the mutation lease, the focus
/// suppression, the window observation and the key hold all live in it, so
/// a dropped caller cannot end any of them before the hand-back.
pub(super) async fn held_window_insert(
    lease: Option<super::BackgroundMutationLease>,
    pid: i32,
    wid: u32,
    text: String,
) -> ToolResult {
    let char_count = text.chars().count();
    let task = crate::input::worker_context::spawn_async(async move {
        let _lease = lease;
        // Window enumeration blocks: off the runtime workers.
        let observed = crate::goat::action_latency::spawn_action_blocking(|| {
            let prior_front = crate::apps::frontmost_pid();
            (
                prior_front,
                crate::window_change_detector::WindowChangeDetector::snapshot(prior_front),
            )
        })
        .await;
        let (prior_front, snapshot) = match observed {
            Ok(observed) => observed,
            Err(error) => return (Err(error), String::new()),
        };
        let step = crate::focus_guard::with_focus_suppressed(
            Some(pid),
            prior_front,
            "type_text.held_window",
            || async move {
                crate::goat::action_latency::spawn_action_blocking(move || {
                    run_hold(&mut LiveHoldOps { pid, wid }, &text)
                })
                .await
            },
        )
        .await;
        let changes = super::finish_window_observation(snapshot).await;
        (step, changes.result_suffix())
    });
    match task.await {
        Ok((Ok(step), suffix)) => append_text(hold_result(step, pid, wid, char_count), &suffix),
        Ok((Err(error), _)) | Err(error) => {
            let message = format!(
                "type_text: the held-window insert task failed ({error}). Text may have been \
                 inserted into window {wid}, and key may have been handed back during unwinding \
                 or left on it; read the window before sending it again."
            );
            ToolResult::error(message.clone())
                .with_structured(serde_json::json!({
                    "code": "type_text_unverified",
                    "effect": "unverifiable",
                    "pid": pid,
                    "window_id": wid,
                    "message": message,
                }))
                .with_disposition(cua_driver_contract::ErrorDisposition::SUSPECTED_NOOP)
        }
    }
}

/// Append the suffix to the result's first text.
fn append_text(mut result: ToolResult, suffix: &str) -> ToolResult {
    if suffix.is_empty() {
        return result;
    }
    if let Some(cua_driver_core::protocol::Content::Text { text, .. }) = result.content.first_mut()
    {
        text.push_str(suffix);
    }
    result
}

/// The live hold route on one window.
pub(super) struct LiveHoldOps {
    pub pid: i32,
    pub wid: u32,
}

impl HoldOps for LiveHoldOps {
    type Scope = crate::input::skylight::WindowKeyScope;
    type Element = crate::ax::snapshot::RetainedElement;

    fn enter(&mut self) -> Result<Self::Scope, ScopeEntryError> {
        crate::input::skylight::enter_exact_window_key_inline_detailed(
            self.pid as libc::pid_t,
            self.wid,
        )
    }

    fn entry(&self, scope: &Self::Scope) -> KeyEntry {
        scope.entry()
    }

    fn finish(&mut self, scope: Self::Scope) -> HandBack {
        scope.finish()
    }

    fn focused(&mut self) -> Option<Self::Element> {
        let ptr =
            unsafe { crate::ax::exact_target::focused_element_in_window(self.pid, self.wid) }?;
        // SAFETY: ptr is a live +1 element; retain takes its own reference
        // before ours is released.
        let owned = unsafe { crate::ax::snapshot::RetainedElement::retain(ptr as usize) };
        unsafe { core_foundation::base::CFRelease(ptr as _) };
        Some(owned)
    }

    fn assess(&mut self, element: &Self::Element) -> Assessment {
        let ptr = element.as_ptr();
        match unsafe { super::not_editable::classify_element(ptr as _) } {
            crate::ax::editable::CellVerdict::NotEditing => return Assessment::NotEditing,
            crate::ax::editable::CellVerdict::Unreadable => return Assessment::Unreadable,
            _ => {}
        }
        if !super::type_text::target_is_proven_native(self.pid, ptr, self.wid) {
            return Assessment::WebContent;
        }
        if unsafe { super::type_text::focused_accepts_text(ptr as _) } {
            Assessment::Accepts
        } else {
            Assessment::NotText
        }
    }

    fn before(&mut self, element: &Self::Element) -> Option<Before> {
        let ptr = element.as_ptr() as crate::ax::bindings::AXUIElementRef;
        let value = unsafe { crate::ax::write_outcome::read_text_value(ptr) }?;
        let range = unsafe { crate::ax::tree::read_selected_text_range(ptr) }?;
        Some(Before {
            value,
            range: (
                usize::try_from(range.location).ok()?,
                usize::try_from(range.length).ok()?,
            ),
        })
    }

    fn can_dispatch(&mut self, element: &Self::Element) -> Dispatch {
        let ptr = element.as_ptr();
        // The app's focused element itself, in this window: AXFocused alone
        // reads true for a text view whose window is not key.
        let still_focused = unsafe {
            crate::ax::exact_target::focused_element_in_window(self.pid, self.wid).is_some_and(
                |focused| {
                    let same = core_foundation::base::CFEqual(
                        focused as core_foundation::base::CFTypeRef,
                        ptr as core_foundation::base::CFTypeRef,
                    ) != 0;
                    core_foundation::base::CFRelease(focused as _);
                    same
                },
            )
        };
        if !still_focused
            || !crate::input::skylight::window_routes_keys(self.pid as libc::pid_t, self.wid)
        {
            return Dispatch::FocusMoved;
        }
        Dispatch::Ok
    }

    fn settable(&mut self, element: &Self::Element) -> Settable {
        unsafe { crate::ax::write_outcome::query_settable(element.as_ptr() as _, "AXSelectedText") }
    }

    fn write(&mut self, element: &Self::Element, text: &str) -> i32 {
        unsafe {
            crate::ax::bindings::set_string_attr(
                element.as_ptr() as crate::ax::bindings::AXUIElementRef,
                "AXSelectedText",
                text,
            )
        }
    }

    fn read_value(&mut self, element: &Self::Element) -> ValueRead {
        crate::ax::write_outcome::read_value_if_allowed(
            &mut crate::ax::label_reader::NativeLabelReader,
            element.as_ptr() as crate::ax::bindings::AXUIElementRef,
        )
    }

    fn mark(&mut self, element: &Self::Element) -> HeldMark {
        let (pid, wid) = (self.pid, self.wid);
        let ptr = element.as_ptr();
        let mut ops = LiveHoldOps { pid, wid };
        let probe = element.clone();
        unsafe {
            crate::document_edit::ensure_marked_in_held_window(
                pid,
                ptr as crate::ax::bindings::AXUIElementRef,
                || ops.can_dispatch(&probe) == Dispatch::Ok,
            )
        }
    }

    fn describe(&mut self, element: &Self::Element) -> String {
        let ptr = element.as_ptr() as crate::ax::bindings::AXUIElementRef;
        let role =
            unsafe { crate::ax::bindings::copy_string_attr(ptr, "AXRole") }.unwrap_or_default();
        format!("{role} of window {}", self.wid)
    }
}

#[cfg(test)]
mod secure_read_tests {
    use super::tests::{run, Fake};

    #[test]
    fn hold_before_withheld_refuses_without_sending() {
        let mut fake = Fake::ok();
        fake.before = None;
        let (_, result) = run(&mut fake);
        assert!(!fake.log.contains(&"write"));
        let data = result.structured_content.unwrap();
        assert_eq!(data["effect"], "unverifiable");
        assert_eq!(data["text_sent"], false);
        assert!(!fake.log.contains(&"mark"));
        assert_eq!(
            fake.log.iter().filter(|event| **event == "finish").count(),
            1
        );
    }

    #[test]
    fn hold_after_withheld_is_sent_unverifiable() {
        let mut fake = Fake::ok();
        fake.after_withheld = true;
        fake.ax_error = -25204;
        let (_, result) = run(&mut fake);
        assert_eq!(
            fake.log.iter().filter(|event| **event == "write").count(),
            1
        );
        assert!(!fake.log.contains(&"mark"));
        let data = result.structured_content.unwrap();
        assert_eq!(data["effect"], "unverifiable");
        assert_eq!(data["readback"], "ax_error");
        assert_eq!(data["ax_error"], -25204);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scripted live reads and writes, with every call logged.
    pub(super) struct Fake {
        enter: Result<(), (KeyEntry, Option<HandBack>)>,
        entry: KeyEntry,
        hand_back: HandBack,
        focused: bool,
        assessment: Assessment,
        pub(super) before: Option<Before>,
        dispatch: Dispatch,
        pub(super) ax_error: i32,
        pub(super) after: Option<String>,
        pub(super) after_withheld: bool,
        pub(super) settable: Settable,
        mark: HeldMark,
        pub(super) log: Vec<&'static str>,
    }

    impl Fake {
        pub(super) fn ok() -> Self {
            Self {
                enter: Ok(()),
                entry: KeyEntry {
                    records_posted: 4,
                    records_uncertain: false,
                    ax_write: false,
                },
                hand_back: HandBack::Restored,
                focused: true,
                assessment: Assessment::Accepts,
                before: Some(Before {
                    value: "hello alpha".into(),
                    range: (0, 0),
                }),
                dispatch: Dispatch::Ok,
                ax_error: 0,
                after: Some(" TOKhello alpha".into()),
                after_withheld: false,
                settable: Settable::Yes,
                mark: HeldMark::Marked,
                log: Vec::new(),
            }
        }
    }

    impl HoldOps for Fake {
        type Scope = ();
        type Element = ();
        fn enter(&mut self) -> Result<(), ScopeEntryError> {
            self.log.push("enter");
            self.enter.map_err(|(entry, hand_back)| ScopeEntryError {
                reason: "window 7 could not be confirmed as the app's key window".into(),
                entry,
                hand_back,
            })
        }
        fn entry(&self, _: &()) -> KeyEntry {
            self.entry
        }
        fn finish(&mut self, _: ()) -> HandBack {
            self.log.push("finish");
            self.hand_back
        }
        fn focused(&mut self) -> Option<()> {
            self.log.push("focused");
            self.focused.then_some(())
        }
        fn assess(&mut self, _: &()) -> Assessment {
            self.log.push("assess");
            self.assessment
        }
        fn before(&mut self, _: &()) -> Option<Before> {
            self.log.push("before");
            self.before.clone()
        }
        fn can_dispatch(&mut self, _: &()) -> Dispatch {
            self.log.push("can_dispatch");
            self.dispatch
        }
        fn settable(&mut self, _: &()) -> Settable {
            self.settable
        }
        fn write(&mut self, _: &(), _: &str) -> i32 {
            self.log.push("write");
            self.ax_error
        }
        fn read_value(&mut self, _: &()) -> ValueRead {
            self.log.push("read_value");
            if self.after_withheld {
                ValueRead::Withheld
            } else {
                ValueRead::Allowed(
                    self.after
                        .clone()
                        .map(ValueObs::Text)
                        .unwrap_or(ValueObs::Missing),
                )
            }
        }
        fn mark(&mut self, _: &()) -> HeldMark {
            self.log.push("mark");
            self.mark
        }
        fn describe(&mut self, _: &()) -> String {
            "AXTextArea of window 7".into()
        }
    }

    fn structured(result: &ToolResult) -> serde_json::Value {
        result.structured_content.clone().unwrap()
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

    pub(super) fn run(fake: &mut Fake) -> (HoldStep, ToolResult) {
        let step = run_hold(fake, " TOK");
        let result = hold_result(step.clone(), 42, 7, 4);
        (step, result)
    }

    #[test]
    fn the_hold_is_needed_only_for_an_untargeted_background_insert_into_a_non_key_window() {
        assert!(hold_needed(true, true, true, false));
        assert!(!hold_needed(true, true, true, true), "already key");
        assert!(
            !hold_needed(false, true, true, false),
            "element or x,y given"
        );
        assert!(
            !hold_needed(true, false, true, false),
            "foreground or semantic-only"
        );
        assert!(!hold_needed(true, true, false, false), "no SPIs");
        assert!(ax_insert_route(false, "com.apple.TextEdit"));
        assert!(!ax_insert_route(true, "com.apple.Terminal"), "terminal");
        assert!(!ax_insert_route(false, "com.google.Chrome"), "Chromium");
    }

    #[test]
    fn a_confirmed_insert_marks_the_document_and_hands_key_back() {
        let mut fake = Fake::ok();
        let (_, result) = run(&mut fake);
        assert_eq!(
            fake.log,
            [
                "enter",
                "focused",
                "assess",
                "before",
                "can_dispatch",
                "write",
                "read_value",
                "mark",
                "finish"
            ]
        );
        let data = structured(&result);
        assert_eq!(data["effect"], "confirmed");
        assert_eq!(data["document_marked"], "marked");
        assert_eq!(data["key_hold"]["records_posted"], 4);
        assert_eq!(data["key_hold"]["hand_back"], "restored");
        let text = text_of(&result);
        assert!(text.contains("held as key"), "{text}");
        assert!(text.contains("went back"), "{text}");
    }

    #[test]
    fn an_entry_refusal_sends_nothing_and_asks_for_the_element_token() {
        let mut fake = Fake::ok();
        fake.enter = Err((KeyEntry::default(), None));
        let (_, result) = run(&mut fake);
        assert_eq!(fake.log, ["enter"], "no finish after a failed entry");
        let data = structured(&result);
        assert_eq!(data["code"], "window_not_key");
        assert_eq!(data["effect"], "refused");
        assert_eq!(data["keys_sent"], false);
        let text = text_of(&result);
        assert!(text.contains("Nothing was sent."), "{text}");
        assert!(text.contains("element_token"), "{text}");
    }

    #[test]
    fn an_entry_refusal_after_posted_records_discloses_them() {
        let mut fake = Fake::ok();
        fake.enter = Err((
            KeyEntry {
                records_posted: 2,
                records_uncertain: false,
                ax_write: true,
            },
            Some(HandBack::Restored),
        ));
        let (_, result) = run(&mut fake);
        let text = text_of(&result);
        assert!(!text.contains("Nothing was sent"), "{text}");
        assert!(text.contains("No text was typed."), "{text}");
        assert!(text.contains("2 key-window record(s)"), "{text}");
        assert_eq!(
            structured(&result)["key_hold"]["focused_window_written"],
            true
        );
    }

    #[test]
    fn every_refusal_under_the_hold_hands_key_back_once_and_writes_nothing() {
        type Setup = Box<dyn Fn(&mut Fake)>;
        let cases: Vec<(&str, Setup)> = vec![
            ("no_focused_text_input", Box::new(|f| f.focused = false)),
            (
                "not_editable",
                Box::new(|f| f.assessment = Assessment::NotEditing),
            ),
            (
                "not_editable",
                Box::new(|f| f.assessment = Assessment::Unreadable),
            ),
            (
                "no_focused_text_input",
                Box::new(|f| f.assessment = Assessment::NotText),
            ),
            (
                "no_focused_text_input",
                Box::new(|f| f.assessment = Assessment::WebContent),
            ),
            ("no_focused_text_input", Box::new(|f| f.before = None)),
            (
                "no_focused_text_input",
                Box::new(|f| f.dispatch = Dispatch::FocusMoved),
            ),
        ];
        for (code, setup) in cases {
            let mut fake = Fake::ok();
            setup(&mut fake);
            let (_, result) = run(&mut fake);
            assert!(!fake.log.contains(&"write"), "{code}: {:?}", fake.log);
            assert!(!fake.log.contains(&"mark"), "{code}");
            assert_eq!(
                fake.log.iter().filter(|c| **c == "finish").count(),
                1,
                "{code}"
            );
            assert_eq!(fake.log.last(), Some(&"finish"), "{code}");
            let data = structured(&result);
            assert_eq!(data["code"], code);
            assert_eq!(data["effect"], "unverifiable");
            assert_eq!(data["text_sent"], false);
            let text = text_of(&result);
            assert!(text.contains("No text was typed."), "{code}: {text}");
            assert!(text.contains("element_token"), "{code}: {text}");
        }
        let mut zero = Fake::ok();
        zero.entry = KeyEntry::default();
        zero.hand_back = HandBack::NotMoved;
        zero.before = None;
        let (_, result) = run(&mut zero);
        assert_eq!(structured(&result)["effect"], "refused");
        assert_eq!(structured(&result)["text_sent"], false);
        assert!(!zero.log.contains(&"write"));
        assert!(!zero.log.contains(&"mark"));
        assert_eq!(zero.log.iter().filter(|c| **c == "finish").count(), 1);
    }

    #[test]
    fn readback_confirms_only_the_exact_splice() {
        let before = Before {
            value: "a alpha b".into(),
            range: (2, 5),
        };
        assert_eq!(
            judge(&before, "BETA", 0, Some("a BETA b")),
            Readback::Confirmed
        );
        // The text already present elsewhere does not confirm.
        let caret = Before {
            value: "TOK".into(),
            range: (3, 0),
        };
        assert_eq!(judge(&caret, "TOK", 0, Some("TOK")), Readback::Unchanged);
        assert_eq!(
            judge(&caret, "TOK", 0, Some("XTOK")),
            Readback::Unverifiable
        );
        // Replacing a selection with the same text reads unchanged.
        let same = Before {
            value: "a alpha b".into(),
            range: (2, 5),
        };
        assert_eq!(
            judge(&same, "alpha", 0, Some("a alpha b")),
            Readback::Unchanged
        );
        // An AX error or an unreadable value never confirms.
        assert_eq!(
            judge(&before, "BETA", -25200, Some("a BETA b")),
            Readback::AxError(-25200)
        );
        assert_eq!(judge(&before, "BETA", 0, None), Readback::Unverifiable);
        // UTF-16 offsets around a surrogate pair.
        let emoji = Before {
            value: "😀x".into(),
            range: (2, 1),
        };
        assert_eq!(spliced(&emoji, "y").as_deref(), Some("😀y"));
        let inside = Before {
            value: "😀x".into(),
            range: (1, 0),
        };
        assert_eq!(spliced(&inside, "y"), None);
    }

    #[test]
    fn an_unconfirmed_write_is_unverifiable_and_never_marks() {
        for (after, ax_error, readback) in [
            (Some("hello alpha"), 0, "unchanged"),
            (Some("something else"), 0, "unverifiable"),
            (None, 0, "unverifiable"),
            (Some(" TOKhello alpha"), -25204, "ax_error"),
        ] {
            let mut fake = Fake::ok();
            fake.after = after.map(str::to_owned);
            fake.ax_error = ax_error;
            let (_, result) = run(&mut fake);
            assert!(!fake.log.contains(&"mark"), "{readback}");
            assert_eq!(fake.log.last(), Some(&"finish"));
            let data = structured(&result);
            assert_eq!(data["effect"], "unverifiable", "{readback}");
            assert_eq!(data["readback"], readback);
            assert_eq!(result.is_error == Some(true), ax_error != 0);
        }
    }

    #[test]
    fn a_mark_that_may_have_left_a_space_makes_the_insert_unverifiable() {
        for mark in [
            HeldMark::SpaceMayRemain,
            HeldMark::Unconfirmed,
            HeldMark::NothingSent,
            HeldMark::DocumentUnknown,
        ] {
            let mut fake = Fake::ok();
            fake.mark = mark;
            let (_, result) = run(&mut fake);
            let data = structured(&result);
            assert_eq!(data["effect"], "unverifiable", "{mark:?}");
            assert_eq!(data["document_marked"], mark.label());
            assert!(text_of(&result).contains("Warning"), "{mark:?}");
        }
        let mut fake = Fake::ok();
        fake.mark = HeldMark::NotADocument;
        assert_eq!(structured(&run(&mut fake).1)["effect"], "confirmed");
    }
}

#[cfg(test)]
mod u32_outcome_tests {
    use super::tests::{run, Fake};
    use super::*;

    #[test]
    fn unchanged_splice_keeps_existing_unverified_contract() {
        let mut fake = Fake::ok();
        fake.before = Some(Before {
            value: "same".into(),
            range: (0, 4),
        });
        fake.after = Some("same".into());
        let step = run_hold(&mut fake, "same");
        let result = hold_result(step, 42, 7, 4);
        let data = result.structured_content.unwrap();
        assert_eq!(data["effect"], "unverifiable");
        assert_eq!(data["readback"], "unchanged");
        assert!(!fake.log.contains(&"mark"));
        assert_eq!(
            fake.log.iter().filter(|event| **event == "write").count(),
            1
        );
        assert_eq!(
            fake.log.iter().filter(|event| **event == "finish").count(),
            1
        );
    }

    #[test]
    fn withheld_after_write_keeps_handback_and_suppresses_mark() {
        let mut fake = Fake::ok();
        fake.after_withheld = true;
        fake.ax_error = crate::ax::bindings::kAXErrorCannotComplete;
        let (_, result) = run(&mut fake);
        let data = result.structured_content.unwrap();
        assert_eq!(data["effect"], "unverifiable");
        assert_eq!(
            data["ax_error"],
            crate::ax::bindings::kAXErrorCannotComplete
        );
        assert_eq!(data["key_hold"]["hand_back"], "restored");
        assert_eq!(
            result.disposition,
            Some(cua_driver_contract::ErrorDisposition::SUSPECTED_NOOP)
        );
        assert!(!fake.log.contains(&"mark"));
        assert_eq!(
            fake.log.iter().filter(|event| **event == "write").count(),
            1
        );
        assert_eq!(
            fake.log.iter().filter(|event| **event == "finish").count(),
            1
        );
        let mut unsupported = Fake::ok();
        unsupported.settable = Settable::No;
        let (_, result) = run(&mut unsupported);
        assert_eq!(result.structured_content.unwrap()["text_sent"], false);
        assert!(!unsupported.log.contains(&"write"));
        assert!(!unsupported.log.contains(&"mark"));
    }
}
