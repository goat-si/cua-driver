use super::*;

fn star(child_count: usize) -> Vec<Vec<usize>> {
    let mut children = vec![Vec::new(); child_count + 1];
    children[0] = (1..=child_count).collect();
    children
}

fn chain(len: usize) -> Vec<Vec<usize>> {
    let mut children = vec![Vec::new(); len];
    for i in 0..len.saturating_sub(1) {
        children[i] = vec![i + 1];
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
    }
}
