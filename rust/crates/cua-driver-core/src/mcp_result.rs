// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! The one place a `tools/call` result is held to the contract the driver
//! advertises.
//!
//! MCP validates every `structuredContent` a tool emits against that tool's
//! advertised `outputSchema` — payloads sent with `isError: true` included —
//! and rejects a response whose payload matches neither arm (`-32602`), or a
//! successful response that declares a schema and carries no structured
//! payload at all (`-32600`). Either way the client discards the whole
//! response, so the message the driver placed in `content` never reaches the
//! agent.
//!
//! Individual result constructors cannot enforce that: `ToolResult::error`
//! leaves `structuredContent` unset, the transport paths invent diagnostics
//! like `{"exit_code": 1}`, and the proxy synthesises an empty success when the
//! daemon answers `ok` with no result. So the rule lives here instead, on the
//! single path every direct and daemon-backed `tools/call` result passes
//! through.

use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
};
use tracing::warn;

use cua_driver_contract::{
    advertised_tool_output_schema, advertises_output_schema, conforming_error_envelope,
    derive_error_disposition, is_refusal_envelope, validate_success_output, ErrorDisposition,
};

/// Marker code for a result the driver replaced because the tool's own payload
/// could not be advertised under its `outputSchema`.
pub const TOOL_OUTPUT_INVALID_CODE: &str = "tool_output_invalid";

type CachedValidator = Result<Arc<jsonschema::Validator>, String>;
static OUTPUT_VALIDATORS: LazyLock<Mutex<HashMap<String, CachedValidator>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn validate_wire_output(tool: &str, structured: &Value) -> Result<(), String> {
    // Only published tools reach this cache, so arbitrary client names cannot grow it.
    let validator = OUTPUT_VALIDATORS
        .lock()
        .map_err(|_| "output schema cache unavailable".to_owned())?
        .entry(tool.to_owned())
        .or_insert_with(|| {
            let schema = advertised_tool_output_schema(tool)
                .ok_or_else(|| "advertised output schema unavailable".to_owned())?;
            jsonschema::validator_for(&schema)
                .map(Arc::new)
                .map_err(|error| format!("invalid advertised output schema: {error}"))
        })
        .clone()?;
    if validator.is_valid(structured) {
        Ok(())
    } else {
        Err("structured content does not match the advertised output schema".to_owned())
    }
}

/// Hold one `tools/call` result to the tool's advertised `outputSchema`.
///
/// - An error result keeps its diagnostic, normalized into the refusal arm so
///   a strict client accepts it and reads the `content` message.
/// - A successful result for a tool that advertises a schema must carry a
///   structured payload that the schema accepts. A missing or rejected payload
///   becomes a conforming internal-error result, because the alternative is a
///   response the client drops entirely.
/// - A successful result for a tool that advertises no schema is returned
///   unchanged: there is nothing for a client to validate it against.
pub fn conforming_tool_result(tool: &str, result: Value) -> Value {
    conforming_tool_result_inner(tool, result, advertises_output_schema(tool))
}

/// Reject contracts this proxy cannot safely normalize errors against. An older
/// daemon may omit its schema; that does not promise the local success contract.
pub fn validate_proxy_output_schema(tool: &str, schema: Option<&Value>) -> Result<(), String> {
    validate_proxy_schema_pair(tool, advertised_tool_output_schema(tool).as_ref(), schema)
}

fn validate_proxy_schema_pair(
    tool: &str,
    local: Option<&Value>,
    schema: Option<&Value>,
) -> Result<(), String> {
    if let Some(schema) = schema {
        if local != Some(schema) {
            return Err(format!(
                "incompatible daemon output schema for {tool}; use matching proxy and daemon versions"
            ));
        }
    }
    Ok(())
}

/// Apply the result boundary using the executing daemon's advertised contract.
/// Only matching canonical schemas enter the bounded validator cache.
pub fn conforming_proxy_tool_result(
    tool: &str,
    result: Value,
    schema: Option<&Value>,
) -> Result<Value, String> {
    validate_proxy_output_schema(tool, schema)?;
    Ok(conforming_tool_result_inner(tool, result, schema.is_some()))
}

fn conforming_tool_result_inner(tool: &str, result: Value, has_schema: bool) -> Value {
    finalize_error_disposition(tool, normalize_tool_result(tool, result, has_schema))
}

/// Finalize the disposition after a caller's result normalization. The SDK
/// also uses this boundary for direct results, which have no MCP schema arm.
pub fn finalize_error_disposition(tool: &str, mut normalized: Value) -> Value {
    if normalized.get("isError").and_then(Value::as_bool) == Some(true) {
        let result = normalized.as_object_mut().expect("isError on object");
        let original = result.remove("structuredContent").unwrap_or(Value::Null);
        let mut structured = match original {
            Value::Object(object) => Value::Object(object),
            Value::Null => json!({}),
            other => json!({"detail": other}),
        };
        let effect_evidence = derive_error_disposition(&structured, None);
        let evidence = if effect_evidence == ErrorDisposition::DELIVERED {
            effect_evidence
        } else {
            aggregate_evidence(tool, &structured).unwrap_or(effect_evidence)
        };
        let candidate = structured
            .get("disposition")
            .and_then(|value| serde_json::from_value::<ErrorDisposition>(value.clone()).ok())
            .filter(|value| value.is_valid());
        let selected = match candidate {
            Some(value)
                if value == ErrorDisposition::NOT_DELIVERED
                    && evidence != ErrorDisposition::UNKNOWN =>
            {
                evidence
            }
            Some(value)
                if effect_evidence == ErrorDisposition::DELIVERED
                    && structured.get("effect").and_then(Value::as_str) == Some("unverifiable")
                    && value.delivery == cua_driver_contract::InputDelivery::DeliveryUncertain =>
            {
                // An unverifiable effect does not prove delivery, so explicit
                // uncertainty is the truthful value; retry stays unsafe. A
                // partial effect proves some input landed and keeps
                // delivered_unverified.
                ErrorDisposition::SUSPECTED_NOOP
            }
            Some(value)
                if effect_evidence == ErrorDisposition::DELIVERED
                    && value.delivery == cua_driver_contract::InputDelivery::DeliveryUncertain =>
            {
                effect_evidence
            }
            Some(value) => value,
            None => evidence,
        };
        structured["disposition"] = serde_json::to_value(selected).expect("disposition serializes");
        result.insert("structuredContent".to_owned(), structured);
    }
    normalized
}

/// An aggregate's own action history takes precedence over its leaf error.
/// Only claim an attempted action when the public report proves one.
fn aggregate_evidence(tool: &str, structured: &Value) -> Option<ErrorDisposition> {
    if tool == "run_steps" {
        let action = structured.get("steps")?.as_array()?.iter().any(|step| {
            let action = step
                .get("tool")
                .and_then(Value::as_str)
                .is_some_and(|name| crate::batch_tools::BATCHABLE_TOOLS.contains(&name));
            action
                && (step.get("phase").and_then(Value::as_str) == Some("action")
                    || (step.get("ok").and_then(Value::as_bool) == Some(true)
                        && step.get("skipped").and_then(Value::as_bool) != Some(true)))
        });
        return action.then_some(ErrorDisposition::DELIVERED);
    }
    if tool == "run_script" {
        let calls = structured.get("calls").and_then(Value::as_array);
        let action_tool = |entry: &Value| {
            entry
                .get("tool")
                .and_then(Value::as_str)
                .is_some_and(|name| {
                    !matches!(
                        name,
                        "list_apps" | "list_windows" | "get_window_state" | "verify_state" | "zoom"
                    )
                })
        };
        let action = calls.is_some_and(|calls| {
            calls.iter().any(|call| {
                action_tool(call)
                    || call
                        .get("driver_calls")
                        .and_then(Value::as_array)
                        .is_some_and(|driver_calls| driver_calls.iter().any(action_tool))
            })
        });
        if action {
            return Some(ErrorDisposition::DELIVERED);
        }
        let incomplete = structured
            .pointer("/error/kind")
            .and_then(Value::as_str)
            .is_some_and(|kind| {
                matches!(
                    kind,
                    "timeout" | "cancelled" | "stalled" | "internal" | "memory"
                )
            })
            || structured.pointer("/error/code").and_then(Value::as_str) == Some("timeout")
            || calls.is_some_and(|calls| {
                calls.iter().any(|call| {
                    call.get("driver_calls_omitted")
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                        > 0
                })
            });
        return incomplete.then_some(ErrorDisposition::UNCERTAIN);
    }
    None
}

fn normalize_tool_result(tool: &str, result: Value, has_schema: bool) -> Value {
    let Value::Object(mut result) = result else {
        return internal_error_result(format!(
            "internal result mismatch for {tool}: tool result is not an object"
        ));
    };

    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let structured = result.remove("structuredContent").unwrap_or(Value::Null);
        result.insert(
            "structuredContent".to_owned(),
            conforming_error_envelope(structured),
        );
        return Value::Object(result);
    }

    if !has_schema {
        return Value::Object(result);
    }

    match result.get("structuredContent") {
        None | Some(Value::Null) => {
            warn!(
                tool,
                "successful tool result declared an output schema but carried no structured content"
            );
            invalid_output_result(result, format!(
                "internal output mismatch for {tool}: tool advertises an output schema but returned no structured content"
            ))
        }
        // A refusal payload satisfies the advertised schema's other arm. The
        // typed success validator would reject it, so ask the arm it belongs
        // to rather than replacing a payload a client accepts.
        Some(structured) if is_refusal_envelope(structured) => Value::Object(result),
        Some(structured) => match validate_wire_output(tool, structured)
            .and_then(|()| validate_success_output(tool, structured.clone()))
        {
            Ok(_) => Value::Object(result),
            Err(error) => {
                warn!(
                    tool,
                    "successful tool result carried structured content its output schema rejects"
                );
                invalid_output_result(
                    result,
                    format!("internal output mismatch for {tool}: {error}"),
                )
            }
        },
    }
}

fn invalid_output_result(original: serde_json::Map<String, Value>, message: String) -> Value {
    let mut result = internal_error_result(message);
    if let Some(Value::Array(content)) = original.get("content") {
        result["content"]
            .as_array_mut()
            .unwrap()
            .extend(content.clone());
    }
    if let Some(structured) = original.get("structuredContent") {
        // Preserve evidence as an explicitly invalid diagnostic, never as success.
        result["structuredContent"]["invalid_output"] = structured.clone();
    }
    result
}

/// Build the `isError: true` result for a call that produced no usable tool
/// payload, already normalized into the refusal arm.
pub fn internal_error_result(message: impl Into<String>) -> Value {
    let message = format!(
        "{}; the tool may have executed. Verify state before retrying.",
        message.into()
    );
    json!({
        "content": [{"type": "text", "text": message}],
        "isError": true,
        "structuredContent": {"code": TOOL_OUTPUT_INVALID_CODE, "execution_state": "unknown"},
    })
}

/// Build a tool-level error result carrying a caller-supplied diagnostic.
///
/// The diagnostic is passed through untouched here; [`conforming_tool_result`]
/// is what normalizes it, so every error path gets the same treatment whether
/// or not it went through this constructor.
pub fn tool_error_result(message: impl Into<String>, structured: Value) -> Value {
    let message = message.into();
    json!({
        "content": [{"type": "text", "text": message}],
        "isError": true,
        "structuredContent": structured,
    })
}

#[cfg(test)]
mod tests {
    //! The boundary is asserted the way a strict MCP client sees it: every
    //! payload leaving it is validated against the schema the tool actually
    //! advertises.

    #[test]
    fn typed_uncertain_survives_unverifiable_effect() {
        let result = super::finalize_error_disposition(
            "click",
            serde_json::json!({"isError": true, "structuredContent": {
                "effect": "unverifiable",
                "disposition": {"delivery": "delivery_uncertain", "retry": "unsafe"}
            }}),
        );
        assert_eq!(
            result["structuredContent"]["disposition"],
            serde_json::json!({
                "delivery": "delivery_uncertain", "retry": "unsafe"
            })
        );
        let unknown_retry = super::finalize_error_disposition(
            "click",
            serde_json::json!({"isError": true, "structuredContent": {
                "effect": "unverifiable",
                "disposition": {"delivery": "delivery_uncertain", "retry": "unknown"}
            }}),
        );
        assert_eq!(
            unknown_retry["structuredContent"]["disposition"],
            serde_json::json!({"delivery": "delivery_uncertain", "retry": "unsafe"})
        );
        let partial = super::finalize_error_disposition(
            "click",
            serde_json::json!({"isError": true, "structuredContent": {
                "effect": "partial",
                "disposition": {"delivery": "delivery_uncertain", "retry": "unsafe"}
            }}),
        );
        assert_eq!(
            partial["structuredContent"]["disposition"],
            serde_json::json!({"delivery": "delivered_unverified", "retry": "unsafe"})
        );
    }
    use super::*;
    use cua_driver_contract::{advertised_tool_output_schema, TOOL_INVOCATION_FAILED_CODE};

    /// An action tool, so the advertised schema is the shared `ActionResult`
    /// one plus the refusal arm.
    const ACTION_TOOL: &str = "click";
    /// Advertises no `outputSchema` at all.
    const UNSCHEMED_TOOL: &str = "no_such_tool";

    #[test]
    fn malformed_result_replacement_is_unknown() {
        let result = conforming_tool_result(ACTION_TOOL, json!("malformed"));
        assert_eq!(
            result["structuredContent"]["disposition"],
            json!({
                "delivery": "unknown", "retry": "unknown"
            })
        );
    }

    #[test]
    fn not_delivered_with_partial_is_downgraded() {
        let result = conforming_tool_result(
            ACTION_TOOL,
            tool_error_result(
                "partial",
                json!({
                    "effect": "partial", "disposition": {"delivery": "not_delivered", "retry": "safe"}
                }),
            ),
        );
        assert_eq!(
            result["structuredContent"]["disposition"],
            json!({
                "delivery": "delivered_unverified", "retry": "unsafe"
            })
        );
    }

    #[test]
    fn effect_refused_alone_is_unknown() {
        let result = conforming_tool_result(
            ACTION_TOOL,
            tool_error_result("refused", json!({"effect": "refused"})),
        );
        assert_eq!(
            result["structuredContent"]["disposition"],
            json!({
                "delivery": "unknown", "retry": "unknown"
            })
        );
    }

    #[test]
    fn suspected_noop_is_uncertain_and_unsafe() {
        let result = conforming_tool_result(
            ACTION_TOOL,
            tool_error_result("uncertain", json!({"effect": "suspected_noop"})),
        );
        assert_eq!(
            result["structuredContent"]["disposition"],
            json!({
                "delivery": "delivery_uncertain", "retry": "unsafe"
            })
        );
    }

    #[test]
    fn invalid_pair_is_replaced() {
        let result = conforming_tool_result(
            ACTION_TOOL,
            tool_error_result(
                "bad",
                json!({
                    "disposition": {"delivery": "delivered_unverified", "retry": "safe"}
                }),
            ),
        );
        assert_eq!(
            result["structuredContent"]["disposition"],
            json!({
                "delivery": "unknown", "retry": "unknown"
            })
        );
    }

    #[test]
    fn unverifiable_effect_is_delivered_unverified() {
        let result = conforming_tool_result(
            ACTION_TOOL,
            tool_error_result("unverified", json!({"effect": "unverifiable"})),
        );
        assert_eq!(
            result["structuredContent"]["disposition"],
            json!({
                "delivery": "delivered_unverified", "retry": "unsafe"
            })
        );
    }

    #[test]
    fn proxy_path_keeps_schema_equality() {
        let schema = advertised_tool_output_schema(ACTION_TOOL).unwrap();
        let result = conforming_proxy_tool_result(
            ACTION_TOOL,
            tool_error_result("error", json!({})),
            Some(&schema),
        )
        .unwrap();
        assert_conforms(ACTION_TOOL, &result);
        assert_eq!(
            result["structuredContent"]["disposition"],
            json!({
                "delivery": "unknown", "retry": "unknown"
            })
        );
        assert!(validate_proxy_output_schema(ACTION_TOOL, Some(&schema)).is_ok());
    }

    #[test]
    fn aggregate_dominates_leaf_refusal_code() {
        let result = conforming_tool_result(
            "run_steps",
            tool_error_result(
                "refused",
                json!({
                    "code":"permission_denied", "steps":[{"tool":"click","ok":true}, {"tool":"type_text","ok":false,"phase":"action","effect":"refused"}],
                    "disposition":{"delivery":"not_delivered","retry":"safe"}
                }),
            ),
        );
        assert_eq!(
            result["structuredContent"]["disposition"],
            json!({"delivery":"delivered_unverified", "retry":"unsafe"})
        );
    }

    #[test]
    fn incomplete_history_is_not_not_delivered() {
        let result = conforming_tool_result(
            "run_script",
            tool_error_result(
                "timeout",
                json!({
                    "error":{"kind":"timeout"}, "calls":[],
                    "disposition":{"delivery":"not_delivered","retry":"safe"}
                }),
            ),
        );
        assert_eq!(
            result["structuredContent"]["disposition"],
            json!({"delivery":"delivery_uncertain", "retry":"unknown"})
        );
    }

    #[test]
    fn nested_script_action_contradicts_not_delivered() {
        let result = conforming_tool_result(
            "run_script",
            tool_error_result(
                "failed",
                json!({
                    "calls":[{"tool":"get_window_state", "driver_calls":[{"tool":"click"}, {"tool":"get_window_state"}]}],
                    "disposition":{"delivery":"not_delivered", "retry":"safe"}
                }),
            ),
        );
        assert_eq!(
            result["structuredContent"]["disposition"],
            json!({"delivery":"delivered_unverified", "retry":"unsafe"})
        );
    }

    #[test]
    fn success_payload_untouched() {
        let result = json!({"isError":false, "content":[], "structuredContent":{"disposition":"tool value"}});
        assert_eq!(
            conforming_tool_result("no_such_tool", result.clone()),
            result
        );
    }

    #[test]
    fn window_schema_accepts_old_and_new_payloads_and_refuses_mixed_versions() {
        let tool = "get_window_state";
        let new_schema = advertised_tool_output_schema(tool).unwrap();
        let validator = jsonschema::validator_for(&new_schema).unwrap();
        let old_success: Value = serde_json::from_str(include_str!(
            "../../../../contract/fixtures/get-window-state-old-success-schema.json"
        ))
        .unwrap();
        let old_schema = cua_driver_contract::advertised_output_schema(old_success);
        let old_validator = jsonschema::validator_for(&old_schema).unwrap();
        let old_element = &old_schema["anyOf"][0]["properties"]["elements"]["items"];
        let new_element = &new_schema["anyOf"][0]["properties"]["elements"]["items"];
        assert_eq!(
            old_element.get("additionalProperties"),
            new_element.get("additionalProperties")
        );
        let old_payload = json!({"pid":7,"window_id":9,"elements":[{"element_index":1,"role":"button","depth":0,"identifier":"mac-id","focused":true}]});
        let new_payload = json!({"pid":7,"window_id":9,"observation_complete":false,
            "observation_incomplete_reasons":[{"kind":"budget","detail":"node_budget"}],
            "elements":[{"element_index":1,"role":"button","depth":0,"identifier":"mac-id","focused":true,"boundary":{
                "reasons":[{"kind":"budget","detail":"node_budget"}],"children_skipped":{"known":2}}}]});
        assert!(old_validator.is_valid(&old_payload));
        assert!(validator.is_valid(&old_payload));
        assert!(
            validator.is_valid(&new_payload),
            "{:?}",
            validator.validate(&new_payload)
        );
        let mut invalid = new_payload.clone();
        invalid["elements"][0]["boundary"]["reasons"][0]["detail"] = json!("not_a_reason");
        assert!(!validator.is_valid(&invalid));
        let message = "incompatible daemon output schema for get_window_state; use matching proxy and daemon versions";
        assert_eq!(
            validate_proxy_schema_pair(tool, Some(&new_schema), Some(&old_schema)).unwrap_err(),
            message
        );
        assert_eq!(
            validate_proxy_schema_pair(tool, Some(&old_schema), Some(&new_schema)).unwrap_err(),
            message
        );
        assert_eq!(
            validate_proxy_output_schema(tool, Some(&old_schema)).unwrap_err(),
            message
        );
    }

    #[track_caller]
    fn assert_conforms(tool: &str, result: &Value) {
        let schema = advertised_tool_output_schema(tool).expect("tool advertises a schema");
        let compiled = jsonschema::validator_for(&schema).expect("schema compiles");
        let structured = &result["structuredContent"];

        assert!(
            compiled.is_valid(structured),
            "advertised schema rejected {structured}"
        );
    }

    /// The shape this boundary exists for: the bare diagnostic the transport
    /// paths invent satisfies neither arm, which is what produced the `-32602`
    /// reports.
    #[test]
    fn a_bare_exit_code_diagnostic_does_not_validate() {
        let schema = advertised_tool_output_schema(ACTION_TOOL).expect("schema");
        let compiled = jsonschema::validator_for(&schema).expect("schema compiles");

        assert!(!compiled.is_valid(&json!({"exit_code": 1})));
    }

    #[test]
    fn an_error_diagnostic_gains_a_refusal_marker() {
        let result = conforming_tool_result(
            ACTION_TOOL,
            tool_error_result("daemon transport closed", json!({"exit_code": 1})),
        );

        assert_conforms(ACTION_TOOL, &result);
        // The diagnostic survives; only the marker is added.
        assert_eq!(result["structuredContent"]["exit_code"], 1);
        assert_eq!(
            result["structuredContent"]["code"],
            TOOL_INVOCATION_FAILED_CODE
        );
        assert_eq!(result["content"][0]["text"], "daemon transport closed");
    }

    /// `ToolResult::error` leaves `structuredContent` unset, and every tool
    /// that refuses through it lands here.
    #[test]
    fn an_error_without_any_payload_gains_one() {
        let result = conforming_tool_result(
            ACTION_TOOL,
            json!({"content": [{"type": "text", "text": "Unknown tool: nope"}], "isError": true}),
        );

        assert_conforms(ACTION_TOOL, &result);
        assert_eq!(
            result["structuredContent"]["code"],
            TOOL_INVOCATION_FAILED_CODE
        );
    }

    /// The guard adds a marker; it does not relabel a payload that already
    /// names its own refusal.
    #[test]
    fn an_existing_refusal_code_is_preserved() {
        let result = conforming_tool_result(
            ACTION_TOOL,
            tool_error_result("denied", json!({"code": "permission_denied"})),
        );

        assert_conforms(ACTION_TOOL, &result);
        assert_eq!(result["structuredContent"]["code"], "permission_denied");
    }

    /// The proxy's empty-success fallback: a client that was promised a schema
    /// rejects this response outright rather than reading it as an error.
    #[test]
    fn a_success_missing_its_structured_payload_becomes_an_error() {
        let result = conforming_tool_result(ACTION_TOOL, json!({"content": [], "isError": false}));

        assert_conforms(ACTION_TOOL, &result);
        assert_eq!(result["isError"], true);
        assert_eq!(
            result["structuredContent"]["code"],
            TOOL_OUTPUT_INVALID_CODE
        );
    }

    #[test]
    fn a_success_whose_payload_the_schema_rejects_becomes_an_error() {
        let result = conforming_tool_result(
            ACTION_TOOL,
            json!({
                "content": [],
                "isError": false,
                "structuredContent": {"clicked": true},
            }),
        );

        assert_conforms(ACTION_TOOL, &result);
        assert_eq!(result["isError"], true);
        assert_eq!(
            result["structuredContent"]["code"],
            TOOL_OUTPUT_INVALID_CODE
        );
    }

    #[test]
    fn a_conforming_success_is_returned_untouched() {
        let structured = json!({
            "effect": "confirmed",
            "route": "accessibility",
            "delivery": {"mode": "background"},
            "evidence": [{"kind": "value_readback"}],
        });
        let success = json!({
            "content": [{"type": "text", "text": "clicked"}],
            "isError": false,
            "structuredContent": structured,
        });

        let result = conforming_tool_result(ACTION_TOOL, success.clone());

        assert_conforms(ACTION_TOOL, &result);
        assert_eq!(result, success);
    }

    #[test]
    fn a_success_missing_required_nullable_fields_becomes_a_conforming_error() {
        let structured = json!({
            "session": "schema-test",
            "capture_scope": "window",
            "effective_scope": "window",
            "desktop_capture_authorized": false,
            "desktop_unlocked": false,
        });
        let tool = "get_session_state";
        assert!(validate_success_output(tool, structured.clone()).is_ok());
        let result = conforming_tool_result(
            tool,
            json!({
                "content": [{"type": "text", "text": "action completed"}],
                "isError": false,
                "structuredContent": structured,
            }),
        );

        assert_conforms(tool, &result);
        assert_eq!(result["isError"], true);
        assert_eq!(
            result["structuredContent"]["code"],
            TOOL_OUTPUT_INVALID_CODE
        );
        assert_eq!(result["structuredContent"]["execution_state"], "unknown");
        assert_eq!(
            result["structuredContent"]["invalid_output"]["session"],
            "schema-test"
        );
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Verify state before retrying"));
        assert_eq!(result["content"][1]["text"], "action completed");
    }

    fn agent_cursor_state(position: Value) -> Value {
        json!({
            "session": "cursor-state-test",
            "enabled": true,
            "position": position,
            "theme": {
                "id": "cua-default",
                "version": "1",
                "profile": "default",
                "reduced_motion": "auto",
                "fallback": null,
            },
            "visual_state": {
                "requested_action": "idle",
                "resolved_action": "idle",
                "modifiers": [],
                "phase": "idle",
                "frame": 0,
                "preempted_count": 0,
            },
            "motion": {
                "start_handle": 0.3,
                "end_handle": 0.3,
                "arc_size": 0.25,
                "arc_flow": 0.0,
                "spring": 0.72,
                "glide_duration_ms": 750.0,
                "dwell_after_click_ms": 400.0,
                "idle_hide_ms": 3000.0,
                "turn_radius": 12.0,
                "style": "signature_arc",
                "timing": "native",
                "effects": {
                    "trail": false,
                    "glow": true,
                    "magnet": false,
                    "ripple": true,
                    "squish": true,
                },
            },
        })
    }

    /// A session cursor that has never moved has no position yet. Every
    /// platform reports that as `"position": null`, and the advertised schema
    /// must accept it rather than turning the read into `tool_output_invalid`.
    #[test]
    fn agent_cursor_state_accepts_a_cursor_that_has_not_moved() {
        const TOOL: &str = "get_agent_cursor_state";
        for position in [Value::Null, json!({"x": 10.0, "y": 20.0})] {
            let success = json!({
                "content": [{"type": "text", "text": "Agent cursor state."}],
                "isError": false,
                "structuredContent": agent_cursor_state(position),
            });

            let result = conforming_tool_result(TOOL, success.clone());

            assert_conforms(TOOL, &result);
            assert_eq!(result, success);
        }
    }

    /// `position` stays required: an unmoved cursor reports `null`, it does
    /// not drop the key.
    #[test]
    fn agent_cursor_state_without_a_position_key_is_rejected() {
        const TOOL: &str = "get_agent_cursor_state";
        let mut structured = agent_cursor_state(Value::Null);
        structured.as_object_mut().unwrap().remove("position");

        let result = conforming_tool_result(
            TOOL,
            json!({"content": [], "isError": false, "structuredContent": structured}),
        );

        assert_eq!(result["isError"], true);
        assert_eq!(
            result["structuredContent"]["code"],
            TOOL_OUTPUT_INVALID_CODE
        );
    }

    /// A tool with no advertised schema has no contract to violate, so its
    /// successful payload must not be second-guessed here.
    #[test]
    fn a_success_from_a_tool_without_a_schema_is_untouched() {
        let success = json!({
            "content": [{"type": "text", "text": "ok"}],
            "isError": false,
            "structuredContent": {"anything": "goes"},
        });

        assert_eq!(
            conforming_tool_result(UNSCHEMED_TOOL, success.clone()),
            success
        );
    }

    #[test]
    fn a_result_that_is_not_an_object_becomes_a_conforming_error() {
        let result = conforming_tool_result(ACTION_TOOL, json!("not a result"));

        assert_conforms(ACTION_TOOL, &result);
        assert_eq!(result["isError"], true);
    }
}
