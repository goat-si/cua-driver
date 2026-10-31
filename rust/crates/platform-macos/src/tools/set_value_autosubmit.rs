//! Search-field submission after an AX value write.

use cua_driver_core::protocol::ToolResult;
use serde_json::Value;

pub(super) async fn finish<F, Fut>(
    enabled: bool,
    pid: i32,
    mut result: ToolResult,
    write_not_applied: bool,
    maybe_landed: bool,
    post: F,
) -> ToolResult
where
    F: FnOnce(i32, &'static str) -> Fut,
    Fut: std::future::Future<Output = Result<bool, String>>,
{
    if !enabled || result.is_error == Some(true) {
        return result;
    }
    let effect = result
        .structured_content
        .as_ref()
        .and_then(|v| v.get("effect"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    // Submitting a write that did not take effect would run the search for
    // the field's old text. On web content an unchanged read-back is still
    // reported as unverifiable, so the outcome's own flag decides too.
    let note = match effect.as_deref() {
        _ if maybe_landed => "Return was not posted: the value write may have landed; read the value before deciding what to do next.".to_owned(),
        _ if write_not_applied => "Return was not posted: the read-back still showed the \
             previous text, so submitting would search for it."
            .to_owned(),
        Some(effect) if effect != "confirmed" && effect != "unverifiable" => format!(
            "Return was not posted: the value write reported effect {effect}, so submitting \
             would search for the field's previous text."
        ),
        _ => match post(pid, "return").await {
            Ok(true) => {
                "Return was posted to the search field's process; submission is unverified."
                    .to_owned()
            }
            Ok(false) => {
                "Return delivery is uncertain; it may have been posted. Submission is unverified."
                    .to_owned()
            }
            Err(reason) => {
                format!("The value write was sent, but Return was not posted: {reason}")
            }
        },
    };
    if let Some(cua_driver_core::protocol::Content::Text { text, .. }) = result.content.first_mut()
    {
        text.push(' ');
        text.push_str(&note);
    }
    let structured = result
        .structured_content
        .get_or_insert_with(|| serde_json::json!({}));
    let combined = match structured.get("note").and_then(Value::as_str) {
        Some(previous) if !previous.is_empty() => format!("{previous} {note}"),
        _ => note,
    };
    structured["note"] = Value::String(combined);
    result
}

pub(super) async fn post_return(
    lease: super::BackgroundMutationLease,
    window_id: u32,
    element: crate::ax::snapshot::RetainedElement,
    key: &'static str,
) -> Result<bool, String> {
    tokio::spawn(async move { post_return_inner(lease, window_id, element, key).await })
        .await
        .unwrap_or(Ok(false))
}

async fn post_return_inner(
    lease: super::BackgroundMutationLease,
    window_id: u32,
    element: crate::ax::snapshot::RetainedElement,
    key: &'static str,
) -> Result<bool, String> {
    let pid = lease.pid();
    let ptr = element.as_ptr();
    super::gate_user_takeover(pid, Some(window_id)).map_err(refusal_reason)?;
    if !super::type_text::element_in_requested_window(pid, window_id, ptr) {
        return Err("element_outside_target_window".into());
    }
    let gate_window = super::type_text::gate_window_id_for_element(ptr, window_id);
    let action = if crate::input::skylight::is_focus_without_raise_available() {
        cua_driver_core::background_input::BackgroundAction::ExactWindowKeysFocused
    } else {
        super::window_keyboard_action(
            cua_driver_core::background_input::BackgroundAction::GenericKey,
        )
    };
    lease
        .gate_again(gate_window, Some(ptr), action)
        .await
        .map_err(refusal_reason)?;
    let scope = super::acquire_key_scope(pid, Some(window_id), false)
        .await
        .map_err(refusal_reason)?;
    let task = crate::focus_guard::with_focus_suppressed_now(
        Some(pid),
        "set_value.autosubmit",
        || async move {
            crate::goat::action_latency::spawn_action_blocking(move || {
                let _lease = lease;
                // Declared after the lease so it drops first: hand-back
                // starts while this app's lease is still held, as in press_key.
                let _scope = scope;
                super::press_key::verify_raw_key_target(
                    pid,
                    Some(window_id),
                    Some(element.as_ptr()),
                    true,
                )?;
                Ok::<_, anyhow::Error>(crate::input::keyboard::press_key(pid, key, &[]))
            })
            .await
        },
    )
    .await;
    match task {
        Ok(Ok(Ok(()))) => Ok(true),
        Ok(Ok(Err(_))) | Err(_) => Ok(false),
        Ok(Err(reason)) => Err(reason.to_string()),
    }
}

fn refusal_reason(result: ToolResult) -> String {
    result
        .structured_content
        .as_ref()
        .and_then(|v| v.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("key gate refused or could not be checked")
        .to_owned()
}

pub(super) fn enabled(args: &Value) -> Result<bool, &'static str> {
    match args.get("autosubmit") {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err("autosubmit must be a boolean."),
    }
}

pub(super) fn search_field(role: Option<&str>, subrole: Option<&str>) -> bool {
    role == Some("AXSearchField") || subrole == Some("AXSearchField")
}

pub(super) fn minimized_refusal(window_id: u32) -> ToolResult {
    super::pre_dispatch_refusal(
        cua_driver_core::background_input::refusal_codes::MINIMIZED_OR_HIDDEN,
        format!(
            "window {window_id} is minimized: set_value autosubmit needs the window to stay \
             visible for Return, and the value write minimizes it again. No value or key was \
             sent; restore the window first."
        ),
    )
}

pub(super) fn non_search_refusal() -> ToolResult {
    super::pre_dispatch_refusal(
        "autosubmit_requires_search_field",
        "set_value autosubmit requires a search field; no value or key was sent.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn autosubmit_argument_defaults_false_and_rejects_non_boolean() {
        assert_eq!(enabled(&json!({})), Ok(false));
        assert_eq!(enabled(&json!({"autosubmit": true})), Ok(true));
        assert!(enabled(&json!({"autosubmit": "true"})).is_err());
        assert!(enabled(&json!({"autosubmit": null})).is_err());
    }

    #[test]
    fn autosubmit_requires_search_role_or_subrole() {
        assert!(search_field(Some("AXSearchField"), None));
        assert!(search_field(Some("AXTextField"), Some("AXSearchField")));
        assert!(!search_field(Some("AXTextField"), None));
        assert!(!search_field(None, None));
        let refusal = non_search_refusal();
        assert_eq!(refusal.is_error, Some(true));
        let data = refusal.structured_content.unwrap();
        assert_eq!(data["code"], "autosubmit_requires_search_field");
        assert_eq!(data["effect"], "refused");
    }

    #[tokio::test]
    async fn autosubmit_posts_return_after_write_to_target_pid() {
        let events = std::cell::RefCell::new(vec!["write".to_owned()]);
        let posted_events = &events;
        let result = ToolResult::text("set").with_structured(json!({"effect":"confirmed"}));
        let result = finish(true, 42, result, false, false, |pid, key| async move {
            posted_events.borrow_mut().push(format!("{key}:{pid}"));
            Ok(true)
        })
        .await;
        assert_eq!(*events.borrow(), vec!["write", "return:42"]);
        assert!(result.structured_content.unwrap()["note"]
            .as_str()
            .unwrap()
            .contains("posted"));
    }

    #[tokio::test]
    async fn autosubmit_default_never_posts_and_refusal_preserves_value_effect() {
        let result = ToolResult::text("set").with_structured(json!({"effect":"confirmed"}));
        let result = finish(false, 42, result, false, false, |_, _| async {
            panic!("unexpected key")
        })
        .await;
        assert_eq!(result.structured_content.unwrap()["effect"], "confirmed");
        let result = ToolResult::text("set").with_structured(json!({"effect":"unverifiable"}));
        let result = finish(true, 42, result, false, false, |_, _| async {
            Err("user_interacting".into())
        })
        .await;
        let data = result.structured_content.unwrap();
        assert_eq!(data["effect"], "unverifiable");
        assert!(data["note"].as_str().unwrap().contains("user_interacting"));
    }

    /// A write the driver saw not take effect must not be submitted: Return
    /// would run the search for the field's old text.
    #[tokio::test]
    async fn autosubmit_skips_return_when_the_value_did_not_apply() {
        for effect in ["not_applied", "refused"] {
            let result = ToolResult::text("set").with_structured(json!({ "effect": effect }));
            let result = finish(true, 42, result, false, false, |_, _| async {
                panic!("Return after {effect}")
            })
            .await;
            let data = result.structured_content.unwrap();
            assert_eq!(data["effect"], effect);
            assert!(data["note"]
                .as_str()
                .unwrap()
                .contains("Return was not posted"));
        }
    }
    #[tokio::test]
    async fn after_uncertain_autosubmit_posts_no_return() {
        let result = ToolResult::text("set").with_structured(json!({"effect":"confirmed"}));
        let result = finish(true, 42, result, false, true, |_, _| async {
            panic!("Return after uncertain write")
        })
        .await;
        let data = result.structured_content.unwrap();
        assert_eq!(data["effect"], "confirmed");
        assert!(data["note"]
            .as_str()
            .unwrap()
            .contains("Return was not posted"));
    }

    #[tokio::test]
    async fn applied_path_still_marks_documents_and_autosubmits() {
        let events = std::cell::RefCell::new(vec!["document"]);
        let result = ToolResult::text("set")
            .with_structured(json!({"effect":"confirmed", "document_key_edit_sent":true}));
        let result = finish(true, 42, result, false, false, |_, _| async {
            events.borrow_mut().push("return");
            Ok(true)
        })
        .await;
        assert_eq!(*events.borrow(), ["document", "return"]);
        assert_eq!(
            result.structured_content.unwrap()["document_key_edit_sent"],
            true
        );
    }
}
