//! set_value tool — matches the Swift reference in SetValueTool.swift.
//!
//! Two modes, determined by the element's AXRole:
//!
//! * **AXPopUpButton**: Find the option (a child, or an `AXMenuItem` under the
//!   popup's `AXMenu`) whose AXTitle or AXValue matches `value`
//!   (case-insensitive) and AXPress it.  AppKit and Chromium popups publish
//!   their items only while the menu is open, so the menu is opened for the
//!   selection and closed again.  Safari `<select>` elements are set through
//!   `osascript do JavaScript` instead.
//!
//! * **Everything else**: Write `AXValue` directly (sliders, steppers, native
//!   text fields that expose a settable AXValue).

use crate::goat::action_latency as latency;

use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::sync::Arc;

use crate::apps;
use crate::ax::bindings::{
    copy_action_names, copy_bool_attr, copy_children, copy_string_attr, copy_url_attr,
    kAXErrorSuccess, perform_action, set_number_attr, set_string_attr, AXUIElementRef,
};
use crate::ax::write_outcome::{
    classify_attr_write, query_settable, read_value_typed, AttrWrite, Settable, ValueObs,
};
use crate::focus_guard;
use crate::window_change_detector::WindowChangeDetector;
use core_foundation::base::CFRelease;

use super::ToolState;

pub struct SetValueTool {
    state: Arc<ToolState>,
}

impl SetValueTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "set_value".into(),
        description: "Set an element's value by `element_token`. Popup button / select: selects the item whose title \
            or value matches `value` (case-insensitive); Safari `<select>` through the DOM, AppKit and \
            Chromium popups by opening the menu, pressing the item and closing it (the user's front window \
            keeps key focus). An unknown value lists the options. Other elements: writes AXValue (sliders, \
            steppers, date pickers, native text fields). A Finder file name cell (list or icon view, not \
            being edited) is renamed through Finder's scripting interface in the background and read back \
            from disk; `value` is the full new name with its extension (macOS asks once to allow controlling \
            Finder). Refusals: Finder's Get Info `Name` field, `file_name_needs_rename`; Safari's address \
            field, `address_field_not_settable` (the refusal names the route that opens an address). WebKit \
            ignores AXValue writes to web inputs: use `type_text_chars`.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["value"],
            "properties": {
                "session": { "type": "string", "description": "For multi-call work, prefer a short public session label and repeat it on every call that accepts it." },
                "pid": { "type": "integer", "description": "Target process ID. Optional with element_token (the token carries the pid)." },
                "window_id": {
                    "type": "integer",
                    "description": "Window ID. Omit with element_token."
                },
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "value": {
                    "type": "string",
                    "description": "New value, coerced to the element's type."
                },
                "autosubmit": { "type": "boolean", "description": "For a search field, post Return after setting the value. Default false; submission is not verified." }
            },
            "additionalProperties": false
        }),
        read_only:   false,
        destructive: true,
        idempotent:  true,
        open_world:  true,
    })
}

#[async_trait]
impl Tool for SetValueTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let autosubmit = match super::set_value_autosubmit::enabled(&args) {
            Ok(value) => value,
            Err(reason) => return super::invalid_arguments(reason),
        };
        let pid = match super::target_pid(&self.state, &args) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let value = match args.require_str("value") {
            Ok(v) => v,
            Err(e) => return e,
        };

        let (element_index, window_id, element_guard) = match latency::resolve(|| {
            self.state.snapshots.resolve(pid, &args)
        }) {
            Ok(cua_driver_core::element_token::ResolvedElement::None) => {
                return super::invalid_arguments(
                    "set_value requires element_token to address the target element.",
                )
            }
            Ok(cua_driver_core::element_token::ResolvedElement::Element {
                window_id,
                element_index,
                element,
            }) => match u32::try_from(window_id) {
                Ok(window_id) => (element_index, window_id, element),
                Err(_) => return super::invalid_arguments("window_id is out of range for macOS."),
            },
            Err(refusal) => return refusal,
        };

        let element_ptr = element_guard.as_ptr();

        // set_value is an always-background semantic AX mutation. Re-prove
        // that the retained element still belongs to the requested exact
        // window immediately before any cursor or AX work; a cache hit alone
        // is not delivery proof after a window lifecycle or Space change.
        let _mutation_lease = match super::gate_background_window_action(
            pid,
            window_id,
            Some(element_ptr),
            cua_driver_core::background_input::BackgroundAction::AxSemantic,
        )
        .await
        {
            Ok(lease) => lease,
            Err(refusal_result) => return refusal_result,
        };

        // A file's name as Finder lists it takes an AXValue write and reads it
        // back, but the file is never renamed. Refuse before anything moves.
        // Finder's Get Info Name field does the same.
        // In Finder, the file is renamed through Finder's scripting interface
        // instead, with Finder left in the background, and read back from
        // disk.
        let name_guard = element_guard.clone();
        let name_target = latency::spawn_action_blocking(move || unsafe {
            let element = name_guard.as_ptr() as AXUIElementRef;
            if file_name_cell(element) {
                let finder =
                    crate::apps::bundle_id_for_pid(pid).as_deref() == Some("com.apple.finder");
                match copy_url_attr(element)
                    .as_deref()
                    .and_then(crate::apple_events::file_url_to_path)
                {
                    Some(path) if finder => Some(NameTarget::FinderItem(path)),
                    _ => Some(NameTarget::Refuse(LIST_RENAME_ROUTE)),
                }
            } else if get_info_name_field(pid, element) {
                Some(NameTarget::Refuse(GET_INFO_RENAME_ROUTE))
            } else {
                None
            }
        })
        .await
        .ok()
        .flatten();
        match name_target {
            Some(NameTarget::Refuse(reason)) => {
                return file_name_needs_rename(pid, window_id, reason)
            }
            Some(NameTarget::FinderItem(path)) => {
                if autosubmit {
                    return super::set_value_autosubmit::non_search_refusal();
                }
                let new_name = value.clone();
                let rename_path = path.clone();
                let renamed = latency::spawn_action_blocking(move || {
                    crate::apple_events::finder_rename(&rename_path, &new_name)
                })
                .await;
                return finder_rename_result(pid, window_id, &path, &value, renamed);
            }
            None => {}
        }

        let address_guard = element_guard.clone();
        let address_field = latency::spawn_action_blocking(move || unsafe {
            let identifier =
                copy_string_attr(address_guard.as_ptr() as AXUIElementRef, "AXIdentifier");
            is_safari_address_field(
                crate::apps::bundle_id_for_pid(pid).as_deref(),
                identifier.as_deref(),
            )
        })
        .await
        .unwrap_or(false);
        if address_field {
            return address_field_refusal(pid, window_id);
        }

        if autosubmit {
            let guard = element_guard.clone();
            let search = latency::spawn_action_blocking(move || unsafe {
                let element = guard.as_ptr() as AXUIElementRef;
                let role = copy_string_attr(element, "AXRole");
                let subrole = copy_string_attr(element, "AXSubrole");
                super::set_value_autosubmit::search_field(role.as_deref(), subrole.as_deref())
            })
            .await;
            match search {
                Ok(true) => {}
                Ok(false) => return super::set_value_autosubmit::non_search_refusal(),
                Err(error) => return super::pre_dispatch_refusal(
                    "autosubmit_target_unreadable",
                    format!("Could not classify the autosubmit target ({error}); no value or key was sent."),
                ),
            }
        }

        // A text field in a minimized window: restore the window for the
        // write (its key priming and focus need a visible window), then
        // minimize it again, all under this lease.
        let restore_guard = element_guard.clone();
        let restore_needed = latency::spawn_action_blocking(move || {
            let ptr = restore_guard.as_ptr();
            let role =
                unsafe { copy_string_attr(ptr as AXUIElementRef, "AXRole") }.unwrap_or_default();
            let facts = crate::ax::exact_target::gather_background_facts(pid, window_id, Some(ptr));
            text_value_role_requires_exact_focus(&role)
                && super::type_text::restore_decision(facts.target_minimized, facts.app_hidden)
                    == super::type_text::RestoreDecision::Restore
        })
        .await
        .unwrap_or(false);
        if restore_needed {
            // Return would go to a window minimized again right after the
            // write; refuse before anything is written.
            if autosubmit {
                return super::set_value_autosubmit::minimized_refusal(window_id);
            }
            return minimized_set_value(
                _mutation_lease,
                element_guard,
                element_index,
                pid,
                window_id,
                value,
            )
            .await;
        }

        // TOKEN rule (handoff 3.7): after the gate and the rename routes,
        // attempt focus and refuse a text-role element with no editing
        // evidence instead of writing into it.
        if let Some(refusal) =
            super::not_editable::token_not_editable_refusal(pid, element_ptr, window_id).await
        {
            return refusal;
        }

        let cursor_key = super::cursor_tools::resolve_cursor_key(&args);
        let center_guard = element_guard.clone();
        if let Ok((Some((screen_x, screen_y)), target_rect)) =
            latency::spawn_action_blocking(move || unsafe {
                let el = center_guard.as_ptr() as AXUIElementRef;
                (
                    crate::ax::bindings::element_screen_center(el),
                    crate::ax::bindings::element_screen_rect(el),
                )
            })
            .await
        {
            crate::cursor::overlay::send_command(
                cursor_key.clone(),
                cursor_overlay::OverlayCommand::PinAbove(window_id as u64),
            );
            crate::cursor::overlay::animate_cursor_to_target(
                cursor_key.clone(),
                screen_x,
                screen_y,
                target_rect,
            )
            .await;
            self.state
                .cursor_registry
                .update_position(&cursor_key, screen_x, screen_y);
        }
        // An AXValue read-back is not ground truth for web content. Chromium,
        // WebKit, and Electron can echo the write through accessibility while
        // the renderer never observes it. Reuse type_text's bounded ancestor
        // check so native browser chrome stays trusted but rendered content is
        // always reported as unverified.
        let ax_echo_surface = super::type_text::target_in_web_area(
            pid,
            Some((element_ptr, Some(element_index))),
            Some(window_id),
        );

        // ── Focus-suppression wrap (Swift WindowChangeDetector + FocusGuard) ──
        // AXValue writes on popups / sliders can cause reflex activations
        // in Chromium-based apps; the AXPopUpButton path also AXPresses a
        // child option which can trigger app activation in some setups.
        let prior_front = apps::frontmost_pid();
        let snapshot = WindowChangeDetector::snapshot(prior_front);

        let post_guard = element_guard.clone();
        let result = focus_guard::with_focus_suppressed(
            Some(pid),
            prior_front,
            "set_value.AXValue",
            || async move {
                latency::spawn_action_blocking(move || {
                    set_value_blocking(
                        element_guard.as_ptr(),
                        element_index,
                        pid,
                        window_id,
                        &value,
                    )
                })
                .await
            },
        )
        .await;

        let changes = snapshot.detect_async().await;

        // Read before the result is built: on web content an unchanged
        // read-back is reported as unverifiable, not as not applied.
        let write_not_applied = matches!(&result, Ok(Ok(outcome)) if outcome.not_applied);
        let maybe_landed = matches!(&result, Ok(Ok(outcome)) if outcome.maybe_landed);
        let result = set_value_tool_result(
            result.map_err(|error| error.to_string()),
            ax_echo_surface,
            changes.result_suffix(),
            pid,
            element_index,
        );
        super::set_value_autosubmit::finish(
            autosubmit,
            pid,
            result,
            write_not_applied,
            maybe_landed,
            |_, key| async move {
                super::set_value_autosubmit::post_return(
                    _mutation_lease,
                    window_id,
                    post_guard,
                    key,
                )
                .await
            },
        )
        .await
    }
}

/// The tool result for a finished set_value write.
fn set_value_tool_result(
    result: Result<anyhow::Result<SetValueOutcome>, String>,
    ax_echo_surface: bool,
    suffix: String,
    pid: i32,
    element_index: usize,
) -> ToolResult {
    match result {
        Ok(Ok(outcome)) if outcome.not_applied && !ax_echo_surface => {
            let vcl = crate::input::pointer_toolkit::detect(pid)
                == Some(crate::input::pointer_toolkit::PointerReadingToolkit::Vcl);
            let mut result = not_applied_result(element_index, vcl);
            // The first text block is the published summary.
            if let Some(cua_driver_core::protocol::Content::Text { text, .. }) =
                result.content.first_mut()
            {
                text.push_str(&suffix);
            }
            result
        }
        Ok(Ok(mut outcome)) => {
            apply_surface_trust(&mut outcome, ax_echo_surface);
            apply_verification_label(&mut outcome);
            let mut msg = outcome.detail;
            msg.push_str(&suffix);
            let verified = outcome.verified.unwrap_or(false);
            let mut structured = serde_json::json!({
                "path": "ax",
                "verified": verified,
                "effect": if verified { "confirmed" } else { "unverifiable" },
            });
            if let Some(edited) = outcome.document_edited {
                structured["document_key_edit_sent"] = serde_json::json!(edited);
            }
            if ax_echo_surface {
                structured["escalation"] = serde_json::json!({
                    "recommended": "px",
                    "reason": "AXValue read-back is not trusted for web content. Verify \
                               through the renderer; use browser page tools for a tab or \
                               manipulate the control through its pixel action."
                });
            }
            ToolResult::text(msg).with_structured(structured)
        }
        Ok(Err(e)) => ToolResult::error(format!("set_value failed: {e}.{suffix}")),
        Err(e) => ToolResult::error(format!("Task error: {e}.{suffix}")),
    }
}

// ── A text field in a minimized window (400-F1 wp2) ─────────────────────────

/// Restore the minimized window, apply the TOKEN rule and the write on the
/// visible window, then minimize it again. Everything runs in one task that
/// owns the lease: a cancelled caller cannot release it, or skip the
/// re-minimize, while the write is still in flight.
async fn minimized_set_value(
    lease: super::BackgroundMutationLease,
    element_guard: crate::ax::snapshot::RetainedElement,
    element_index: usize,
    pid: i32,
    window_id: u32,
    value: String,
) -> ToolResult {
    use super::type_text::{AxMinimizeOps, MinimizedRestore, RestoreReport};
    let task = tokio::spawn(async move {
        let _lease = lease;
        let Some(ops) = latency::spawn_action_blocking(move || AxMinimizeOps::of(pid, window_id))
            .await
            .ok()
            .flatten()
        else {
            return super::pre_dispatch_refusal(
                cua_driver_core::background_input::refusal_codes::MINIMIZED_OR_HIDDEN,
                format!("window {window_id} is minimized and could not be resolved to restore it; AXValue was not written."),
            );
        };
        let begun = latency::spawn_action_blocking(move || {
            let mut restore = MinimizedRestore::new(ops);
            let begun = restore.begin(|| std::thread::sleep(std::time::Duration::from_millis(25)));
            (restore, begun)
        })
        .await;
        let (restore, begun) = match begun {
            Ok(pair) => pair,
            Err(error) => {
                return restore_task_failure(format!(
                    "set_value: restoring the minimized window failed ({error}); it may be left \
                     restored. Nothing was written."
                ))
            }
        };
        let finish = |restore: MinimizedRestore<AxMinimizeOps>| async move {
            latency::spawn_action_blocking(move || restore.finish())
                .await
                .unwrap_or(RestoreReport::ReminimizeNotConfirmed(None))
        };
        let with_note =
            |result: ToolResult, report: RestoreReport| attach_restore_cleanup(result, report, "");
        if let Err(reason) = begun {
            let report = finish(restore).await;
            return with_note(
                super::pre_dispatch_refusal(
                    cua_driver_core::background_input::refusal_codes::MINIMIZED_OR_HIDDEN,
                    format!(
                        "window {window_id} is minimized and {reason}; AXValue was not written."
                    ),
                ),
                report,
            );
        }
        // The window is visible now: prove it again as an exact, visible
        // keyboard target, and hold it as the app's key window before anything (the TOKEN rule's focus
        // attempt included) touches it.
        let gate_guard = element_guard.clone();
        let regated = latency::spawn_action_blocking(move || {
            use cua_driver_core::background_input::{
                decide_background_input, BackgroundAction, BackgroundInputDecision,
                ExactWindowTarget,
            };
            let facts = crate::ax::exact_target::gather_background_facts(
                pid,
                window_id,
                Some(gate_guard.as_ptr()),
            );
            let refusal = match decide_background_input(
                ExactWindowTarget { pid, window_id },
                &facts,
                BackgroundAction::ExactWindowKeysFocused,
            ) {
                BackgroundInputDecision::Refuse(refusal) => Some((refusal.code, refusal.reason)),
                // The gate accepts any readable visibility for focused keys;
                // a restored window must read visible in a visible app.
                BackgroundInputDecision::Execute { .. }
                    if !(facts.target_minimized == Some(false)
                        && facts.app_hidden == Some(false)) =>
                {
                    Some((
                        cua_driver_core::background_input::refusal_codes::MINIMIZED_OR_HIDDEN,
                        "it does not read as visible in a visible app".to_owned(),
                    ))
                }
                BackgroundInputDecision::Execute { .. } => None,
            };
            if let Some(refusal) = refusal {
                return (restore, Err(refusal));
            }
            let scope = crate::input::skylight::enter_exact_window_key_inline(
                pid as libc::pid_t,
                window_id,
            )
            .map_err(|reason| (super::WINDOW_NOT_KEY, reason));
            (restore, scope)
        })
        .await;
        let (restore, scope) = match regated {
            Ok((restore, Ok(scope))) => (restore, scope),
            Ok((restore, Err((code, reason)))) => {
                let report = finish(restore).await;
                return with_note(
                    super::pre_dispatch_refusal(
                        code,
                        format!(
                            "window {window_id} was restored, but {reason}; AXValue was not written."
                        ),
                    ),
                    report,
                );
            }
            Err(error) => {
                return restore_task_failure(format!(
                    "set_value: re-checking the restored window failed ({error}); it may be left \
                     restored. Nothing was written."
                ))
            }
        };
        let element_ptr = element_guard.as_ptr();
        // Focus the field here, then judge its editability read-only (this
        // route does not call the TOKEN helper, which may write focus again).
        let focus_guard = element_guard.clone();
        let focused = latency::spawn_action_blocking(move || {
            let ptr = focus_guard.as_ptr();
            let already =
                unsafe { crate::ax::bindings::copy_bool_attr(ptr as AXUIElementRef, "AXFocused") }
                    == Some(true);
            if !already {
                let _ = crate::input::ax_actions::focus_element(ptr);
            }
            let holds_focus =
                unsafe { crate::ax::bindings::copy_bool_attr(ptr as AXUIElementRef, "AXFocused") }
                    == Some(true)
                    && crate::input::ax_actions::is_element_focused(pid, ptr);
            if !holds_focus {
                return RestoredFocus::NotFocused {
                    focus_written: !already,
                };
            }
            if unsafe { super::not_editable::sync_focused_cell_allows(pid, Some(window_id)) } {
                RestoredFocus::Editable
            } else {
                RestoredFocus::NotEditable {
                    focus_written: !already,
                }
            }
        })
        .await
        .unwrap_or(RestoredFocus::NotFocused {
            focus_written: true,
        });
        let token_refusal = match focused {
            RestoredFocus::Editable => None,
            // A focus write was sent: the receipts say so.
            RestoredFocus::NotFocused { focus_written } => {
                Some(super::not_editable::not_editable_result(
                    format!(
                    "the field in restored window {window_id} did not read back as focused after \
                     the focus attempt; AXValue was not written."
                ),
                    if focus_written {
                        super::not_editable::FocusDispatch::Indeterminate
                    } else {
                        super::not_editable::FocusDispatch::None
                    },
                ))
            }
            // focus_element hides AX errors: a write this route made is
            // possibly delivered, never proven.
            RestoredFocus::NotEditable { focus_written } => {
                Some(super::not_editable::not_editable_result(
                    "the target element is not being edited; AXValue was not written.".to_owned(),
                    if focus_written {
                        super::not_editable::FocusDispatch::Indeterminate
                    } else {
                        super::not_editable::FocusDispatch::None
                    },
                ))
            }
        };
        if let Some(refusal) = token_refusal {
            let entry = scope.entry();
            let hand_back = latency::spawn_action_blocking(move || scope.finish())
                .await
                .unwrap_or(crate::input::skylight::HandBack::NotConfirmed);
            let report = finish(restore).await;
            let disclosure = crate::input::skylight::scope_disclosure(entry, Some(hand_back));
            return attach_restore_cleanup(refusal, report, &disclosure);
        }
        let ax_echo_surface = super::type_text::target_in_web_area(
            pid,
            Some((element_ptr, Some(element_index))),
            Some(window_id),
        );
        let write_guard = element_guard.clone();
        let written = latency::spawn_action_blocking(move || {
            // The scope moved in after the restore guard, so a panic hands
            // key back before the window is minimized again.
            let scope = scope;
            let result = set_value_blocking_with(
                write_guard.as_ptr(),
                element_index,
                pid,
                window_id,
                &value,
                true,
            );
            let entry = scope.entry();
            let hand_back = scope.finish();
            let disclosure = crate::input::skylight::scope_disclosure(entry, Some(hand_back));
            (restore, result, disclosure)
        })
        .await;
        let (restore, result, disclosure) = match written {
            Ok(pair) => pair,
            Err(error) => {
                return restore_task_failure(format!(
                    "set_value: the write task failed ({error}); it may have written, and the \
                     window may be left restored. Read the field before repeating it."
                ))
            }
        };
        let report = finish(restore).await;
        let suffix = [disclosure.clone(), report.describe()]
            .into_iter()
            .filter(|note| !note.is_empty())
            .map(|note| format!(" {note}"))
            .collect::<String>();
        let result = set_value_tool_result(Ok(result), ax_echo_surface, suffix, pid, element_index);
        attach_restore_cleanup(result, report, &disclosure)
    });
    task.await.unwrap_or_else(|error| {
        restore_task_failure(format!(
            "set_value: the minimized-window task failed ({error}); it may have written, and the \
             window may be left restored. Read the field before repeating it."
        ))
    })
}

/// A restored-window task that failed: the window may be left restored and
/// the write may have happened; the whole text is structured too.
fn restore_task_failure(message: String) -> ToolResult {
    ToolResult::error(message.clone()).with_structured(serde_json::json!({
        "effect": "unverifiable",
        "restore_cleanup": "unknown",
        "message": message,
    }))
}

/// Every result of a temporary restore says what became of the window: in
/// text (once) and as structured `restore_cleanup`.
fn attach_restore_cleanup(
    mut result: ToolResult,
    report: super::type_text::RestoreReport,
    disclosure: &str,
) -> ToolResult {
    let note = [disclosure.to_owned(), report.describe()]
        .into_iter()
        .filter(|note| !note.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let already = result.content.iter().any(|content| {
        matches!(content, cua_driver_core::protocol::Content::Text { text, .. } if text.contains(&report.describe()))
    });
    if !note.is_empty() && !already {
        // The first text block is the published summary.
        match result.content.iter_mut().find_map(|content| match content {
            cua_driver_core::protocol::Content::Text { text, .. } => Some(text),
            _ => None,
        }) {
            Some(text) => {
                text.push(' ');
                text.push_str(&note);
            }
            None => result
                .content
                .push(cua_driver_core::protocol::Content::text(note.clone())),
        }
    }
    if !matches!(
        result.structured_content,
        Some(serde_json::Value::Object(_))
    ) {
        result.structured_content = Some(serde_json::json!({}));
    }
    if let Some(serde_json::Value::Object(fields)) = result.structured_content.as_mut() {
        fields.insert("restore_cleanup".into(), serde_json::json!(report.code()));
        // Error results are not projected into a summary: carry the
        // disclosure as a structured field too.
        if !note.is_empty() {
            fields.insert("restore_note".into(), serde_json::json!(note));
        }
    }
    carry_message(result)
}

/// Error results are not projected into a summary: put their whole text
/// (the cause, partial-write warnings, scope disclosure) in a structured
/// `message` when none is there yet.
fn carry_message(mut result: ToolResult) -> ToolResult {
    let text: Vec<&str> = result
        .content
        .iter()
        .filter_map(|content| match content {
            cua_driver_core::protocol::Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let text = text.join(" ");
    if let Some(serde_json::Value::Object(fields)) = result.structured_content.as_mut() {
        if !text.is_empty() && !fields.contains_key("message") {
            fields.insert("message".into(), serde_json::json!(text));
        }
    }
    result
}

/// Whether the element's window is a document (has an AXDocument).
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef`.
/// `false` only when the window reads back without one; unreadable counts
/// as a document (the save warning is the safe report).
unsafe fn window_has_document(element: AXUIElementRef) -> bool {
    let Some(window) = crate::ax::bindings::copy_element_attr(element, "AXWindow") else {
        return true;
    };
    let attr = core_foundation::string::CFString::new("AXDocument");
    let mut value: core_foundation::base::CFTypeRef = std::ptr::null();
    let err = crate::ax::bindings::AXUIElementCopyAttributeValue(
        window,
        core_foundation::base::TCFType::as_concrete_TypeRef(&attr),
        &mut value,
    );
    if !value.is_null() {
        CFRelease(value);
    }
    CFRelease(window as _);
    // kAXErrorNoValue / kAXErrorAttributeUnsupported: read back without one.
    !matches!(err, -25212 | -25205)
}

/// The restored-window route's own focus step.
enum RestoredFocus {
    Editable,
    NotFocused { focus_written: bool },
    NotEditable { focus_written: bool },
}

// ── File name cells ──────────────────────────────────────────────────────────

const FILE_NAME_NEEDS_RENAME: &str = "file_name_needs_rename";

/// Whether `element` is a file's name as a list shows it (Finder's list and
/// icon views): a text field that names a file and is not being edited.
/// Finder's inline rename editor is focused, so it stays writable.
pub(super) unsafe fn file_name_cell(element: AXUIElementRef) -> bool {
    copy_string_attr(element, "AXRole").as_deref() == Some("AXTextField")
        && is_file_name_cell(
            copy_string_attr(element, "AXFilename").as_deref(),
            copy_url_attr(element).as_deref(),
            copy_bool_attr(element, "AXFocused"),
        )
}

fn is_file_name_cell(filename: Option<&str>, url: Option<&str>, focused: Option<bool>) -> bool {
    filename.is_some_and(|name| !name.is_empty())
        && url.is_some_and(|url| url.starts_with("file://"))
        && focused != Some(true)
}

/// Whether `element` is the Name & Extension field of Finder's Get Info
/// window. It names no file through AXFilename or AXURL, so `file_name_cell`
/// misses it, yet an AXValue write there renames nothing either.
unsafe fn get_info_name_field(pid: i32, element: AXUIElementRef) -> bool {
    is_get_info_name_field(
        crate::apps::bundle_id_for_pid(pid).as_deref(),
        copy_string_attr(element, "AXRole").as_deref(),
        copy_string_attr(element, "AXIdentifier").as_deref(),
        copy_bool_attr(element, "AXFocused"),
    )
}

fn is_get_info_name_field(
    bundle_id: Option<&str>,
    role: Option<&str>,
    identifier: Option<&str>,
    focused: Option<bool>,
) -> bool {
    bundle_id == Some("com.apple.finder")
        && role == Some("AXTextField")
        && identifier == Some("Name")
        && focused != Some(true)
}

/// What a write to a file-name element turns into.
enum NameTarget {
    /// A Finder item at this path: rename it through Finder.
    FinderItem(std::path::PathBuf),
    /// Refuse with this route.
    Refuse(&'static str),
}

/// A rename read back from disk: the new name is listed, the old one gone.
fn rename_record() -> cua_driver_core::action_record::ActionExecutionRecord {
    use cua_driver_core::action_record::{
        ActionEffect, ActionEvidence, ActionExecutionRecord, ActionTransport, ActualDelivery,
        EvidenceKind, RequestedDelivery,
    };
    ActionExecutionRecord::builder(
        ActionEffect::Confirmed,
        ActionTransport::MacosAppleEvent,
        RequestedDelivery::Background,
    )
    .actual_delivery(ActualDelivery::Background)
    .evidence(ActionEvidence {
        kind: EvidenceKind::ValueReadback,
        detail: "The folder on disk lists the new name and not the old one".into(),
    })
    .build()
    .expect("rename record is valid")
}

fn finder_rename_result(
    pid: i32,
    window_id: u32,
    path: &std::path::Path,
    new_name: &str,
    renamed: Result<
        Result<std::path::PathBuf, crate::apple_events::ScriptRefusal>,
        tokio::task::JoinError,
    >,
) -> ToolResult {
    let old_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    match renamed {
        Ok(Ok(new_path)) => ToolResult::text(format!(
            "✅ Renamed \"{old_name}\" to \"{new_name}\" through Finder's scripting interface, \
             with Finder left in the background (not activated). Verified on disk: {} exists \
             and the old name is gone. Take a fresh get_window_state before addressing the \
             item again.",
            new_path.display()
        ))
        .with_structured(serde_json::json!({
            "path": "apple_events",
            "verified": true,
            "effect": "confirmed",
            "renamed_from": path.display().to_string(),
            "renamed_to": new_path.display().to_string(),
        }))
        .with_action_record(rename_record()),
        Ok(Err(refusal)) => ToolResult::error(format!(
            "set_value refused ({FILE_NAME_NEEDS_RENAME}): renaming \"{old_name}\" through \
             Finder did not complete ({}: {}). The file keeps its name; nothing else was sent.",
            refusal.code, refusal.reason
        ))
        .with_structured(serde_json::json!({
            "code": FILE_NAME_NEEDS_RENAME,
            "effect": "refused",
            "path": "apple_events",
            "pid": pid,
            "window_id": window_id,
            "reason": refusal.reason,
            "cause": refusal.code,
        })),
        Err(error) => ToolResult::error(format!("set_value rename task failed: {error}")),
    }
}

const LIST_RENAME_ROUTE: &str = "This is a file's name as the list shows it. Writing its AXValue \
    changes only what the list shows, never the file, so nothing was written. To rename the \
    file: click this element to select the item, then with Finder frontmost send press_key \
    return, hotkey cmd+a (Finder selects the name without its extension), type_text the full \
    new name, and press_key return, each with scope:\"desktop\". Then check the new name in a \
    fresh get_window_state.";

const GET_INFO_RENAME_ROUTE: &str = "This is the Name & Extension field of Finder's Get Info \
    window. Writing its AXValue changes only what the field shows, never the file, so nothing \
    was written. To rename the file in the background, call set_value with the new name on the \
    file's name cell in a Finder window that lists it. Otherwise take a fresh get_window_state \
    of this window and click the field's centre in its screenshot pixels (pass its capture_id), then hotkey cmd+a, type_text \
    the full new name with its extension, and press_key return, each with \
    delivery_mode:\"foreground\" on this window. A changed extension makes Finder ask for \
    confirmation in a dialog first. Then check the new name in a fresh listing of the folder; \
    this window's title changes with it.";

const ADDRESS_FIELD_NOT_SETTABLE: &str = "address_field_not_settable";

const ADDRESS_FIELD_ROUTE: &str = "This is Safari's address and search field. Safari takes an \
    AXValue write and reads it back, but the field keeps showing the page's address and nothing \
    is opened, so nothing was written. To open an address with Safari left in the background: \
    invoke_menu [\"File\", \"Open Location...\"] on this window (the field starts editing), \
    then type_text the address into the field's element_token from a fresh get_window_state, \
    then press_key return on this window. Check the window title or the field in a fresh \
    get_window_state. To read or act on the page itself use the page tool.";

/// Safari's address and search field: an `AXValue` write is read back while
/// the field shows and keeps the page's address, and it never navigates.
fn is_safari_address_field(bundle_id: Option<&str>, identifier: Option<&str>) -> bool {
    bundle_id.is_some_and(|bundle| bundle.starts_with("com.apple.Safari"))
        && identifier == Some("WEB_BROWSER_ADDRESS_AND_SEARCH_FIELD")
}

fn address_field_refusal(pid: i32, window_id: u32) -> ToolResult {
    ToolResult::error(format!(
        "set_value refused ({ADDRESS_FIELD_NOT_SETTABLE}): {ADDRESS_FIELD_ROUTE}"
    ))
    .with_structured(serde_json::json!({
        "code": ADDRESS_FIELD_NOT_SETTABLE,
        "effect": "refused",
        "path": "ax",
        "pid": pid,
        "window_id": window_id,
        "reason": ADDRESS_FIELD_ROUTE,
    }))
}

fn file_name_needs_rename(pid: i32, window_id: u32, reason: &str) -> ToolResult {
    ToolResult::error(format!(
        "set_value refused ({FILE_NAME_NEEDS_RENAME}): {reason}"
    ))
    .with_structured(serde_json::json!({
        "code": FILE_NAME_NEEDS_RENAME,
        "effect": "refused",
        "path": "ax",
        "pid": pid,
        "window_id": window_id,
        "reason": reason,
    }))
}

// ── Blocking implementation (runs on spawn_blocking thread) ─────────────────

/// Outcome of a `set_value` write.
///
/// `verified` is `None` for paths that do not perform a value read-back (the
/// AXPopUpButton path drives menu items rather than writing AXValue), and
/// `Some(false)` when a read-back ran but could not confirm the write. A
/// successful `AXUIElementSetAttributeValue` return code is not by itself
/// evidence that the value landed: web content behind an AXWebArea accepts the
/// write and echoes it back through AXValue while the renderer never observes
/// it — the same trap `type_text` already documents.
struct SetValueOutcome {
    detail: String,
    verified: Option<bool>,
    /// `Some(false)` when the element already held the requested value, so the
    /// write was a no-op. Lets callers distinguish "idempotent" from "applied".
    changed: Option<bool>,
    /// The read-back still showed the old value after a short settle: the app
    /// took the accessibility write but did not apply it.
    not_applied: bool,
    /// The document's edited mark after the write; `None` outside a
    /// document or when the value did not move.
    document_edited: Option<bool>,
    maybe_landed: bool,
}

fn apply_surface_trust(outcome: &mut SetValueOutcome, ax_echo_surface: bool) {
    if ax_echo_surface && outcome.verified == Some(true) {
        outcome.verified = Some(false);
        outcome.changed = None;
        outcome.detail.push_str(
            " AXValue read-back is not trusted for web content; verify the \
             renderer via screenshot or use the browser page tools.",
        );
    }
}

fn apply_verification_label(outcome: &mut SetValueOutcome) {
    if outcome.verified != Some(true) {
        if let Some(rest) = outcome.detail.strip_prefix("✅ Set") {
            outcome.detail = format!("📨 Sent (unverified){rest}");
        }
    }
}

fn set_value_blocking(
    element_ptr: usize,
    element_index: usize,
    pid: i32,
    window_id: u32,
    value: &str,
) -> anyhow::Result<SetValueOutcome> {
    set_value_blocking_with(element_ptr, element_index, pid, window_id, value, false)
}

/// `restored_window`: the window was restored from the Dock for this write.
/// Its key scope then hands key back on this thread before returning (the
/// caller re-minimizes next).
fn set_value_blocking_with(
    element_ptr: usize,
    element_index: usize,
    pid: i32,
    window_id: u32,
    value: &str,
    restored_window: bool,
) -> anyhow::Result<SetValueOutcome> {
    let element = element_ptr as AXUIElementRef;

    let role = unsafe { copy_string_attr(element, "AXRole") }.unwrap_or_default();

    if role == "AXPopUpButton" {
        let element_title = unsafe { copy_string_attr(element, "AXTitle") }.unwrap_or_default();
        // Menu-item selection, not an AXValue write — no read-back to report.
        select_popup_option(
            element,
            element_index,
            pid,
            window_id,
            value,
            &element_title,
        )
        .map(|detail| SetValueOutcome {
            detail,
            verified: None,
            changed: None,
            not_applied: false,
            document_edited: None,
            maybe_landed: false,
        })
    } else {
        // A restored window's caller already holds an inline key scope for
        // the whole write and reports its hand-back.
        let _key_scope = if text_value_role_requires_exact_focus(&role) && restored_window {
            None
        } else if text_value_role_requires_exact_focus(&role) {
            Some(
                super::key_scope(pid, Some(window_id))
                    .map_err(|reason| anyhow::anyhow!(reason))?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "set_value requires exact-window key priming before an editable text AXValue write, but that transport is unavailable"
                        )
                    })?,
            )
        } else {
            None
        };
        if text_value_role_requires_exact_focus(&role) {
            // A restored window needs no focus write when the field already
            // holds focus.
            let needs_focus =
                !restored_window || unsafe { copy_bool_attr(element, "AXFocused") } != Some(true);
            if needs_focus {
                crate::input::ax_actions::focus_element(element_ptr)?;
            }
            let own_focus = unsafe { copy_bool_attr(element, "AXFocused") } == Some(true);
            let exact_window =
                super::type_text::element_in_requested_window(pid, window_id, element_ptr);
            let app_focus = crate::input::ax_actions::is_element_focused(pid, element_ptr);
            let key_window =
                super::type_text::focused_window_matches_element(pid, window_id, element_ptr);
            let focused = own_focus && exact_window && app_focus && key_window;
            if !focused {
                anyhow::bail!(
                    "set_value target did not become the application's focused text element in the requested window (AXFocused={own_focus}, exact_window={exact_window}, app_focus={app_focus}, key_window={key_window}); AXValue was not written"
                )
            }
        }
        let secure = unsafe { copy_string_attr(element, "AXSubrole") }.as_deref()
            == Some("AXSecureTextField");
        let actions = unsafe { copy_action_names(element) };
        let ctx = LadderContext {
            secure,
            can_step: actions
                .iter()
                .any(|a| a == "AXIncrement" || a == "AXDecrement"),
        };
        let mut ops = LiveAxValueOps { element };
        let outcome = write_value_ladder(&mut ops, value, &ctx);
        finish_ladder(outcome, element, pid, element_index, &role, restored_window)
    }
}

trait AxValueOps {
    fn read_typed(&mut self) -> ValueObs;
    fn query_settable(&mut self) -> Settable;
    fn set_number(&mut self, value: f64) -> i32;
    fn set_string(&mut self, value: &str) -> i32;
    fn perform_step(&mut self, action: &str) -> i32;
    fn settle_pause(&mut self);
}

struct LiveAxValueOps {
    element: AXUIElementRef,
}
impl AxValueOps for LiveAxValueOps {
    fn read_typed(&mut self) -> ValueObs {
        unsafe { read_value_typed(self.element) }
    }
    fn query_settable(&mut self) -> Settable {
        unsafe { query_settable(self.element, "AXValue") }
    }
    fn set_number(&mut self, value: f64) -> i32 {
        unsafe { set_number_attr(self.element, "AXValue", value) }
    }
    fn set_string(&mut self, value: &str) -> i32 {
        unsafe { set_string_attr(self.element, "AXValue", value) }
    }
    fn perform_step(&mut self, action: &str) -> i32 {
        unsafe { perform_action(self.element, action) }
    }
    fn settle_pause(&mut self) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

struct LadderContext {
    secure: bool,
    can_step: bool,
}
#[derive(Debug)]
struct LadderOutcome {
    verified: Option<bool>,
    changed: Option<bool>,
    not_applied: bool,
    maybe_landed: bool,
    steps_sent: u32,
    steps_maybe_landed: u32,
    stepped: bool,
    error: Option<i32>,
    reached: bool,
}
impl LadderOutcome {
    fn new() -> Self {
        Self {
            verified: None,
            changed: None,
            not_applied: false,
            maybe_landed: false,
            steps_sent: 0,
            steps_maybe_landed: 0,
            stepped: false,
            error: None,
            reached: false,
        }
    }
}
fn obs_string(obs: &ValueObs) -> Option<String> {
    match obs {
        ValueObs::Number(n) => Some(n.to_string()),
        ValueObs::Text(s) => Some(s.clone()),
        _ => None,
    }
}
fn write_value_ladder(
    ops: &mut impl AxValueOps,
    value: &str,
    ctx: &LadderContext,
) -> LadderOutcome {
    let mut out = LadderOutcome::new();
    let target = value.trim().parse::<f64>().ok();
    let before = ops.read_typed();
    let settable = ops.query_settable();
    if let (Settable::No, ValueObs::Number(mut current), Some(target), true) =
        (settable, &before, target, ctx.can_step)
    {
        out.stepped = true;
        let mut radius = f64::EPSILON;
        for _ in 0..500 {
            if (current - target).abs() <= radius {
                out.reached = true;
                break;
            }
            let action = if current < target {
                "AXIncrement"
            } else {
                "AXDecrement"
            };
            out.steps_sent += 1;
            let err = ops.perform_step(action);
            match classify_attr_write(err) {
                AttrWrite::Applied => out.steps_maybe_landed += 1,
                AttrWrite::Rejected => {
                    out.error = Some(err);
                    break;
                }
                AttrWrite::Uncertain => {
                    out.steps_maybe_landed += 1;
                    out.maybe_landed = true;
                    out.error = Some(err);
                    break;
                }
            }
            let ValueObs::Number(next) = ops.read_typed() else {
                break;
            };
            if next == current {
                break;
            }
            radius = (next - current).abs() / 2.0;
            current = next;
        }
        if !out.maybe_landed && out.error.is_none() && (current - target).abs() <= radius {
            out.reached = true;
        }
        if out.reached && !ctx.secure {
            let after = ops.read_typed();
            let before_s = obs_string(&before);
            let after_s = obs_string(&after);
            (out.verified, out.changed) =
                classify_write(before_s.as_deref(), after_s.as_deref(), value, true);
        }
        return out;
    }
    let first_number = target.is_some() && !matches!(before, ValueObs::Text(_));
    let mut number = first_number;
    let err = loop {
        let err = if number {
            ops.set_number(target.expect("numeric representation"))
        } else {
            ops.set_string(value)
        };
        match classify_attr_write(err) {
            AttrWrite::Applied => break err,
            AttrWrite::Uncertain => {
                out.maybe_landed = true;
                break err;
            }
            AttrWrite::Rejected => {
                if number {
                    number = false;
                } else if first_number || target.is_none() {
                    out.error = Some(err);
                    return out;
                } else {
                    number = true;
                }
                // Only one alternate representation may be attempted.
                let alternate = if number {
                    ops.set_number(target.expect("numeric alternate"))
                } else {
                    ops.set_string(value)
                };
                match classify_attr_write(alternate) {
                    AttrWrite::Applied => break alternate,
                    AttrWrite::Rejected => {
                        out.error = Some(alternate);
                        return out;
                    }
                    AttrWrite::Uncertain => {
                        out.maybe_landed = true;
                        break alternate;
                    }
                }
            }
        }
    };
    out.error = (err != kAXErrorSuccess).then_some(err);
    if !ctx.secure {
        let before_s = obs_string(&before);
        let numeric = !matches!(before, ValueObs::Text(_)) && target.is_some();
        let after = ops.read_typed();
        let after_s = obs_string(&after);
        (out.verified, out.changed) =
            classify_write(before_s.as_deref(), after_s.as_deref(), value, numeric);
        for _ in 0..NOT_APPLIED_REREADS {
            if !write_not_applied(out.verified, out.changed) {
                break;
            }
            ops.settle_pause();
            let after = ops.read_typed();
            let after_s = obs_string(&after);
            (out.verified, out.changed) =
                classify_write(before_s.as_deref(), after_s.as_deref(), value, numeric);
        }
        // A numeric control may clamp to the original value; this cannot prove
        // that an accepted write had no effect.
        out.not_applied =
            !out.maybe_landed && !numeric && write_not_applied(out.verified, out.changed);
    }
    out.reached = true;
    out
}

fn finish_ladder(
    out: LadderOutcome,
    element: AXUIElementRef,
    pid: i32,
    element_index: usize,
    role: &str,
    restored_window: bool,
) -> anyhow::Result<SetValueOutcome> {
    if !out.reached {
        let err = out.error.unwrap_or(kAXErrorSuccess);
        if out.maybe_landed || out.steps_sent > 0 {
            anyhow::bail!("AX value operation failed with error {err}; {} step action(s) sent ({} may have landed); the value may have changed. Read it before repeating", out.steps_sent, out.steps_maybe_landed)
        }
        anyhow::bail!("AXUIElementSetAttributeValue(AXValue) failed with error {err}");
    }
    if out.stepped {
        return Ok(SetValueOutcome {
            detail: format!(
                "✅ Set AXValue on [{element_index}] {role} via AXIncrement/AXDecrement stepping."
            ),
            verified: out.verified,
            changed: out.changed,
            not_applied: false,
            document_edited: None,
            maybe_landed: out.maybe_landed,
        });
    }
    let suffix = match (out.verified, out.changed) {
        (Some(true), Some(false)) => " Value already matched; write was idempotent.",
        (Some(true), _) => "",
        (Some(false), _) => " Read-back did not confirm the value; verify via screenshot.",
        (None, _) => " Value is not readable through AX; could not confirm.",
    };
    let document = document_after_ladder(
        &out,
        restored_window,
        || unsafe { crate::document_edit::ensure_marked_after_ax_write(pid, element, true) },
        || unsafe { window_has_document(element) },
    );
    let warning = if out.maybe_landed {
        " The value may have changed; read it before deciding what to do next."
    } else {
        ""
    };
    Ok(SetValueOutcome {
        detail: format!(
            "✅ Set AXValue on [{element_index}] {role}.{suffix}{}{warning}",
            document.note()
        ),
        verified: out.verified,
        changed: out.changed,
        not_applied: out.not_applied,
        document_edited: document.structured(),
        maybe_landed: out.maybe_landed,
    })
}

fn document_after_ladder(
    out: &LadderOutcome,
    restored_window: bool,
    mark: impl FnOnce() -> crate::document_edit::DocumentEdited,
    has_document: impl FnOnce() -> bool,
) -> crate::document_edit::DocumentEdited {
    if !out.maybe_landed && out.changed == Some(true) && !restored_window {
        mark()
    } else if restored_window && out.changed != Some(false) && has_document() {
        crate::document_edit::DocumentEdited::NotMarked
    } else {
        crate::document_edit::DocumentEdited::NotADocument
    }
}

fn text_value_role_requires_exact_focus(role: &str) -> bool {
    matches!(
        role,
        "AXTextField" | "AXTextArea" | "AXSearchField" | "AXSecureTextField" | "AXComboBox"
    )
}

/// Re-reads (100 ms apart) before an unchanged value counts as not applied.
const NOT_APPLIED_REREADS: usize = 3;

/// The read-back did not match and the value did not move: the app accepted
/// the accessibility write (LibreOffice Calc cells do) but did not apply it.
fn write_not_applied(verified: Option<bool>, changed: Option<bool>) -> bool {
    verified == Some(false) && changed == Some(false)
}

/// The refusal for a write the app did not apply. Nothing changed, so a
/// script must stop here instead of carrying on as if it had.
fn not_applied_result(element_index: usize, vcl: bool) -> ToolResult {
    let route = if vcl {
        "LibreOffice ignores accessibility value writes and drops background pointer \
         input, so its cells cannot be edited in the background. Editing them needs the \
         window in front: bring_to_front if the user allows it, then type_text into the \
         cell. Otherwise stop and report that the edit is blocked."
    } else {
        "Try the control's own input route instead: click it, then type_text, or use a \
         keyboard shortcut. If that needs the window in front, bring_to_front only if the \
         user allows it."
    };
    ToolResult::error(format!(
        "set_value did not take: element [{element_index}] still shows its old value after \
         the write. The app accepted the accessibility write but did not apply it, so nothing \
         changed. {route}"
    ))
    .with_structured(serde_json::json!({
        "code": "write_not_applied",
        "path": "ax",
        "verified": false,
        "effect": "not_applied",
    }))
}

/// Decide what a post-write AXValue read proves.
///
/// Returns `(verified, changed)`:
/// - `verified = None` when AXValue is not readable at all, so the write can be
///   neither confirmed nor denied.
/// - `verified = Some(true)` when the read-back equals the requested value.
///   Numeric controls are compared numerically so `"25"` matches a slider that
///   reports `"25.0"`.
/// - `changed = Some(false)` when the read-back equals what was there before,
///   i.e. the element's value did not move. Combined with `verified` this
///   separates "already had the requested value" (verified + unchanged) from
///   "the write did not take" (unverified + unchanged).
fn classify_write(
    before: Option<&str>,
    after: Option<&str>,
    requested: &str,
    numeric: bool,
) -> (Option<bool>, Option<bool>) {
    let Some(after) = after else {
        return (None, None);
    };
    let matches = |observed: &str, expected: &str| -> bool {
        if observed == expected {
            return true;
        }
        if !numeric {
            return false;
        }
        match (
            observed.trim().parse::<f64>(),
            expected.trim().parse::<f64>(),
        ) {
            (Ok(a), Ok(b)) => {
                let scale = a.abs().max(b.abs()).max(1.0);
                (a - b).abs() <= 1e-9 * scale
            }
            _ => false,
        }
    };
    let verified = matches(after, requested);
    let changed = before.map(|before| !matches(after, before));
    (Some(verified), changed)
}

// ── AXPopUpButton path ───────────────────────────────────────────────────────

/// How long to wait for a popup's menu to publish its items after AXPress.
const POPUP_OPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2500);
/// Consecutive unchanged polls that mean the menu has finished filling in.
const POPUP_STABLE_POLLS: u32 = 3;
/// How long a dismissed menu may take to close before Escape is sent.
const POPUP_CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);
const POPUP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
/// AX messaging timeout while opening the menu. The target app enters its
/// menu-tracking loop inside the AXPress, so the call would otherwise block
/// for the full default timeout (~1.5 s) with the menu already open.
const POPUP_PRESS_TIMEOUT_SECONDS: f32 = 0.5;

/// One selectable entry of a popup: the retained AX element plus the title
/// and value it reports.
struct PopupOption {
    element: AXUIElementRef,
    title: String,
    value: String,
}

/// Release every retained option.
fn release_options(options: &[PopupOption]) {
    for option in options {
        unsafe { CFRelease(option.element as _) };
    }
}

/// The popup's options as AX exposes them: its direct children, or, when a
/// child is an `AXMenu` (AppKit `NSPopUpButton`, Chromium `<select>`), that
/// menu's `AXMenuItem` children. Menus without items yield nothing, so an
/// unopened popup reads as empty instead of as one untitled option.
fn popup_options(popup: AXUIElementRef) -> Vec<PopupOption> {
    let mut options = Vec::new();
    for child in unsafe { copy_children(popup) } {
        let role = unsafe { copy_string_attr(child, "AXRole") }.unwrap_or_default();
        if role == "AXMenu" {
            for item in unsafe { copy_children(child) } {
                options.push(describe_option(item));
            }
            unsafe { CFRelease(child as _) };
        } else {
            options.push(describe_option(child));
        }
    }
    options.retain(|option| !(option.title.is_empty() && option.value.is_empty()));
    options
}

fn describe_option(element: AXUIElementRef) -> PopupOption {
    PopupOption {
        element,
        title: unsafe { copy_string_attr(element, "AXTitle") }.unwrap_or_default(),
        value: unsafe { copy_string_attr(element, "AXValue") }.unwrap_or_default(),
    }
}

/// Index of the option whose title or value equals `value`, ignoring case and
/// surrounding whitespace.
fn matching_option(options: &[(String, String)], value: &str) -> Option<usize> {
    let wanted = value.trim().to_lowercase();
    options.iter().position(|(title, option_value)| {
        title.trim().to_lowercase() == wanted || option_value.trim().to_lowercase() == wanted
    })
}

fn option_pairs(options: &[PopupOption]) -> Vec<(String, String)> {
    options
        .iter()
        .map(|option| (option.title.clone(), option.value.clone()))
        .collect()
}

fn describe_available(options: &[PopupOption]) -> String {
    options
        .iter()
        .map(|option| {
            let label = if option.title.is_empty() {
                &option.value
            } else {
                &option.title
            };
            format!("\"{label}\"")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether the popup's menu is open right now: an `AXMenu` child that already
/// lists items. A closed AppKit popup has an empty `AXMenu` (or none), and a
/// closed Chromium popup lists only its selected item directly.
fn popup_menu_is_open(popup: AXUIElementRef) -> bool {
    let mut open = false;
    for child in unsafe { copy_children(popup) } {
        if unsafe { copy_string_attr(child, "AXRole") }.as_deref() == Some("AXMenu") {
            let items = unsafe { copy_children(child) };
            open |= !items.is_empty();
            for item in items {
                unsafe { CFRelease(item as _) };
            }
        }
        unsafe { CFRelease(child as _) };
    }
    open
}

/// Close a menu this call opened, without choosing anything. `AXCancel` is
/// enough for AppKit; Chromium's menu ignores it, so fall back to Escape sent
/// to the app (the open menu is what receives it).
fn dismiss_popup_menu(popup: AXUIElementRef, pid: i32) {
    for child in unsafe { copy_children(popup) } {
        let role = unsafe { copy_string_attr(child, "AXRole") }.unwrap_or_default();
        if role == "AXMenu" {
            let _ = unsafe { perform_action(child, "AXCancel") };
        }
        unsafe { CFRelease(child as _) };
    }
    let deadline = std::time::Instant::now() + POPUP_CLOSE_TIMEOUT;
    while popup_menu_is_open(popup) {
        if std::time::Instant::now() >= deadline {
            let _ = crate::input::keyboard::press_key_no_auth(pid, "escape", &[]);
            std::thread::sleep(POPUP_CLOSE_TIMEOUT);
            return;
        }
        std::thread::sleep(POPUP_POLL_INTERVAL);
    }
}

/// Whether a popup belongs to a Chromium renderer (Chrome-family browsers and
/// Electron apps), whose `<select>` menu is a native popup.
fn is_chromium_popup_owner(pid: i32) -> bool {
    let bundle_id = crate::apps::bundle_id_for_pid(pid).unwrap_or_default();
    super::type_text::is_chromium_identity(&bundle_id)
        || crate::browser::electron_js::ElectronJs::is_electron(pid)
}

/// Whether pressing `element` would open a Chromium or Electron `<select>`
/// menu: an `AXPopUpButton` inside web content of a Chromium renderer. The
/// browser's own toolbar popups have no web-area ancestor and do not match.
pub(super) fn is_chromium_web_select(pid: i32, window_id: u32, element: AXUIElementRef) -> bool {
    unsafe { copy_string_attr(element, "AXRole") }.as_deref() == Some("AXPopUpButton")
        && is_chromium_popup_owner(pid)
        && super::type_text::target_in_web_area(
            pid,
            Some((element as usize, None)),
            Some(window_id),
        )
}

/// Refusal for a background press that would open a Chromium `<select>` menu
/// (CUA-1795). Opening it orders the browser window above the user's window
/// for at least a frame, which undoing afterwards cannot hide; `set_value`
/// selects through the page's type-ahead and never opens the menu.
pub(super) fn chromium_select_open_refusal(
    pid: i32,
    window_id: u32,
    element_index: Option<usize>,
    element: AXUIElementRef,
) -> cua_driver_core::protocol::ToolResult {
    let title = unsafe { copy_string_attr(element, "AXTitle") }.unwrap_or_default();
    let current = unsafe { copy_string_attr(element, "AXValue") }.unwrap_or_default();
    let target = match element_index {
        Some(index) => format!("[{index}] AXPopUpButton \"{title}\""),
        None => format!("the AXPopUpButton \"{title}\" at that point"),
    };
    cua_driver_core::protocol::ToolResult::error(format!(
        "Not opened: {target} is a web dropdown (<select>). In the background, opening its \
         menu brings the browser window above the user's window, so nothing was sent. Its \
         current value is \"{current}\". Choose an option in one call with \
         set_value(element_token, \"<option text>\"): it types the option's text into the \
         closed control and the menu never opens. Open the menu with \
         delivery_mode:\"foreground\" only if the user agreed to the browser coming forward."
    ))
    .with_structured(serde_json::json!({
        "code": "select_menu_would_raise_window",
        "effect": "refused",
        "pid": pid,
        "window_id": window_id,
        "current_value": current,
        "escalation": {
            "recommended": "set_value",
            "reason": "a background <select> is chosen with set_value; opening its menu would raise the browser window",
        },
    }))
}

/// How long the page may take to apply a type-ahead selection.
const TYPEAHEAD_SETTLE: std::time::Duration = std::time::Duration::from_millis(800);

/// Select a background Chromium `<select>` without opening its menu.
///
/// Chromium shows a `<select>` menu as a native popup. Opening it orders the
/// whole browser window above the user's window, and the window stays there
/// after the menu closes; re-activating the user's app does not reorder it
/// (Bench v2: WF-02 to WF-08, IR-05, IR-06, MA-02, CDB-G04). A focused closed
/// `<select>` takes the page's own type-ahead instead: typing an option's
/// label selects it and fires `input` and `change`, and the menu never opens.
///
/// Returns `None` when this route does not apply or did not land on `value`;
/// the caller then falls back to opening the menu.
fn select_chromium_popup_by_typeahead(
    popup: AXUIElementRef,
    element_index: usize,
    pid: i32,
    window_id: u32,
    value: &str,
    element_title: &str,
) -> Option<String> {
    let wanted = value.trim();
    if wanted.is_empty()
        || crate::apps::frontmost_pid() == Some(pid)
        || popup_menu_is_open(popup)
        || !is_chromium_popup_owner(pid)
    {
        return None;
    }
    let selected = || unsafe { copy_string_attr(popup, "AXValue") }.unwrap_or_default();
    let matches = |current: &str| current.trim().to_lowercase() == wanted.to_lowercase();
    if matches(&selected()) {
        return Some(format!(
            "✅ '{wanted}' is already selected in AXPopUpButton [{element_index}] \
             \"{element_title}\"; nothing was changed."
        ));
    }
    // Focus moves inside the page only: no activation, no window order change.
    unsafe { crate::ax::bindings::set_bool_attr_true(popup, "AXFocused") };
    std::thread::sleep(POPUP_POLL_INTERVAL);
    if unsafe { copy_bool_attr(popup, "AXFocused") } != Some(true) {
        return None;
    }
    // Keys go to the window's focused element, so the window must be the
    // app's key window (in-app records only; the app is not activated).
    let _scope = super::key_scope(pid, Some(window_id)).ok()?;
    crate::input::keyboard::type_text(pid, wanted).ok()?;
    let deadline = std::time::Instant::now() + TYPEAHEAD_SETTLE;
    loop {
        let current = selected();
        if matches(&current) {
            return Some(format!(
                "✅ Selected '{current}' in AXPopUpButton [{element_index}] \
                 \"{element_title}\" by typing its label into the focused control \
                 (the menu was not opened, so the window order did not change)."
            ));
        }
        if std::time::Instant::now() >= deadline {
            tracing::debug!(
                pid,
                wanted,
                current,
                "type-ahead did not select the option; opening the menu"
            );
            return None;
        }
        std::thread::sleep(POPUP_POLL_INTERVAL);
    }
}

fn select_popup_option(
    element: AXUIElementRef,
    element_index: usize,
    pid: i32,
    window_id: u32,
    value: &str,
    element_title: &str,
) -> anyhow::Result<String> {
    if let Some(detail) = select_chromium_popup_by_typeahead(
        element,
        element_index,
        pid,
        window_id,
        value,
        element_title,
    ) {
        return Ok(detail);
    }
    let mut options = popup_options(element);
    let mut opened_menu = false;
    let mut press_thread = None;

    if matching_option(&option_pairs(&options), value).is_none() {
        // Safari/WebKit: no AX children while the popup is closed. Set the
        // <select> through the DOM instead of opening a menu.
        if options.is_empty() {
            let app_name = crate::apps::get_app_name_for_pid(pid).unwrap_or_default();
            if app_name == "Safari" {
                return set_select_via_js(element_index, element_title, value);
            }
        }
        // AppKit NSPopUpButton and Chromium <select> publish their items only
        // while the menu is open (a closed Chromium popup lists just the
        // selected one). Open it, wait for the full list, press the match,
        // and close the menu again whatever happens. The menu's tracking
        // steals key focus; in the background the key-focus guard armed by
        // the caller hands it straight back to the user's front window.
        //
        // The AXPress returns only once the app's menu tracking lets it (or
        // the messaging timeout fires), so run it on its own thread and read
        // the items as soon as they appear: the menu, and with it the key
        // focus it holds, stays open for the shortest time.
        let already_open = popup_menu_is_open(element);
        let closed_titles = if already_open {
            Vec::new()
        } else {
            option_pairs(&options)
        };
        release_options(&options);
        opened_menu = true;
        // A menu left open by an earlier click is already showing its items;
        // pressing again would close it.
        if !already_open {
            let popup_address = element as usize;
            press_thread = Some(std::thread::spawn(move || unsafe {
                let popup = popup_address as AXUIElementRef;
                crate::ax::bindings::AXUIElementSetMessagingTimeout(
                    popup,
                    POPUP_PRESS_TIMEOUT_SECONDS,
                );
                let _ = perform_action(popup, "AXPress");
                crate::ax::bindings::AXUIElementSetMessagingTimeout(popup, 0.0);
            }));
        }
        let deadline = std::time::Instant::now() + POPUP_OPEN_TIMEOUT;
        let mut last_seen: Vec<(String, String)> = Vec::new();
        let mut stable_polls = 0;
        loop {
            options = popup_options(element);
            let seen = option_pairs(&options);
            if matching_option(&seen, value).is_some() {
                break;
            }
            // The menu is open once the list differs from the closed one; it
            // is complete once the list stops changing.
            if !seen.is_empty() && seen != closed_titles {
                stable_polls = if seen == last_seen {
                    stable_polls + 1
                } else {
                    0
                };
                if stable_polls >= POPUP_STABLE_POLLS {
                    break;
                }
            }
            last_seen = seen;
            if std::time::Instant::now() >= deadline {
                break;
            }
            release_options(&options);
            std::thread::sleep(POPUP_POLL_INTERVAL);
        }
        if options.is_empty() {
            dismiss_popup_menu(element, pid);
            if let Some(press) = press_thread.take() {
                let _ = press.join();
            }
            anyhow::bail!(
                "AXPopUpButton [{element_index}] \"{element_title}\" exposed no options even \
                 after its menu was opened, so nothing was selected. Click the popup, then \
                 choose the option with press_key (down, return) or a pixel click on the item."
            )
        }
    }

    let result = match matching_option(&option_pairs(&options), value) {
        Some(i) => {
            let option = &options[i];
            let err = unsafe { perform_action(option.element, "AXPress") };
            if err == kAXErrorSuccess {
                let label = if option.title.is_empty() {
                    &option.value
                } else {
                    &option.title
                };
                Ok(format!(
                    "✅ Selected '{label}' in AXPopUpButton [{element_index}] \
                     \"{element_title}\" via AX menu item AXPress{}.",
                    if opened_menu {
                        " (the popup's menu was opened for the selection and closed again)"
                    } else {
                        ""
                    }
                ))
            } else {
                if opened_menu {
                    dismiss_popup_menu(element, pid);
                }
                Err(anyhow::anyhow!(
                    "AXPress on menu item failed with error {err}"
                ))
            }
        }
        None => {
            if opened_menu {
                dismiss_popup_menu(element, pid);
            }
            Err(anyhow::anyhow!(
                "No option matching '{value}' in AXPopUpButton [{element_index}] \
                 \"{element_title}\". Available: [{}]",
                describe_available(&options)
            ))
        }
    };
    // The opening AXPress returns once the menu is closed or its timeout
    // fires; wait for it so no AX call outlives this tool call.
    if let Some(press) = press_thread {
        let _ = press.join();
    }
    release_options(&options);
    result
}

#[cfg(test)]
mod popup_option_tests {
    use super::matching_option;

    fn pairs(titles: &[&str]) -> Vec<(String, String)> {
        titles
            .iter()
            .map(|title| ((*title).to_owned(), String::new()))
            .collect()
    }

    #[test]
    fn matches_title_ignoring_case_and_whitespace() {
        let options = pairs(&["Open", "Duplicate", "Closed"]);
        assert_eq!(matching_option(&options, "duplicate"), Some(1));
        assert_eq!(matching_option(&options, "  Closed "), Some(2));
        assert_eq!(matching_option(&options, "Pending"), None);
    }

    #[test]
    fn matches_value_when_title_differs() {
        let options = vec![("Duplicate of".to_owned(), "dup".to_owned())];
        assert_eq!(matching_option(&options, "DUP"), Some(0));
    }
}

// ── Safari JavaScript fallback ───────────────────────────────────────────────

/// Set an HTML `<select>` value in Safari via `osascript do JavaScript`.
/// Searches all `<select>` elements for an `<option>` whose text or value matches
/// `value` (case-insensitive), then sets it and dispatches a `change` event.
fn set_select_via_js(
    element_index: usize,
    element_title: &str,
    value: &str,
) -> anyhow::Result<String> {
    // Percent-encode the lowercased value using only unreserved URL characters
    // as the allowed set, matching the Swift reference's percent-encoding approach.
    // This makes the string safe to embed in both a JS single-quoted string
    // (via decodeURIComponent) and an AppleScript double-quoted string.
    let v_low = value.to_lowercase();
    let v_encoded = percent_encode_unreserved(&v_low);

    // JavaScript that matches the Swift reference verbatim.
    let js = format!(
        "(function(){{\
         var v=decodeURIComponent('{v_encoded}');\
         var ss=document.querySelectorAll('select'),opts=[];\
         for(var i=0;i<ss.length;i++){{\
         for(var j=0;j<ss[i].options.length;j++){{\
         var t=ss[i].options[j].text.toLowerCase(),\
         u=ss[i].options[j].value.toLowerCase();\
         opts.push(t+'|'+u);\
         if(t===v||u===v){{\
         ss[i].value=ss[i].options[j].value;\
         ss[i].dispatchEvent(new Event('change',{{bubbles:true}}));\
         return 'SET:'+ss[i].value;}}}}\
         }}return 'NOTFOUND:'+opts.join(',');\
         }})()"
    );

    let apple_script =
        format!("tell application \"Safari\" to do JavaScript \"{js}\" in front document");

    // Spawn osascript with a 10-second deadline. A stuck Safari permission
    // prompt or unresponsive renderer can cause wait() to block indefinitely,
    // which would stall the MCP tool handler permanently.
    let mut child = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&apple_script)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("osascript launch failed: {e}"))?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    anyhow::bail!("osascript timed out after 10 seconds");
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => anyhow::bail!("osascript wait error: {e}"),
        }
    }
    let out = child
        .wait_with_output()
        .map_err(|e| anyhow::anyhow!("osascript output error: {e}"))?;

    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();

    if let Some(dom_val) = raw.strip_prefix("SET:") {
        Ok(format!(
            "✅ Set select [{element_index}] '{element_title}' to '{value}' via \
             Safari JavaScript (DOM value: \"{dom_val}\")."
        ))
    } else if let Some(available) = raw.strip_prefix("NOTFOUND:") {
        anyhow::bail!(
            "No <option> matching '{value}' found in any <select>. \
             Available (text|value): {available}"
        )
    } else if raw.is_empty() && !out.status.success() {
        let err_text = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("osascript failed: {}", err_text.trim())
    } else {
        anyhow::bail!(
            "JavaScript returned unexpected output: {}",
            &raw[..raw.len().min(200)]
        )
    }
}

// ── Percent-encoding helper ──────────────────────────────────────────────────

/// Percent-encode a string, leaving only unreserved URL characters (`-._~` +
/// alphanumerics) unencoded.  Matches the Swift reference's approach.
fn percent_encode_unreserved(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_' || b == b'~' {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_digit(b >> 4));
            out.push(hex_digit(b & 0xF));
        }
    }
    out
}

fn hex_digit(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'A' + n - 10) as char,
        _ => '0',
    }
}

#[cfg(test)]
mod tests {
    use super::{
        address_field_refusal, apply_surface_trust, apply_verification_label, classify_write,
        file_name_needs_rename, is_file_name_cell, is_get_info_name_field, is_safari_address_field,
        not_applied_result, text_value_role_requires_exact_focus, write_not_applied,
        SetValueOutcome, GET_INFO_RENAME_ROUTE, LIST_RENAME_ROUTE,
    };

    #[test]
    fn autosubmit_schema_matches_shared_contract() {
        let schema = &super::def().input_schema;
        assert_eq!(schema["properties"]["autosubmit"]["type"], "boolean");
        assert!(
            cua_driver_core::tool_schema::shared_schema_violations("set_value", schema).is_empty()
        );
        if let Some(contract) = cua_driver_contract::tool_contract("set_value") {
            assert!(
                cua_driver_contract::compatibility::schema_subset_violations(
                    &contract.input_schema,
                    schema
                )
                .is_empty()
            );
        }
    }

    #[test]
    fn only_safaris_address_field_is_an_address_field() {
        let id = Some("WEB_BROWSER_ADDRESS_AND_SEARCH_FIELD");
        assert!(is_safari_address_field(Some("com.apple.Safari"), id));
        assert!(is_safari_address_field(
            Some("com.apple.SafariTechnologyPreview"),
            id
        ));
        assert!(!is_safari_address_field(
            Some("com.apple.Safari"),
            Some("SearchField")
        ));
        assert!(!is_safari_address_field(Some("com.apple.Safari"), None));
        assert!(!is_safari_address_field(Some("com.apple.TextEdit"), id));
        assert!(!is_safari_address_field(None, id));
    }

    #[test]
    fn the_address_field_is_refused_with_the_route_that_navigates() {
        // Safari reads the written AXValue back while the field still shows
        // and keeps the page's URL; that read-back was reported as confirmed.
        let result = address_field_refusal(7, 9);
        assert_eq!(result.is_error, Some(true));
        let data = result.structured_content.expect("structured refusal");
        assert_eq!(data["code"], "address_field_not_settable");
        assert_eq!(data["effect"], "refused");
        assert_eq!(data["pid"], 7);
        assert_eq!(data["window_id"], 9);
        let text = data["reason"].as_str().unwrap();
        assert!(text.contains("Open Location"), "{text}");
        assert!(text.contains("type_text"), "{text}");
        assert!(text.contains("press_key return"), "{text}");
    }

    #[test]
    fn a_listed_file_name_is_a_file_name_cell() {
        let url = Some("file:///Users/me/lab/charlie.bin");
        assert!(is_file_name_cell(Some("charlie.bin"), url, Some(false)));
        assert!(is_file_name_cell(Some("charlie.bin"), url, None));
    }

    #[test]
    fn a_finder_rename_publishes_a_confirmed_system_api_record() {
        let record = super::rename_record();
        assert!(record.validate().is_ok());
        let path = std::path::Path::new("/tmp/a.txt");
        let ok = super::finder_rename_result(7, 42, path, "b.txt", Ok(Ok("/tmp/b.txt".into())));
        assert_eq!(ok.is_error, None);
        assert!(ok.action_record.is_some());
        let refused = super::finder_rename_result(
            7,
            42,
            path,
            "b.txt",
            Ok(Err(crate::apple_events::ScriptRefusal {
                code: "automation_denied",
                reason: "off".into(),
            })),
        );
        assert_eq!(refused.is_error, Some(true));
        let data = refused.structured_content.unwrap();
        assert_eq!(data["code"], "file_name_needs_rename");
        assert_eq!(data["cause"], "automation_denied");
    }

    #[test]
    fn rename_editor_and_ordinary_fields_stay_writable() {
        let url = Some("file:///Users/me/lab/charlie.bin");
        // Finder's inline rename editor is focused while editing.
        assert!(!is_file_name_cell(Some("charlie.bin"), url, Some(true)));
        // A plain text field names no file.
        assert!(!is_file_name_cell(None, None, Some(false)));
        assert!(!is_file_name_cell(Some(""), url, Some(false)));
        assert!(!is_file_name_cell(Some("charlie.bin"), None, Some(false)));
        assert!(!is_file_name_cell(
            Some("page"),
            Some("https://example.com/page"),
            None
        ));
    }

    #[test]
    fn get_info_name_field_is_refused_and_its_neighbours_are_not() {
        const FINDER: Option<&str> = Some("com.apple.finder");
        // (bundle id, role, AXIdentifier, AXFocused, refused). Identifiers are
        // the ones Finder reported on macOS 26.4.
        let cases = [
            (FINDER, "AXTextField", Some("Name"), Some(false), true),
            (FINDER, "AXTextField", Some("Name"), None, true),
            // A real click starts an edit session; Return then commits an
            // AXValue write, so the focused field stays writable.
            (FINDER, "AXTextField", Some("Name"), Some(true), false),
            // The "Name & Extension" disclosure triangle shares the identifier.
            (FINDER, "AXDisclosureTriangle", Some("Name"), None, false),
            // Tags field, list inline rename editor, list name cell.
            (FINDER, "AXTextField", Some("_NS:34"), None, false),
            (
                FINDER,
                "AXTextField",
                Some("ShrinkToFit Text Field"),
                Some(true),
                false,
            ),
            (FINDER, "AXTextField", None, Some(false), false),
            (FINDER, "AXTextArea", Some("Comments"), None, false),
            // The same field shape in another app.
            (
                Some("com.example.notes"),
                "AXTextField",
                Some("Name"),
                None,
                false,
            ),
            (None, "AXTextField", Some("Name"), None, false),
        ];
        for (bundle, role, identifier, focused, refused) in cases {
            assert_eq!(
                is_get_info_name_field(bundle, Some(role), identifier, focused),
                refused,
                "{bundle:?} {role} {identifier:?} {focused:?}"
            );
        }
    }

    #[test]
    fn get_info_refusal_names_the_foreground_route() {
        let result = file_name_needs_rename(7, 42, GET_INFO_RENAME_ROUTE);
        let data = result.structured_content.unwrap();
        assert_eq!(data["code"], "file_name_needs_rename");
        let reason = data["reason"].as_str().unwrap();
        for needed in [
            "Get Info",
            "nothing was written",
            "screenshot pixels",
            "capture_id",
            "cmd+a",
            "type_text",
            "return",
            "delivery_mode:\"foreground\"",
        ] {
            assert!(reason.contains(needed), "missing {needed:?}: {reason}");
        }
    }

    #[test]
    fn an_unchanged_read_back_is_not_applied_but_an_idempotent_one_is_fine() {
        // Read-back still shows the old value: not applied.
        let (verified, changed) =
            classify_write(Some("pending"), Some("pending"), "matched", false);
        assert!(write_not_applied(verified, changed));
        // Already held the requested value: idempotent, applied.
        let (verified, changed) =
            classify_write(Some("matched"), Some("matched"), "matched", false);
        assert!(!write_not_applied(verified, changed));
        // The app reformatted the value: it moved, so it is not "not applied".
        let (verified, changed) = classify_write(Some(""), Some("1,248.60"), "1248.6", false);
        assert!(!write_not_applied(verified, changed));
        // Unreadable before or after: cannot tell.
        let (verified, changed) = classify_write(None, Some("x"), "y", false);
        assert!(!write_not_applied(verified, changed));
        let refusal = not_applied_result(4, true);
        assert_eq!(refusal.is_error, Some(true));
        let data = refusal.structured_content.unwrap();
        assert_eq!(data["code"], "write_not_applied");
        assert_eq!(data["effect"], "not_applied");
    }

    #[test]
    fn file_name_refusal_names_the_rename_route() {
        let result = file_name_needs_rename(7, 42, LIST_RENAME_ROUTE);
        assert_eq!(result.is_error, Some(true));
        let data = result.structured_content.unwrap();
        assert_eq!(data["code"], "file_name_needs_rename");
        assert_eq!(data["effect"], "refused");
        assert_eq!(
            (data["pid"].as_i64(), data["window_id"].as_u64()),
            (Some(7), Some(42))
        );
        let reason = data["reason"].as_str().unwrap();
        for needed in [
            "never the file",
            "nothing was written",
            "return",
            "cmd+a",
            "type_text",
            "desktop",
        ] {
            assert!(reason.contains(needed), "missing {needed:?}: {reason}");
        }
    }

    #[test]
    fn unreadable_value_reports_neither_verified_nor_changed() {
        // AXValue is not exposed: the write can be neither confirmed nor denied,
        // so the tool must not claim success on the return code alone.
        assert_eq!(
            classify_write(Some("old"), None, "new", false),
            (None, None)
        );
    }

    #[test]
    fn matching_read_back_verifies_the_write() {
        assert_eq!(
            classify_write(Some("old"), Some("new"), "new", false),
            (Some(true), Some(true))
        );
    }

    #[test]
    fn echoed_but_wrong_value_fails_verification() {
        // Web content behind an AXWebArea accepts the write and echoes a value
        // the renderer never took. A success return code must not be reported
        // as a verified write.
        assert_eq!(
            classify_write(Some("old"), Some("old"), "new", false),
            (Some(false), Some(false))
        );
    }

    #[test]
    fn idempotent_write_is_verified_but_unchanged() {
        assert_eq!(
            classify_write(Some("same"), Some("same"), "same", false),
            (Some(true), Some(false))
        );
    }

    #[test]
    fn numeric_controls_compare_numerically() {
        // AXSlider reports "25.0" for a requested "25".
        assert_eq!(
            classify_write(Some("10"), Some("25.000000001"), "25", true),
            (Some(true), Some(true))
        );
    }

    #[test]
    fn numeric_text_is_not_normalised_on_a_text_target() {
        assert_eq!(
            classify_write(Some("old"), Some("7"), "007", false),
            (Some(false), Some(true))
        );
    }

    #[test]
    fn missing_before_still_verifies_numeric_after() {
        assert_eq!(
            classify_write(None, Some("25.0"), "25", true),
            (Some(true), None)
        );
    }

    #[test]
    fn web_content_ax_echo_is_never_reported_as_verified() {
        let mut outcome = SetValueOutcome {
            detail: "Set value.".to_owned(),
            verified: Some(true),
            changed: Some(true),
            not_applied: false,
            document_edited: None,
            maybe_landed: false,
        };
        apply_surface_trust(&mut outcome, true);
        assert_eq!(outcome.verified, Some(false));
        assert_eq!(outcome.changed, None);
        assert!(outcome.detail.contains("not trusted for web content"));
    }

    /// On web content an unchanged read-back is reported as unverifiable, so
    /// autosubmit must use the outcome itself: Return would submit the old text.
    #[tokio::test]
    async fn autosubmit_skips_return_after_a_web_write_that_did_not_apply() {
        let write_not_applied = true;
        let result = super::set_value_tool_result(
            Ok(Ok(SetValueOutcome {
                detail: "✅ Set value.".to_owned(),
                verified: Some(false),
                changed: Some(false),
                not_applied: write_not_applied,
                document_edited: None,
                maybe_landed: false,
            })),
            true,
            String::new(),
            42,
            3,
        );
        assert_eq!(
            result.structured_content.as_ref().unwrap()["effect"],
            "unverifiable"
        );
        let result = super::super::set_value_autosubmit::finish(
            true,
            42,
            result,
            write_not_applied,
            false,
            |_, _| async { panic!("Return after a write that did not apply") },
        )
        .await;
        let note = result.structured_content.unwrap()["note"].clone();
        assert!(
            note.as_str().unwrap().contains("Return was not posted"),
            "{note}"
        );
    }

    #[tokio::test]
    async fn autosubmit_preserves_native_not_applied_error() {
        let result = not_applied_result(3, false);
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.as_ref().unwrap()["code"],
            "write_not_applied"
        );
        let before = serde_json::to_value(&result).unwrap();
        let after = super::super::set_value_autosubmit::finish(
            true,
            42,
            result,
            true,
            false,
            |_, _| async { panic!("Return after a native write that did not apply") },
        )
        .await;
        assert_eq!(serde_json::to_value(&after).unwrap(), before);
    }

    #[test]
    fn native_read_back_remains_trusted() {
        let mut outcome = SetValueOutcome {
            detail: "Set value.".to_owned(),
            verified: Some(true),
            changed: Some(true),
            not_applied: false,
            document_edited: None,
            maybe_landed: false,
        };
        apply_surface_trust(&mut outcome, false);
        assert_eq!(outcome.verified, Some(true));
        assert_eq!(outcome.changed, Some(true));
        assert_eq!(outcome.detail, "Set value.");
    }

    #[test]
    fn editable_text_roles_require_exact_focus_before_value_writes() {
        for role in [
            "AXTextField",
            "AXTextArea",
            "AXSearchField",
            "AXSecureTextField",
            "AXComboBox",
        ] {
            assert!(text_value_role_requires_exact_focus(role), "{role}");
        }
        for role in ["AXSlider", "AXStepper", "AXPopUpButton", "AXButton"] {
            assert!(!text_value_role_requires_exact_focus(role), "{role}");
        }
    }

    #[test]
    fn unverified_result_does_not_keep_a_success_checkmark() {
        let mut outcome = SetValueOutcome {
            detail: "✅ Set AXValue on [4] AXTextField.".to_owned(),
            verified: Some(false),
            changed: Some(false),
            not_applied: false,
            document_edited: None,
            maybe_landed: false,
        };
        apply_verification_label(&mut outcome);
        assert_eq!(
            outcome.detail,
            "📨 Sent (unverified) AXValue on [4] AXTextField."
        );
    }
}

#[cfg(test)]
mod restore_cleanup_tests {
    use super::super::type_text::RestoreReport;
    use super::attach_restore_cleanup;
    use cua_driver_core::protocol::ToolResult;

    #[test]
    fn every_restore_result_carries_its_cleanup() {
        let error = attach_restore_cleanup(
            ToolResult::error("set_value failed: x"),
            RestoreReport::ReminimizeNotConfirmed(Some(false)),
            "",
        );
        assert_eq!(
            error.structured_content.as_ref().expect("structured")["restore_cleanup"],
            "reminimize_not_confirmed"
        );
        // The error's own text reaches structured-only clients too.
        assert!(
            error.structured_content.as_ref().expect("structured")["message"]
                .as_str()
                .is_some_and(|m| m.contains("set_value failed: x"))
        );
        let refusal = attach_restore_cleanup(
            ToolResult::error("refused").with_structured(serde_json::json!({"code": "c"})),
            RestoreReport::ReminimizeNotConfirmed(Some(false)),
            "Key focus went back to the app's previous window.",
        );
        let structured = refusal.structured_content.as_ref().expect("structured");
        assert_eq!(structured["code"], "c");
        assert_eq!(structured["restore_cleanup"], "reminimize_not_confirmed");
        let note = structured["restore_note"]
            .as_str()
            .expect("structured note");
        assert!(note.contains("minimizing it again") && note.contains("went back"));
        let texts: Vec<String> = refusal
            .content
            .iter()
            .filter_map(|c| match c {
                cua_driver_core::protocol::Content::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(texts
            .iter()
            .any(|t| t.contains("minimizing it again") && t.contains("went back")));
        // Already described in the text: not repeated.
        let once = attach_restore_cleanup(
            ToolResult::text(format!("ok. {}", RestoreReport::Reminimized.describe())),
            RestoreReport::Reminimized,
            "",
        );
        assert_eq!(once.content.len(), 1);
    }
}

#[cfg(test)]
mod ladder_tests {
    use super::*;
    use crate::ax::bindings::{kAXErrorCannotComplete, kAXErrorIllegalArgument};
    use std::collections::VecDeque;

    struct Fake {
        reads: VecDeque<ValueObs>,
        last: ValueObs,
        settable: Settable,
        writes: Vec<&'static str>,
        results: VecDeque<i32>,
        steps: VecDeque<i32>,
        pauses: usize,
    }
    impl Fake {
        fn new(before: ValueObs) -> Self {
            Self {
                reads: VecDeque::from([before.clone()]),
                last: before,
                settable: Settable::Yes,
                writes: vec![],
                results: VecDeque::new(),
                steps: VecDeque::new(),
                pauses: 0,
            }
        }
        fn read(mut self, value: ValueObs) -> Self {
            self.reads.push_back(value);
            self
        }
        fn result(mut self, code: i32) -> Self {
            self.results.push_back(code);
            self
        }
        fn step_result(mut self, code: i32) -> Self {
            self.steps.push_back(code);
            self
        }
        fn unsettable(mut self) -> Self {
            self.settable = Settable::No;
            self
        }
    }
    impl AxValueOps for Fake {
        fn read_typed(&mut self) -> ValueObs {
            if let Some(v) = self.reads.pop_front() {
                self.last = v;
            }
            self.last.clone()
        }
        fn query_settable(&mut self) -> Settable {
            self.settable
        }
        fn set_number(&mut self, _: f64) -> i32 {
            self.writes.push("number");
            self.results.pop_front().unwrap_or(kAXErrorSuccess)
        }
        fn set_string(&mut self, _: &str) -> i32 {
            self.writes.push("string");
            self.results.pop_front().unwrap_or(kAXErrorSuccess)
        }
        fn perform_step(&mut self, _: &str) -> i32 {
            self.writes.push("step");
            self.steps.pop_front().unwrap_or(kAXErrorSuccess)
        }
        fn settle_pause(&mut self) {
            self.pauses += 1;
        }
    }
    fn run(f: &mut Fake, request: &str, can_step: bool, secure: bool) -> LadderOutcome {
        write_value_ladder(f, request, &LadderContext { secure, can_step })
    }
    fn num(n: f64) -> ValueObs {
        ValueObs::Number(n)
    }
    fn text(s: &str) -> ValueObs {
        ValueObs::Text(s.into())
    }

    #[test]
    fn an_uncertain_write_is_never_followed_by_another_mutation() {
        let mut f = Fake::new(num(0.)).result(kAXErrorCannotComplete);
        let out = run(&mut f, "5", true, false);
        assert_eq!(f.writes, ["number"]);
        assert!(out.maybe_landed);
        assert!(!out.not_applied);
    }
    #[test]
    fn a_delayed_apply_after_an_uncertain_write_gets_no_second_write() {
        let mut f = Fake::new(num(0.))
            .read(num(0.))
            .read(num(5.))
            .result(kAXErrorCannotComplete);
        let out = run(&mut f, "5", true, false);
        assert_eq!(f.writes, ["number"]);
        assert!(out.maybe_landed);
        assert_eq!(out.verified, Some(true));
    }
    #[test]
    fn unsettable_numeric_with_step_actions_steps_only() {
        let mut f = Fake::new(num(0.))
            .unsettable()
            .read(num(1.))
            .read(num(2.))
            .read(num(2.));
        let out = run(&mut f, "2", true, false);
        assert_eq!(f.writes, ["step", "step"]);
        assert!(out.reached);
        assert_eq!(out.steps_maybe_landed, 2);
    }
    #[test]
    fn a_failed_settable_query_never_steps() {
        let mut f = Fake::new(num(0.)).result(kAXErrorCannotComplete);
        f.settable = Settable::Unknown(kAXErrorCannotComplete);
        let out = run(&mut f, "2", true, false);
        assert_eq!(f.writes, ["number"]);
        assert!(out.maybe_landed);
    }
    #[test]
    fn settable_slider_gets_one_absolute_write() {
        let mut f = Fake::new(num(0.)).read(num(2.));
        let out = run(&mut f, "2", true, false);
        assert_eq!(f.writes, ["number"]);
        assert_eq!(out.verified, Some(true));
    }
    #[test]
    fn a_failure_on_a_settable_slider_stops_without_stepping() {
        let mut f = Fake::new(num(0.))
            .result(kAXErrorIllegalArgument)
            .result(kAXErrorIllegalArgument);
        let out = run(&mut f, "2", true, false);
        assert_eq!(f.writes, ["number", "string"]);
        assert!(!out.reached);
    }
    #[test]
    fn a_pending_write_cannot_combine_with_steps() {
        let mut f = Fake::new(num(0.))
            .unsettable()
            .step_result(kAXErrorCannotComplete);
        let out = run(&mut f, "2", true, false);
        assert_eq!(f.writes, ["step"]);
        assert!(out.maybe_landed);
    }
    #[test]
    fn a_known_numeric_value_gets_a_numeric_write() {
        let mut f = Fake::new(num(0.));
        run(&mut f, "5", false, false);
        assert_eq!(f.writes, ["number"]);
    }
    #[test]
    fn a_known_text_value_gets_a_string_write() {
        let mut f = Fake::new(text("0"));
        run(&mut f, "5", false, false);
        assert_eq!(f.writes, ["string"]);
    }
    #[test]
    fn text_zero_value_rejecting_string_accepts_number() {
        let mut f = Fake::new(text("0"))
            .read(text("5"))
            .result(kAXErrorIllegalArgument);
        let out = run(&mut f, "5", false, false);
        assert_eq!(f.writes, ["string", "number"]);
        assert_eq!(out.verified, Some(true));
    }
    #[test]
    fn number_before_with_non_numeric_request_writes_string_once() {
        let mut f = Fake::new(num(0.));
        run(&mut f, "x", false, false);
        assert_eq!(f.writes, ["string"]);
    }
    #[test]
    fn an_unknown_type_rejected_number_retries_as_string_once() {
        let mut f = Fake::new(ValueObs::OtherType).result(kAXErrorIllegalArgument);
        run(&mut f, "5", false, false);
        assert_eq!(f.writes, ["number", "string"]);
    }
    #[test]
    fn a_rejected_step_stops_and_reports_steps_sent() {
        let mut f = Fake::new(num(0.))
            .unsettable()
            .step_result(kAXErrorIllegalArgument);
        let out = run(&mut f, "5", true, false);
        assert_eq!(f.writes, ["step"]);
        assert_eq!(out.steps_sent, 1);
        assert_eq!(out.steps_maybe_landed, 0);
        assert!(!out.reached);
    }
    #[test]
    fn an_uncertain_step_stops_further_steps() {
        let mut f = Fake::new(num(0.))
            .unsettable()
            .step_result(kAXErrorCannotComplete);
        let out = run(&mut f, "5", true, false);
        assert_eq!(f.writes, ["step"]);
        assert_eq!(out.steps_maybe_landed, 1);
        assert!(out.maybe_landed);
        assert!(!out.reached);
    }
    #[test]
    fn step_size_learns_only_from_applied_steps() {
        let mut f = Fake::new(num(0.))
            .unsettable()
            .read(num(2.))
            .read(num(100.))
            .step_result(kAXErrorSuccess)
            .step_result(kAXErrorCannotComplete);
        let out = run(&mut f, "5", true, false);
        assert_eq!(f.writes, ["step", "step"]);
        assert!(!out.reached);
        assert_eq!(f.reads.len(), 1);
    }
    #[test]
    fn uncertain_then_rejected_keeps_the_warning() {
        let mut f = Fake::new(num(0.))
            .result(kAXErrorCannotComplete)
            .result(kAXErrorIllegalArgument);
        let out = run(&mut f, "5", true, false);
        assert!(out.maybe_landed);
        assert_eq!(f.results.len(), 1);
        assert_eq!(f.writes, ["number"]);
    }
    #[test]
    fn clamp_to_before_stays_unverified() {
        let mut f = Fake::new(num(10.));
        let out = run(&mut f, "20", false, false);
        assert_eq!(out.verified, Some(false));
        assert!(!out.not_applied);
    }
    #[test]
    fn masked_secure_value_is_not_judged_and_not_rewritten() {
        let mut f = Fake::new(text("***"));
        let out = run(&mut f, "5", false, true);
        assert_eq!(f.writes, ["string"]);
        assert_eq!(out.verified, None);
        assert_eq!(f.pauses, 0);
    }
    #[test]
    fn before_equals_target_is_written_once_and_judged_idempotent() {
        let mut f = Fake::new(num(5.));
        let out = run(&mut f, "5", false, false);
        assert_eq!(f.writes, ["number"]);
        assert_eq!((out.verified, out.changed), (Some(true), Some(false)));
    }
    #[test]
    fn applied_write_rereads_three_times_before_not_applied() {
        let mut f = Fake::new(text("old"));
        let out = run(&mut f, "new", false, false);
        assert_eq!(f.pauses, 3);
        assert!(out.not_applied);
    }
    #[test]
    fn secure_fields_skip_rereads() {
        let mut f = Fake::new(text("old"));
        let out = run(&mut f, "new", false, true);
        assert_eq!(f.pauses, 0);
        assert_eq!(out.verified, None);
    }
    #[test]
    fn stepping_success_message_and_half_step_tolerance_unchanged() {
        let mut f = Fake::new(num(0.)).unsettable().read(num(2.)).read(num(2.));
        let out = run(&mut f, "2.9", true, false);
        assert!(out.reached);
        assert_eq!(out.steps_sent, 1);
        let result = finish_ladder(out, std::ptr::null_mut(), 0, 1, "AXSlider", false).unwrap();
        assert!(result
            .detail
            .contains("via AXIncrement/AXDecrement stepping."));
    }
    #[test]
    fn after_uncertain_no_document_keys_are_sent() {
        let mut f = Fake::new(text("old"))
            .read(text("new"))
            .result(kAXErrorCannotComplete);
        let out = run(&mut f, "new", false, false);
        let doc = document_after_ladder(
            &out,
            false,
            || panic!("document keys after uncertain write"),
            || panic!("unexpected document query"),
        );
        assert_eq!(doc, crate::document_edit::DocumentEdited::NotADocument);
        assert!(out.maybe_landed);
    }
    #[test]
    fn applied_path_still_marks_documents_and_preserves_result_fields() {
        let mut f = Fake::new(text("old")).read(text("new"));
        let out = run(&mut f, "new", false, false);
        let doc = document_after_ladder(
            &out,
            false,
            || crate::document_edit::DocumentEdited::Marked,
            || panic!("unexpected document query"),
        );
        assert_eq!(doc.structured(), Some(true));
        let value = SetValueOutcome {
            detail: "set".into(),
            verified: out.verified,
            changed: out.changed,
            not_applied: out.not_applied,
            document_edited: doc.structured(),
            maybe_landed: out.maybe_landed,
        };
        let result = set_value_tool_result(Ok(Ok(value)), false, String::new(), 0, 1);
        let data = result.structured_content.unwrap();
        assert_eq!(data["effect"], "confirmed");
        assert_eq!(data["verified"], true);
        assert_eq!(data["document_key_edit_sent"], true);
        let refused = not_applied_result(1, false);
        let data = refused.structured_content.unwrap();
        assert_eq!(data["code"], "write_not_applied");
        assert_eq!(data["effect"], "not_applied");
        assert_eq!(data["verified"], false);
    }
    #[test]
    fn uncertain_value_result_remains_unverifiable_and_warns() {
        let mut f = Fake::new(num(0.)).result(kAXErrorCannotComplete);
        let out = run(&mut f, "5", false, false);
        let value = finish_ladder(out, std::ptr::null_mut(), 0, 1, "AXSlider", false).unwrap();
        assert!(value.detail.contains("may have changed"));
        let result = set_value_tool_result(Ok(Ok(value)), false, String::new(), 0, 1);
        let data = result.structured_content.unwrap();
        assert_eq!(data["effect"], "unverifiable");
        assert_eq!(data["verified"], false);
        assert!(data.get("document_key_edit_sent").is_none());
    }
}
