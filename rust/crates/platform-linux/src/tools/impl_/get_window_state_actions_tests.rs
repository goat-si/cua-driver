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
