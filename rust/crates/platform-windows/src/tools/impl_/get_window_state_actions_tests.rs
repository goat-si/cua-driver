use super::*;
use crate::uia::UiaNode;

fn node(actions: Vec<String>) -> UiaNode {
    UiaNode {
        element_index: Some(1),
        control_type: "Button".to_owned(),
        name: Some("OK".to_owned()),
        value: None,
        automation_id: None,
        help_text: None,
        actions,
        enabled: Some(true),
        selected: None,
        checked: None,
        expanded: None,
        element_ptr: 0,
        center_x: 0,
        center_y: 0,
        rect: None,
        msaa_role: None,
        depth: 0,
        parent_element_index: None,
        in_web_content: false,
    }
}

#[test]
fn element_entry_includes_actions_when_present() {
    let n = node(vec!["invoke".to_owned(), "toggle".to_owned()]);
    let entry = build_element_entry(&n, None).unwrap();
    assert_eq!(entry["actions"], json!(["invoke", "toggle"]));
}

#[test]
fn element_entry_omits_actions_when_empty() {
    let n = node(Vec::new());
    let entry = build_element_entry(&n, None).unwrap();
    assert!(entry.get("actions").is_none());
}

#[test]
fn element_entry_reports_checked_including_mixed() {
    for (state, expected) in [
        (crate::uia::CheckedState::On, json!(true)),
        (crate::uia::CheckedState::Off, json!(false)),
        (crate::uia::CheckedState::Mixed, json!("mixed")),
    ] {
        let mut n = node(vec!["toggle".to_owned()]);
        n.checked = Some(state);
        let entry = build_element_entry(&n, None).unwrap();
        assert_eq!(entry["checked"], expected);
    }
}

#[test]
fn element_entry_reports_expanded_and_collapsed() {
    for expanded in [true, false] {
        let mut n = node(vec!["expand".to_owned()]);
        n.expanded = Some(expanded);
        let entry = build_element_entry(&n, None).unwrap();
        assert_eq!(entry["expanded"], json!(expanded));
    }
}

#[test]
fn element_entry_omits_checked_and_expanded_when_unknown() {
    let n = node(vec!["invoke".to_owned()]);
    let entry = build_element_entry(&n, None).unwrap();
    assert!(entry.get("checked").is_none());
    assert!(entry.get("expanded").is_none());
}
