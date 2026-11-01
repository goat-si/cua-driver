use super::*;
use crate::atspi::AtspiNode;

fn node(actions: Vec<String>) -> AtspiNode {
    AtspiNode {
        element_index: Some(1),
        role: "button".to_owned(),
        name: Some("ok".to_owned()),
        value: None,
        checked: None,
        enabled: Some(true),
        selected: None,
        toggle_state: None,
        expanded: None,
        description: None,
        actions,
        element_key: 1,
        identity: None,
        depth: 0,
        parent_element_index: None,
        in_web_content: false,
        object_ref: None,
    }
}

#[test]
fn element_entry_omits_actions_when_empty() {
    let n = node(Vec::new());
    let entry = build_element_entry(&n, None, None).unwrap();
    assert!(entry.get("actions").is_none());
}

#[test]
fn element_entry_filters_blank_action_names() {
    let n = node(vec![
        "Press".to_owned(),
        "".to_owned(),
        "   ".to_owned(),
        "Open".to_owned(),
    ]);
    let entry = build_element_entry(&n, None, None).unwrap();
    assert_eq!(entry["actions"], json!(["Press", "Open"]));
}

// goat-si/goat#1261: a toggle's state and an expander's state reach the structured row, and an
// element without either says nothing about them.
#[test]
fn element_entry_reports_toggle_state_and_expanded() {
    use cua_driver_contract::ToggleState;
    for (state, expected) in [
        (ToggleState::On, json!("on")),
        (ToggleState::Off, json!("off")),
        (ToggleState::Mixed, json!("mixed")),
    ] {
        let mut n = node(Vec::new());
        n.toggle_state = Some(state);
        n.expanded = Some(false);
        let entry = build_element_entry(&n, None, None).unwrap();
        assert_eq!(entry["toggle_state"], expected);
        assert_eq!(entry["expanded"], json!(false));
    }
    let entry = build_element_entry(&node(Vec::new()), None, None).unwrap();
    assert!(entry.get("toggle_state").is_none());
    assert!(entry.get("expanded").is_none());
}

fn tree_for_completeness() -> crate::atspi::AtspiTreeResult {
    crate::atspi::AtspiTreeResult {
        tree_markdown: String::new(),
        nodes: Vec::new(),
        bounds: Vec::new(),
        trusted: true,
        degraded_reason: None,
        window_scoped: true,
        truncated: false,
        truncation_reason: None,
        nodes_visited: 7,
        nodes_pending: 0,
        bounds_complete: true,
        elapsed_ms: 123,
        completeness: crate::walk_completeness::Completeness::new(true),
        boundaries: Default::default(),
    }
}

#[test]
fn get_window_state_completeness_adapter_preserves_legacy_fields_and_schema() {
    use crate::walk_completeness::Event;
    let mut tree = tree_for_completeness();
    tree.completeness.record(true, Event::ChildrenFailed, None);
    let mut structured = json!({"pid": 7, "window_id": 9, "truncation_reason": "stale"});
    apply_linux_walk_completeness(&mut structured, &tree, 500);
    assert_eq!(structured["observation_complete"], false);
    assert_eq!(structured["elements_complete"], false);
    assert!(structured.get("truncation_reason").is_none());
    assert_eq!(structured["nodes_visited"], 7);
    assert_eq!(structured["nodes_pending"], 0);
    assert_eq!(structured["walk_elapsed_ms"], 123);
    assert_eq!(structured["timeout_ms"], 500);
    structured["elements"] = json!([{
        "element_index": 1, "role": "button", "depth": 0,
        "boundary": {"reasons": [{"kind": "provider_failure", "detail": "child_enumeration_failed"}], "children_skipped": {"known": 3}}
    }]);
    let wire =
        json!({"content": [{"type": "text", "text": "tree"}], "structuredContent": structured});
    let checked =
        cua_driver_core::mcp_result::conforming_tool_result("get_window_state", wire.clone());
    assert_eq!(
        checked, wire,
        "core MCP result validation rejected the Linux output"
    );

    tree.truncated = true;
    tree.truncation_reason = Some("huge_container".into());
    tree.nodes_pending = 3;
    tree.completeness.record(true, Event::HugeContainer, None);
    apply_linux_walk_completeness(&mut structured, &tree, 500);
    assert_eq!(structured["truncation_reason"], "huge_container");
    assert_eq!(structured["nodes_pending"], 3);
    assert_eq!(structured["elements_complete"], false);
}

#[test]
fn sibling_only_stop_keeps_the_legacy_truncation_and_complete_observation() {
    let mut tree = tree_for_completeness();
    tree.truncated = true;
    tree.truncation_reason = Some("node_budget".into());
    tree.nodes_pending = 4;
    let mut structured = json!({"pid": 7, "window_id": 9});
    apply_linux_walk_completeness(&mut structured, &tree, 500);
    assert_eq!(structured["observation_complete"], true);
    assert_eq!(structured["elements_complete"], false);
    assert_eq!(structured["truncation_reason"], "node_budget");
    assert_eq!(structured["nodes_pending"], 4);
}

#[test]
fn get_window_state_fallback_preserves_timeout_and_is_untrusted() {
    use crate::walk_completeness::{Completeness, Event};
    let mut tree = tree_for_completeness();
    tree.trusted = false;
    tree.truncated = true;
    tree.truncation_reason = Some("timeout".into());
    tree.nodes_pending = 4;
    tree.completeness = Completeness::new(false);
    tree.completeness
        .record(true, Event::UntrustedProvider, None);
    tree.completeness.record(true, Event::TimeBudget, None);
    let mut structured = json!({"pid": 7, "window_id": 9});
    apply_linux_walk_completeness(&mut structured, &tree, 500);
    assert_eq!(structured["truncation_reason"], "timeout");
    assert_eq!(structured["nodes_pending"], 4);
    assert_eq!(structured["observation_complete"], false);
    let reasons = structured["observation_incomplete_reasons"]
        .as_array()
        .unwrap();
    assert!(reasons.iter().any(|r| r["detail"] == "untrusted_provider"));
    assert!(reasons.iter().any(|r| r["detail"] == "time_budget"));
}
