use super::*;
use cua_driver_core::window_state_view::{self, ViewContext, ViewKey, ViewOptions};
use serde_json::json;

fn project(search: bool, shared: bool, verify: bool) -> (Value, Vec<Content>, Vec<String>) {
    let (tree, trace) = crate::ax::tree::evidence_output_fixture(search, shared);
    let snapshot_id = (!verify).then_some(7);
    let elements = if verify {
        build_observation_elements_array(&tree.nodes)
    } else {
        build_elements_array_with_token(&tree.nodes, snapshot_id)
    };
    let query = search.then_some("Beta");
    let elements = cua_driver_core::element_query::project_elements_for_query(
        elements,
        query,
        &tree.tree_markdown,
    );
    let count = tree
        .nodes
        .iter()
        .filter(|node| node.element_index.is_some())
        .count();
    let returned = elements.len();
    let structured = tree_output_fields(
        99,
        42,
        count,
        returned,
        elements_complete(Some(&tree), true, verify),
        tree.tree_markdown.clone(),
        elements,
        search,
        Some(&tree),
        snapshot_id,
        42,
        None,
    );
    assert_eq!(
        structured["walk_elapsed_ms"].as_u64(),
        Some(tree.walk.elapsed_ms)
    );
    let content = response_content(99, 42, Some(&tree), None, false);
    (structured, content, trace)
}

fn assert_same_projection(mut baseline: Value, current: Value) {
    let baseline_elapsed = baseline["walk_elapsed_ms"]
        .as_u64()
        .expect("baseline elapsed time");
    let current_elapsed = current["walk_elapsed_ms"]
        .as_u64()
        .expect("current elapsed time");
    assert_eq!(baseline["walk_elapsed_ms"], json!(baseline_elapsed));
    assert_eq!(current["walk_elapsed_ms"], json!(current_elapsed));
    baseline["walk_elapsed_ms"] = json!(current_elapsed);
    assert_eq!(baseline, current);
    let encoded = serde_json::to_string(&current).unwrap();
    assert_eq!(serde_json::to_string(&baseline).unwrap(), encoded);
    assert!(!encoded.contains("read_evidence"));
    assert!(!encoded.contains("CompletionEvidence"));
    assert!(!encoded.contains("ProviderEnablement"));
}

fn paired(search: bool, verify: bool) -> (Value, Value, Vec<Content>, Vec<Content>) {
    let (baseline, baseline_content, baseline_trace) = project(search, false, verify);
    let (current, current_content, current_trace) = project(search, true, verify);
    assert_eq!(baseline_trace, current_trace);
    assert_eq!(
        serde_json::to_value(&baseline_content).unwrap(),
        serde_json::to_value(&current_content).unwrap()
    );
    (baseline, current, baseline_content, current_content)
}

fn view(payload: &mut Value, content: &mut [Content], args: Value) {
    let options = ViewOptions::from_args(&args).expect("view options");
    let context = ViewContext {
        pid: 42,
        window_id: 99,
        key: ViewKey {
            query: None,
            max_elements: 100,
            max_depth: None,
        },
        focus_probe: None,
    };
    window_state_view::apply(&options, &context, content, payload);
}

#[test]
fn ordinary_output_is_unchanged_by_evidence_refactor() {
    let (baseline, current, _, content) = paired(false, false);
    assert_eq!(
        serde_json::to_value(&content).unwrap()[0]["text"]
            .as_str()
            .unwrap()
            .split("\n\n")
            .next(),
        Some("window_id=99 pid=42 elements=3")
    );
    assert_eq!(current["snapshot_id"], "s00000007");
    assert_eq!(current["element_count"], 3);
    assert_eq!(current["returned_element_count"], 3);
    assert_eq!(current["elements"][0]["element_token"], "s00000007:0");
    assert_eq!(current["elements"][1]["element_token"], "s00000007:1");
    assert_eq!(current["elements"][2]["element_token"], "s00000007:2");
    assert_eq!(current["partial_walk"], true);
    assert_eq!(current["omitted_offscreen_rows"][0]["count"], 1);
    assert_same_projection(baseline, current);
}

#[test]
fn query_output_is_unchanged_by_evidence_refactor() {
    let (baseline, current, _, content) = paired(true, false);
    assert!(serde_json::to_value(&content).unwrap()[0]["text"]
        .as_str()
        .unwrap()
        .ends_with("  - [3] AXRow \"Beta\" [actions=[press]]\n"));
    assert_eq!(
        current["tree_markdown"],
        "- [0] AXTable \"Items\" [actions=[press]]\n  - [3] AXRow \"Beta\" [actions=[press]]\n"
    );
    assert_eq!(current["element_count"], 4);
    assert_eq!(current["filtered_element_count"], 2);
    assert_eq!(current["elements"][0]["element_token"], "s00000007:0");
    assert_eq!(current["elements"][1]["element_token"], "s00000007:3");
    assert_same_projection(baseline, current);
}

#[test]
fn full_output_is_unchanged_by_evidence_refactor() {
    let (mut baseline, mut current, mut baseline_content, mut current_content) =
        paired(false, false);
    view(
        &mut baseline,
        &mut baseline_content,
        json!({"full_output":true}),
    );
    view(
        &mut current,
        &mut current_content,
        json!({"full_output":true}),
    );
    assert_eq!(
        serde_json::to_value(baseline_content).unwrap(),
        serde_json::to_value(current_content).unwrap()
    );
    assert!(current.get("_note").is_some());
    assert!(current.get("tree_markdown").is_some());
    assert!(current.get("elements").is_some());
    assert_same_projection(baseline, current);
}

#[test]
fn verify_output_is_unchanged_by_evidence_refactor() {
    let (baseline, current, _, _) = paired(false, true);
    assert!(current.get("snapshot_id").is_none());
    assert_eq!(current["elements_complete"], false);
    assert_eq!(current["elements"].as_array().unwrap().len(), 4);
    assert_eq!(current["elements"][2]["display_only"], true);
    assert!(current["elements"][2].get("element_token").is_none());
    assert_same_projection(baseline, current);
}

#[test]
fn display_and_screenshot_only_outputs_are_unchanged_by_evidence_refactor() {
    let args = json!({"display_only":true, "include_accessibility_tree":false});
    assert!(!wants_tree(&args));
    assert!(window_state_view::display_only(&args).unwrap());
    let mut payload = tree_output_fields(
        99,
        42,
        0,
        0,
        false,
        String::new(),
        vec![],
        false,
        None,
        None,
        42,
        None,
    );
    window_state_view::mark_display_only(&mut payload);
    apply_screenshot_dimensions(&mut payload, Some((320, 200)), "image/png");
    assert_eq!(payload["display_only"], true);
    assert_eq!(
        payload["frame_note"],
        window_state_view::DISPLAY_ONLY_FRAME_NOTE
    );
    assert_eq!(payload["elements"], json!([]));
    assert_eq!(payload["tree_markdown"], "");
    assert_eq!(payload["screenshot_width"], 320);
    assert_eq!(payload["screenshot_height"], 200);
    assert_eq!(payload["screenshot_mime_type"], "image/png");
    assert!(payload.get("snapshot_id").is_none());
    assert!(payload.get("observation_complete").is_none());
    assert!(!serde_json::to_string(&payload)
        .unwrap()
        .contains("evidence"));
    let png = response_content(99, 42, None, Some((b"fake", false, 320, 200)), false);
    assert_eq!(
        serde_json::to_value(png).unwrap(),
        json!([
            {"type":"image", "data":"ZmFrZQ==", "mimeType":"image/png"},
            {"type":"text", "text":"window_id=99 pid=42 size=320x200"}
        ])
    );
    let jpeg = response_content(99, 42, None, Some((b"fake", false, 320, 200)), true);
    assert_eq!(
        serde_json::to_value(jpeg).unwrap(),
        json!([
            {"type":"image", "data":"ZmFrZQ==", "mimeType":"image/jpeg"},
            {"type":"text", "text":"window_id=99 pid=42 size=320x200"}
        ])
    );
    let jpeg_file = response_content(99, 42, None, Some((b"fake", true, 320, 200)), true);
    assert_eq!(
        serde_json::to_value(jpeg_file).unwrap(),
        json!([
            {"type":"text", "text":"window_id=99 pid=42 size=320x200"}
        ])
    );
}

#[test]
fn scope_discovery_and_budget_outputs_are_unchanged_by_evidence_refactor() {
    let tree = crate::ax::tree::evidence_deadline_fixture();
    let payload = tree_output_fields(
        99,
        42,
        0,
        0,
        false,
        tree.tree_markdown.clone(),
        vec![],
        false,
        Some(&tree),
        None,
        42,
        None,
    );
    assert_eq!(payload["truncated"], true);
    assert_eq!(payload["truncation_reason"], "timeout");
    assert_eq!(payload["nodes_visited"], 1);
    assert_eq!(payload["nodes_pending"], 0);
    assert_eq!(payload["timeout_ms"], 1000);
    assert_eq!(
        payload["walk_elapsed_ms"].as_u64(),
        Some(tree.walk.elapsed_ms)
    );
    assert_eq!(payload["observation_complete"], false);
    assert!(payload.get("snapshot_id").is_none());
    assert!(payload.get("observation_scope_exclusions").is_none());
    assert!(!serde_json::to_string(&payload)
        .unwrap()
        .contains("evidence"));
    let refusal = window_scope_refusal(42, 99, &crate::ax::WindowScope::NotFound).unwrap();
    assert_eq!(refusal.is_error, Some(true));
    assert_eq!(
        refusal.structured_content.unwrap()["code"],
        "window_id_not_found"
    );
    let panel = tree_output_fields(
        99,
        900,
        0,
        0,
        false,
        String::new(),
        vec![],
        false,
        None,
        None,
        42,
        Some(PanelAttestation {
            owner_pid: 900,
            start_time: (1, 2),
        }),
    );
    assert_eq!(panel["requested_pid"], 42);
    assert_eq!(panel["ax_pid"], 900);
    assert_eq!(panel["external_panel_service"], OPEN_SAVE_PANEL_SERVICE);
    assert!(panel.get("snapshot_id").is_none());
}
