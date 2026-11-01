use super::*;

fn star(child_count: usize) -> Vec<Vec<usize>> {
    let mut children = vec![Vec::new(); child_count + 1];
    children[0] = (1..=child_count).collect();
    children
}

fn chain(len: usize) -> Vec<Vec<usize>> {
    let mut children = vec![Vec::new(); len];
    for (i, child) in children.iter_mut().enumerate().take(len.saturating_sub(1)) {
        *child = vec![i + 1];
    }
    children
}

fn walk_synthetic_tree(
    children: &[Vec<usize>],
    node: usize,
    depth: usize,
    state: &mut WalkState<'_>,
) {
    if !state.enter(depth) {
        return;
    }
    for child in &children[node] {
        walk_synthetic_tree(children, *child, depth + 1, state);
    }
}

#[test]
fn walk_state_is_complete_only_when_no_bound_skips_a_node() {
    struct Case {
        name: &'static str,
        children: Vec<Vec<usize>>,
        max_elements: usize,
        max_depth: usize,
        expected_visited: usize,
        expected_complete: bool,
    }

    let cases = [
        Case {
            name: "default bounds admit a 55-node tree",
            children: star(54),
            max_elements: 5000,
            max_depth: 25,
            expected_visited: 55,
            expected_complete: true,
        },
        Case {
            name: "visiting exactly max_elements is still complete",
            children: star(54),
            max_elements: 55,
            max_depth: 25,
            expected_visited: 55,
            expected_complete: true,
        },
        Case {
            name: "skipping a node past max_elements is incomplete",
            children: star(54),
            max_elements: 54,
            max_depth: 25,
            expected_visited: 54,
            expected_complete: false,
        },
        Case {
            name: "a leaf at exactly max_depth is still complete",
            children: chain(3),
            max_elements: 10,
            max_depth: 2,
            expected_visited: 3,
            expected_complete: true,
        },
        Case {
            name: "skipping a descendant past max_depth is incomplete",
            children: chain(3),
            max_elements: 10,
            max_depth: 1,
            expected_visited: 2,
            expected_complete: false,
        },
    ];

    for case in cases {
        let mut budget = WalkBudget::nodes_only(case.max_elements);
        let mut state = WalkState::new(&mut budget, case.max_depth);
        walk_synthetic_tree(&case.children, 0, 0, &mut state);
        assert_eq!(
            state.budget.outcome().nodes_visited,
            case.expected_visited,
            "{}: visited",
            case.name
        );
        assert_eq!(
            state.complete, case.expected_complete,
            "{}: complete",
            case.name
        );
    }
}

#[test]
fn walk_state_keeps_the_callers_budget_across_walks() {
    let mut budget = WalkBudget::nodes_only(2);
    assert!(budget.admit());
    let mut state = WalkState::new(&mut budget, 25);

    walk_synthetic_tree(&star(1), 0, 0, &mut state);

    assert!(!state.complete);
    let outcome = state.budget.outcome();
    assert_eq!((outcome.nodes_visited, outcome.nodes_pending), (2, 1));
    assert_eq!(outcome.reason(), Some("node_budget"));
    assert!(!state.finish());
    let mut output = serde_json::json!({});
    budget.outcome().apply(&mut output);
    assert_eq!(
        output["observation_incomplete_reasons"],
        serde_json::json!([
            {"kind":"budget","detail":"node_budget"}, {"kind":"unknown","detail":"not_asserted"}
        ])
    );
}

#[test]
fn timed_out_walk_cannot_report_complete() {
    let mut budget = WalkBudget::new(100, 10);
    budget.stop_for_timeout();
    let mut state = WalkState::new(&mut budget, 25);

    walk_synthetic_tree(&star(0), 0, 0, &mut state);

    assert!(!state.complete);
    assert_eq!(state.budget.outcome().reason(), Some("timeout"));
    assert_eq!(state.budget.outcome().nodes_visited, 0);
    assert!(!state.finish());
    let mut output = serde_json::json!({});
    budget.outcome().apply(&mut output);
    assert_eq!(
        output["observation_incomplete_reasons"],
        serde_json::json!([
            {"kind":"budget","detail":"time_budget"}, {"kind":"unknown","detail":"not_asserted"}
        ])
    );
}

/// Without a query the generic walk leaves offscreen deferred rows as a
/// summary row; the element list then misses them and is not complete.
#[test]
fn unwalked_offscreen_rows_make_the_walk_incomplete() {
    for (visible, search_offscreen, complete) in [
        (vec![true, false], false, false),
        (vec![false], false, false),
        (vec![true, true], false, true),
        (vec![true, false], true, true),
        (vec![], false, true),
    ] {
        let mut budget = WalkBudget::nodes_only(10);
        let mut state = WalkState::new(&mut budget, 25);
        note_unwalked_offscreen_rows(&mut state, visible.clone(), search_offscreen);
        assert_eq!(
            state.complete, complete,
            "{visible:?} search={search_offscreen}"
        );
        assert_eq!(state.finish(), complete);
        let mut output = serde_json::json!({});
        budget.outcome().apply(&mut output);
        let expected = if complete {
            serde_json::json!([])
        } else {
            serde_json::json!([
                {"kind":"policy_omission","detail":"offscreen"}, {"kind":"unknown","detail":"not_asserted"}
            ])
        };
        assert_eq!(output["observation_incomplete_reasons"], expected);
    }
}

#[test]
fn complete_walk_reports_observation_complete() {
    let mut budget = WalkBudget::nodes_only(10);
    let mut state = WalkState::new(&mut budget, 25);
    walk_synthetic_tree(&star(3), 0, 0, &mut state);
    assert!(state.finish());
    let mut output = serde_json::json!({});
    budget.outcome().apply(&mut output);
    assert_eq!(
        output["observation_incomplete_reasons"],
        serde_json::json!([])
    );
    assert_eq!(output["observation_complete"], true);
}

#[test]
fn omissions_report_exact_reasons() {
    use cua_driver_contract::{IncompleteReasonDetail as D, IncompleteReasonKind as K};
    let cases = [
        (Omission::DepthLimit, K::Budget, D::DepthLimit, true),
        (
            Omission::CacheRequestIncomplete,
            K::ProviderFailure,
            D::AttributeReadFailed,
            true,
        ),
        (
            Omission::ChildrenUnreadable,
            K::ProviderFailure,
            D::ChildEnumerationFailed,
            true,
        ),
        (
            Omission::ProcessIdUnreadable,
            K::ProviderFailure,
            D::AttributeReadFailed,
            true,
        ),
        (
            Omission::SubtreeCacheFailed,
            K::ProviderFailure,
            D::EnumerationErrorUnclassified,
            true,
        ),
        (
            Omission::EmptyWrapperFallback,
            K::ProviderFailure,
            D::ScopeMismatch,
            true,
        ),
        (
            Omission::OffscreenRowsDeferred,
            K::PolicyOmission,
            D::Offscreen,
            true,
        ),
        (
            Omission::TabsCompacted,
            K::PolicyOmission,
            D::InactiveTab,
            true,
        ),
        (
            Omission::OffscreenContentDropped,
            K::PolicyOmission,
            D::Offscreen,
            false,
        ),
        (
            Omission::AttributeReadFailed,
            K::ProviderFailure,
            D::AttributeReadFailed,
            true,
        ),
        (
            Omission::SiblingScanCapped,
            K::PolicyOmission,
            D::HugeContainer,
            true,
        ),
    ];
    for (omission, kind, detail, flips) in cases {
        let mut budget = WalkBudget::nodes_only(10);
        let mut state = WalkState::new(&mut budget, 25);
        state.mark(omission);
        assert_eq!(state.finish(), !flips, "{omission:?}");
        let mut output = serde_json::json!({});
        budget.outcome().apply(&mut output);
        let mut expected = vec![cua_driver_contract::IncompleteReason { kind, detail }];
        if flips {
            expected.push(cua_driver_contract::IncompleteReason {
                kind: K::Unknown,
                detail: D::NotAsserted,
            });
        }
        expected.sort_unstable();
        assert_eq!(output["observation_complete"], false);
        assert_eq!(
            output["observation_incomplete_reasons"],
            serde_json::json!(expected),
            "{omission:?}"
        );
    }
}

#[test]
fn cached_read_classifiers_and_log_fail_closed() {
    for (raw, expected) in [
        (
            RawCached::Ok {
                vt: 8,
                unknown_is_not_supported_token: false,
            },
            CachedRead::Value,
        ),
        (
            RawCached::Ok {
                vt: 13,
                unknown_is_not_supported_token: true,
            },
            CachedRead::Absent,
        ),
        (
            RawCached::Ok {
                vt: 13,
                unknown_is_not_supported_token: false,
            },
            CachedRead::Failed,
        ),
        (
            RawCached::Ok {
                vt: 0,
                unknown_is_not_supported_token: false,
            },
            CachedRead::Failed,
        ),
        (
            RawCached::Ok {
                vt: 11,
                unknown_is_not_supported_token: false,
            },
            CachedRead::Failed,
        ),
        (RawCached::Err(-1), CachedRead::Failed),
    ] {
        assert_eq!(classify_cached_variant(raw, 8), expected);
    }
    for (result, expected) in [
        (Ok(()), CachedRead::Value),
        (Err(0), CachedRead::Absent),
        (Err(0x80004002u32 as i32), CachedRead::Absent),
        (Err(0x80040204u32 as i32), CachedRead::Absent),
        (Err(0x80004005u32 as i32), CachedRead::Failed),
    ] {
        assert_eq!(classify_cached_pattern(result), expected);
    }
    let mut budget = WalkBudget::nodes_only(10);
    let state = WalkState::new(&mut budget, 25);
    state.reads.fail();
    assert!(!state.finish());
    let mut output = serde_json::json!({});
    budget.outcome().apply(&mut output);
    assert_eq!(
        output["observation_incomplete_reasons"],
        serde_json::json!([{"kind":"provider_failure","detail":"attribute_read_failed"},{"kind":"unknown","detail":"not_asserted"}])
    );
}

#[test]
fn real_tab_plan_marks_only_omitted_tabs() {
    use cua_driver_core::snapshot_compaction::{compact_tab_strip, TabChildMeta};
    let mut tabs = vec![
        TabChildMeta {
            role: "TabItem",
            role_description: None,
            title: None,
            selected: Some(false),
            value: None,
            value_state: None
        };
        10
    ];
    tabs[0].selected = Some(true);
    let plan = compact_tab_strip("Tab", None, None, None, &tabs);
    assert!(plan.as_ref().is_some_and(|p| p.omitted_count > 0));
    let mut budget = WalkBudget::nodes_only(10);
    let mut state = WalkState::new(&mut budget, 25);
    note_tab_plan(&mut state, plan.as_ref());
    assert!(!state.finish());
    let mut output = serde_json::json!({});
    budget.outcome().apply(&mut output);
    assert_eq!(
        output["observation_incomplete_reasons"],
        serde_json::json!([{"kind":"policy_omission","detail":"inactive_tab"},{"kind":"unknown","detail":"not_asserted"}])
    );
    for tab in &mut tabs {
        tab.selected = Some(true);
    }
    let all = compact_tab_strip("Tab", None, None, None, &tabs);
    assert!(all.is_none());
    let mut budget = WalkBudget::nodes_only(10);
    let mut state = WalkState::new(&mut budget, 25);
    note_tab_plan(&mut state, all.as_ref());
    let zero = cua_driver_core::snapshot_compaction::TabStripCompaction {
        keep: vec![true],
        omitted_count: 0,
        first_omitted_index: 0,
    };
    note_tab_plan(&mut state, Some(&zero));
    assert!(state.finish());
}

#[test]
fn omissions_and_budget_stops_report_their_exact_union() {
    for timeout in [false, true] {
        let mut budget = WalkBudget::nodes_only(0);
        if timeout {
            budget.stop_for_timeout();
        }
        let mut state = WalkState::new(&mut budget, 0);
        state.mark(Omission::ChildrenUnreadable);
        assert!(!state.enter(0));
        assert!(!state.finish());
        let mut output = serde_json::json!({});
        budget.outcome().apply(&mut output);
        let stop = if timeout {
            "time_budget"
        } else {
            "node_budget"
        };
        assert_eq!(
            output["observation_incomplete_reasons"],
            serde_json::json!([
                {"kind":"provider_failure","detail":"child_enumeration_failed"},
                {"kind":"budget","detail":stop},
                {"kind":"unknown","detail":"not_asserted"}
            ])
        );
        assert_eq!(output["truncated"], true);
    }
    let mut budget = WalkBudget::nodes_only(10);
    let mut state = WalkState::new(&mut budget, 0);
    assert!(!state.enter(1));
    assert!(!state.finish());
    let mut output = serde_json::json!({});
    budget.outcome().apply(&mut output);
    assert_eq!(
        output["observation_incomplete_reasons"],
        serde_json::json!([
            {"kind":"budget","detail":"depth_limit"},
            {"kind":"unknown","detail":"not_asserted"}
        ])
    );
}

#[test]
fn disabled_node_value_pattern_failure_marks_read_untrusted() {
    let mut budget = WalkBudget::nodes_only(10);
    let state = WalkState::new(&mut budget, 25);
    let mut value_reads = 0;
    let mut action_reads = 0;
    let (has_value, actions): (bool, Vec<()>) = collect_pattern_evidence(
        false,
        || {
            value_reads += 1;
            record_pattern_read(Err(0x80004005u32 as i32), &state.reads) == CachedRead::Value
        },
        || {
            action_reads += 1;
            vec![()]
        },
    );
    assert!(!has_value);
    assert!(actions.is_empty());
    assert_eq!((value_reads, action_reads), (1, 0));
    assert!(!state.finish());
    let mut output = serde_json::json!({});
    budget.outcome().apply(&mut output);
    assert_eq!(
        output["observation_incomplete_reasons"],
        serde_json::json!([
            {"kind":"provider_failure","detail":"attribute_read_failed"},
            {"kind":"unknown","detail":"not_asserted"}
        ])
    );
}

#[test]
fn enabled_node_collects_value_and_actions() {
    let (value, actions) = collect_pattern_evidence(true, || true, || vec!["invoke"]);
    assert!(value);
    assert_eq!(actions, vec!["invoke"]);
}
