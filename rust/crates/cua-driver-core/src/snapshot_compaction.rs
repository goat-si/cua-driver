//! Cross-platform accessibility snapshot compaction, static-text summarization,
//! credential/OTP value redaction, and dense-collection row scheduling.
//!
//! Completeness-asserting platforms must implement [`CompactableNode::offscreen`],
//! use [`fold_actionless_text_subtrees_with_reasons`], and merge its reasons
//! into their walk's read-level evidence.

use crate::tree_boundary::{retain_boundary, BoundaryReason, NodeBoundary};
use std::collections::HashMap;

/// Minimum number of tab-like children in a tab strip before inactive tabs are compacted.
pub const TAB_STRIP_COMPACTION_THRESHOLD: usize = 10;

/// Maximum relative descendant depth eligible for static-text subtree folding.
pub const MAX_SUMMARY_DEPTH: usize = 3;

/// Maximum number of distinct text snippets merged into a single row/group summary.
pub const MAX_SUMMARY_SNIPPETS: usize = 6;

/// Maximum character length of a merged static-text summary.
pub const MAX_SUMMARY_CHARS: usize = 220;

/// Maximum character length of each individual snippet before joining.
pub const MAX_SNIPPET_CHARS: usize = 80;

/// Replacement string emitted for redacted secret or verification-code values.
pub const REDACTED_VALUE: &str = "[redacted]";

/// Lightweight metadata for a child of a potential browser tab strip container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabChildMeta<'a> {
    pub role: &'a str,
    pub role_description: Option<&'a str>,
    pub title: Option<&'a str>,
    pub selected: Option<bool>,
    pub value: Option<&'a str>,
    pub value_state: Option<&'a str>,
}

/// Compaction plan for a tab strip container with `>= 10` tabs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabStripCompaction {
    /// `true` for each child index that should be walked and emitted.
    pub keep: Vec<bool>,
    /// Total number of inactive tab children omitted.
    pub omitted_count: usize,
    /// Index of the first omitted tab child where the summary line should appear.
    pub first_omitted_index: usize,
}

impl TabStripCompaction {
    /// Preserve evidence from already walked omitted child subtrees before discarding them.
    /// The plan itself operates on metadata, so callers that omit before walking
    /// have no child boundary to transfer. `children` is a preorder forest with
    /// one root per plan entry, at a common depth and in plan order.
    pub fn retain_omitted_boundaries<N: CompactableNode>(
        &self,
        children: &mut [N],
        mut parent: Option<&mut N>,
    ) -> Vec<BoundaryReason> {
        let depth = children.first().map(CompactableNode::depth);
        assert!(
            children.iter().all(|node| Some(node.depth()) >= depth),
            "invalid tab child depth"
        );
        assert_eq!(
            self.keep.len(),
            children
                .iter()
                .filter(|node| Some(node.depth()) == depth)
                .count(),
            "tab plan and children must match"
        );
        let mut read_reasons = Vec::new();
        let mut root = 0;
        for child in children {
            if Some(child.depth()) == depth {
                root += 1;
            }
            if !self.keep[root - 1] {
                if let Some(boundary) = child.boundary_mut().and_then(Option::take) {
                    let slot = parent
                        .as_deref_mut()
                        .and_then(CompactableNode::boundary_mut);
                    retain_boundary(&boundary, slot, &mut read_reasons);
                }
            }
        }
        read_reasons
    }
}

/// Whether a parent node can act as a browser tab strip container.
pub fn is_tab_strip_container(
    role: &str,
    title: Option<&str>,
    description: Option<&str>,
    role_description: Option<&str>,
    tab_child_count: usize,
) -> bool {
    let role_lower = role.trim().to_ascii_lowercase();
    if matches!(
        role_lower.as_str(),
        "axtabgroup" | "tab" | "tabcontrol" | "page tab list" | "tablist"
    ) {
        return true;
    }
    let named_tab_bar = [title, description, role_description]
        .into_iter()
        .flatten()
        .any(|text| {
            let lower = text.trim().to_ascii_lowercase();
            lower == "tab bar" || lower == "tabs" || lower.contains("tab strip")
        });
    if named_tab_bar {
        return true;
    }
    matches!(
        role_lower.as_str(),
        "axgroup" | "axscrollarea" | "axtoolbar" | "group" | "pane" | "toolbar" | "panel"
    ) && tab_child_count >= TAB_STRIP_COMPACTION_THRESHOLD
}

/// Whether a child node represents a browser tab inside a tab strip.
pub fn is_tab_like_child(parent_role: &str, role: &str, role_description: Option<&str>) -> bool {
    let role_lower = role.trim().to_ascii_lowercase();
    if matches!(
        role_lower.as_str(),
        "axtab" | "tabitem" | "page tab" | "tab"
    ) {
        return true;
    }
    if role_description
        .map(|d| d.trim().eq_ignore_ascii_case("tab"))
        .unwrap_or(false)
    {
        return true;
    }
    let parent_lower = parent_role.trim().to_ascii_lowercase();
    role_lower == "axradiobutton"
        && matches!(
            parent_lower.as_str(),
            "axtabgroup" | "tab" | "tabcontrol" | "page tab list" | "tablist"
        )
}

/// Whether a tab child is currently selected/active.
pub fn is_selected_tab(
    selected: Option<bool>,
    value: Option<&str>,
    value_state: Option<&str>,
) -> bool {
    if selected == Some(true) {
        return true;
    }
    for candidate in [value, value_state].into_iter().flatten() {
        let trimmed = candidate.trim();
        if trimmed == "1"
            || trimmed.eq_ignore_ascii_case("true")
            || trimmed.eq_ignore_ascii_case("selected")
        {
            return true;
        }
    }
    false
}

/// Compute a tab-strip compaction plan when `children` contains at least
/// [`TAB_STRIP_COMPACTION_THRESHOLD`] tab-like items.
///
/// Selected tabs and non-tab controls (such as the "New Tab" button) are
/// retained. When no tab reports a selected state, the first tab is retained
/// so the strip never collapses to zero tabs.
pub fn compact_tab_strip(
    parent_role: &str,
    parent_title: Option<&str>,
    parent_description: Option<&str>,
    parent_role_description: Option<&str>,
    children: &[TabChildMeta<'_>],
) -> Option<TabStripCompaction> {
    let tab_indices: Vec<usize> = children
        .iter()
        .enumerate()
        .filter_map(|(i, child)| {
            is_tab_like_child(parent_role, child.role, child.role_description).then_some(i)
        })
        .collect();
    if tab_indices.len() < TAB_STRIP_COMPACTION_THRESHOLD {
        return None;
    }
    if !is_tab_strip_container(
        parent_role,
        parent_title,
        parent_description,
        parent_role_description,
        tab_indices.len(),
    ) {
        return None;
    }

    let mut selected_tab_indices: Vec<usize> = tab_indices
        .iter()
        .copied()
        .filter(|&i| {
            let c = &children[i];
            is_selected_tab(c.selected, c.value, c.value_state)
        })
        .collect();
    if selected_tab_indices.is_empty() {
        selected_tab_indices.push(tab_indices[0]);
    }

    let mut keep = vec![true; children.len()];
    let mut omitted_count = 0usize;
    let mut first_omitted_index = None;
    for &tab_idx in &tab_indices {
        if !selected_tab_indices.contains(&tab_idx) {
            keep[tab_idx] = false;
            omitted_count += 1;
            if first_omitted_index.is_none() {
                first_omitted_index = Some(tab_idx);
            }
        }
    }
    let first_omitted_index = first_omitted_index?;
    (omitted_count > 0).then_some(TabStripCompaction {
        keep,
        omitted_count,
        first_omitted_index,
    })
}

/// Format the summary line emitted for omitted inactive tabs.
pub fn format_omitted_tabs_line(omitted_count: usize) -> String {
    format!("- ({omitted_count} inactive tabs omitted)")
}

/// Summary line for a list whose offscreen rows were skipped without being
/// counted (Windows' native ListView viewport read).
pub fn format_uncounted_offscreen_rows_line() -> String {
    "- (offscreen rows: not read; scroll the list, or pass query to search them)".to_owned()
}

/// Whether a rendered markdown line is a synthetic compaction/offscreen summary
/// that `filter_tree` should ignore when matching queries.
pub fn is_compaction_summary_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("- (offscreen rows: ")
        || (trimmed.starts_with("- (") && trimmed.trim_end().ends_with(" inactive tabs omitted)"))
}

/// Metadata inspected to determine whether an input control holds a secret or verification code.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FieldSecurityMeta<'a> {
    pub role: &'a str,
    pub subrole: Option<&'a str>,
    pub title: Option<&'a str>,
    pub description: Option<&'a str>,
    pub placeholder: Option<&'a str>,
    pub identifier: Option<&'a str>,
    pub help: Option<&'a str>,
    pub is_password_flag: bool,
}

/// Whether `role` or `subrole` can represent a text/value input field.
pub fn should_probe_secure_text_role(role: &str, subrole: Option<&str>) -> bool {
    for candidate in [Some(role), subrole].into_iter().flatten() {
        let lower = candidate.trim().to_ascii_lowercase();
        if lower.contains("secure")
            || lower.contains("password")
            || lower.contains("text")
            || lower.contains("field")
            || lower.contains("edit")
            || lower.contains("combo")
            || lower.contains("search")
            || lower.contains("entry")
            || lower.contains("input")
        {
            return true;
        }
    }
    false
}

fn contains_security_keyword(text: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    if lower.contains("password")
        || lower.contains("passcode")
        || lower.contains("verification code")
        || lower.contains("verification-code")
        || lower.contains("verification_code")
        || lower.contains("one-time code")
        || lower.contains("one-time-code")
        || lower.contains("one_time_code")
        || lower.contains("onetimecode")
        || lower.contains("security code")
        || lower.contains("secure")
    {
        return true;
    }
    // Token-bounded match for "otp" / "totp" so substrings like "http" or "output" never false-match.
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|token| matches!(token, "otp" | "totp"))
}

/// Determine whether a field represents a password, passcode, OTP, or verification-code input.
pub fn is_secure_text_field(meta: &FieldSecurityMeta<'_>) -> bool {
    if meta.is_password_flag {
        return true;
    }
    for r in [Some(meta.role), meta.subrole].into_iter().flatten() {
        let lower = r.trim().to_ascii_lowercase();
        if lower == "axsecuretextfield"
            || lower == "passwordedit"
            || lower == "password text"
            || lower == "password"
        {
            return true;
        }
    }
    if !should_probe_secure_text_role(meta.role, meta.subrole) {
        return false;
    }
    [
        Some(meta.role),
        meta.subrole,
        meta.title,
        meta.description,
        meta.placeholder,
        meta.identifier,
        meta.help,
    ]
    .into_iter()
    .flatten()
    .any(contains_security_keyword)
}

/// Redact `value` and `value_state` to [`REDACTED_VALUE`] when `meta` identifies a secure field.
pub fn redact_if_secure(
    meta: &FieldSecurityMeta<'_>,
    value: Option<String>,
    value_state: Option<String>,
) -> (Option<String>, Option<String>) {
    if !is_secure_text_field(meta) {
        return (value, value_state);
    }
    let redacted_value = value.and_then(|v| {
        if v.trim().is_empty() {
            None
        } else {
            Some(REDACTED_VALUE.to_owned())
        }
    });
    let redacted_state = value_state.and_then(|s| {
        if s.trim().is_empty() {
            None
        } else {
            Some(REDACTED_VALUE.to_owned())
        }
    });
    (redacted_value, redacted_state)
}

/// Withhold every non-blank secure field name: substring comparisons cannot
/// prove that a label does not expose the secret. The raw value is not consulted.
pub fn redact_mirrored_secure_name(
    secure: bool,
    name: Option<String>,
    _raw_value: Option<&str>,
) -> Option<String> {
    match (secure, name) {
        (true, Some(n)) if !n.trim().is_empty() => Some(REDACTED_VALUE.to_owned()),
        (_, name) => name,
    }
}

/// Normalize whitespace and strip line breaks from a text snippet.
pub fn sanitize_snippet(text: &str, max_chars: usize) -> Option<String> {
    let collapsed: String = text
        .chars()
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\t' {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.is_empty() {
        return None;
    }
    if collapsed.chars().count() <= max_chars {
        Some(collapsed)
    } else {
        let mut truncated: String = collapsed
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect();
        truncated.push('…');
        Some(truncated)
    }
}

/// Summarize up to [`MAX_SUMMARY_SNIPPETS`] distinct static-text snippets when their
/// combined length is within [`MAX_SUMMARY_CHARS`].
pub fn summarize_static_text_snippets<S: AsRef<str>>(snippets: &[S]) -> Option<String> {
    let mut distinct: Vec<String> = Vec::new();
    for raw in snippets {
        let Some(clean) = sanitize_snippet(raw.as_ref(), MAX_SNIPPET_CHARS) else {
            continue;
        };
        if !distinct.iter().any(|existing| existing == &clean) {
            distinct.push(clean);
            if distinct.len() >= MAX_SUMMARY_SNIPPETS {
                break;
            }
        }
    }
    if distinct.is_empty() {
        return None;
    }
    let joined = distinct.join(" ");
    (joined.chars().count() <= MAX_SUMMARY_CHARS).then_some(joined)
}

/// Roles that act as rows or generic containers eligible for actionless static-text folding.
pub fn is_summarizable_container_or_row_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "axrow"
            | "axcell"
            | "axoutlinerow"
            | "axgroup"
            | "axunknown"
            | "listitem"
            | "dataitem"
            | "treeitem"
            | "group"
            | "custom"
            | "pane"
            | "table row"
            | "table cell"
            | "list item"
            | "panel"
            | "section"
    )
}

/// Roles that are pure layout/generic wrappers which can be elided when actionless and unlabelled.
pub fn is_elidable_wrapper_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "axgroup" | "axunknown" | "group" | "custom" | "pane" | "panel" | "section"
    )
}

/// Roles allowed inside an actionless static-text subtree.
pub fn is_plain_text_subtree_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "axstatictext"
            | "axlink"
            | "aximage"
            | "axgroup"
            | "axunknown"
            | "text"
            | "statictext"
            | "hyperlink"
            | "image"
            | "group"
            | "custom"
            | "label"
            | "static"
            | "link"
            | "panel"
            | "section"
    )
}

/// Roles that contribute text snippets during static-text subtree folding.
pub fn is_text_bearing_leaf_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "axstatictext"
            | "axlink"
            | "text"
            | "statictext"
            | "hyperlink"
            | "label"
            | "static"
            | "link"
    )
}

/// Minimal view of a walked node used for generic actionless subtree folding and wrapper elision.
pub trait CompactableNode {
    fn element_index(&self) -> Option<usize>;
    fn role(&self) -> &str;
    fn depth(&self) -> usize;
    fn set_depth(&mut self, depth: usize);
    fn title(&self) -> Option<&str>;
    fn value(&self) -> Option<&str>;
    fn set_value(&mut self, value: Option<String>);
    fn description(&self) -> Option<&str>;
    fn format_line(&self) -> String;
    /// Return observed visibility when available. Completeness-asserting
    /// platforms should implement this to keep differing visibility separate.
    fn offscreen(&self) -> Option<bool> {
        None
    }
    /// Return the node's boundary slot. `None` means this node type does not
    /// support boundaries; implementers must return the same slot on each call.
    /// Unavailable ancestor slots send dropped reasons to the read-level result.
    fn boundary_mut(&mut self) -> Option<&mut Option<NodeBoundary>> {
        None
    }
}

/// Fold nodes while returning read-level reasons, including any text lost to
/// folding caps. Callers must merge these reasons into read-level evidence.
pub fn fold_actionless_text_subtrees_with_reasons<N: CompactableNode>(
    lines: &mut Vec<(usize, String)>,
    nodes: &mut Vec<N>,
) -> Vec<BoundaryReason> {
    fold_actionless_text_subtrees_impl(lines, nodes, true)
}

fn fold_actionless_text_subtrees_impl<N: CompactableNode>(
    lines: &mut Vec<(usize, String)>,
    nodes: &mut Vec<N>,
    render_loss_marker: bool,
) -> Vec<BoundaryReason> {
    let mut read_reasons = Vec::new();
    if lines.len() != nodes.len() || nodes.is_empty() {
        return read_reasons;
    }

    // Pass 1: Elide empty unlabelled actionless wrapper leaves, and hoist children of
    // empty unlabelled actionless wrapper containers.
    let mut i = 0usize;
    while i < nodes.len() {
        let is_empty_wrapper = nodes[i].element_index().is_none()
            && is_elidable_wrapper_role(nodes[i].role())
            && nodes[i]
                .title()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
            && nodes[i]
                .value()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
            && nodes[i]
                .description()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true);
        if !is_empty_wrapper {
            i += 1;
            continue;
        }
        let wrapper_depth = nodes[i].depth();
        if let Some(boundary) = nodes[i].boundary_mut().and_then(Option::take) {
            let ancestor = (0..i).rev().find(|&j| nodes[j].depth() < wrapper_depth);
            let slot = ancestor.and_then(|j| nodes[j].boundary_mut());
            retain_boundary(&boundary, slot, &mut read_reasons);
        }
        let mut sub_end = i + 1;
        while sub_end < nodes.len() && nodes[sub_end].depth() > wrapper_depth {
            sub_end += 1;
        }
        for j in (i + 1)..sub_end {
            let new_d = nodes[j].depth().saturating_sub(1);
            nodes[j].set_depth(new_d);
            lines[j].0 = new_d;
        }
        nodes.remove(i);
        lines.remove(i);
    }

    // Pass 2 follows below.
    fold_after_elision(lines, nodes, &mut read_reasons, render_loss_marker);
    read_reasons
}

/// Fold actionless rows/groups whose descendants (within depth `<= 3`, `<= 6` snippets,
/// `<= 220` chars) are exclusively actionless static-text/link nodes into a single line,
/// and elide empty unlabelled wrapper nodes.
///
/// Because only `element_index() == None` nodes are folded or elided, every addressable
/// node's `element_index` and `parent_element_index` remain completely unchanged.
/// Compatibility wrapper for nodes without boundary support. Implementers of
/// `boundary_mut` must use `fold_actionless_text_subtrees_with_reasons` and
/// propagate its returned reasons to the read result.
pub fn fold_actionless_text_subtrees<N: CompactableNode>(
    lines: &mut Vec<(usize, String)>,
    nodes: &mut Vec<N>,
) {
    let _ = fold_actionless_text_subtrees_impl(lines, nodes, false);
}

fn fold_after_elision<N: CompactableNode>(
    lines: &mut Vec<(usize, String)>,
    nodes: &mut Vec<N>,
    read_reasons: &mut Vec<BoundaryReason>,
    render_loss_marker: bool,
) {
    // Pass 2: Fold actionless row/cell/group subtrees that contain only plain static text
    // within MAX_SUMMARY_DEPTH, MAX_SUMMARY_SNIPPETS, and MAX_SUMMARY_CHARS.
    let mut out_lines: Vec<(usize, String)> = Vec::with_capacity(lines.len());
    let mut out_nodes: Vec<N> = Vec::with_capacity(nodes.len());
    let old_lines: Vec<(usize, String)> = std::mem::take(lines);
    let mut old_nodes: Vec<Option<N>> = std::mem::take(nodes).into_iter().map(Some).collect();

    let mut idx = 0usize;
    while idx < old_nodes.len() {
        let node_ref = old_nodes[idx].as_ref().unwrap();
        let base_depth = node_ref.depth();
        let mut sub_end = idx + 1;
        while sub_end < old_nodes.len() && old_nodes[sub_end].as_ref().unwrap().depth() > base_depth
        {
            sub_end += 1;
        }

        let can_try_fold = node_ref.element_index().is_none()
            && is_summarizable_container_or_row_role(node_ref.role())
            && sub_end > idx + 1;

        if can_try_fold {
            let is_generic_group = is_elidable_wrapper_role(node_ref.role());
            let max_allowed_rel_depth = if is_generic_group {
                MAX_SUMMARY_DEPTH + 1
            } else {
                MAX_SUMMARY_DEPTH
            };
            let mut all_plain = true;
            let mut raw_snippets: Vec<String> = Vec::new();

            for desc_opt in &old_nodes[(idx + 1)..sub_end] {
                let desc = desc_opt.as_ref().unwrap();
                let rel_depth = desc.depth().saturating_sub(base_depth);
                if desc.element_index().is_some()
                    || rel_depth > max_allowed_rel_depth
                    || !is_plain_text_subtree_role(desc.role())
                    || matches!((node_ref.offscreen(), desc.offscreen()), (Some(a), Some(b)) if a != b)
                {
                    all_plain = false;
                    break;
                }
                if is_text_bearing_leaf_role(desc.role()) {
                    for candidate in [desc.value(), desc.title(), desc.description()]
                        .into_iter()
                        .flatten()
                    {
                        let trimmed = candidate.trim();
                        if !trimmed.is_empty() {
                            raw_snippets.push(trimmed.to_owned());
                            break;
                        }
                    }
                }
            }

            let min_snippets = if is_generic_group { 2 } else { 1 };
            if all_plain && raw_snippets.len() >= min_snippets {
                if let Some(summary) = summarize_static_text_snippets(&raw_snippets) {
                    let mut parent_node = old_nodes[idx].take().unwrap();
                    let mut distinct = Vec::new();
                    let mut lost_text = false;
                    for raw in &raw_snippets {
                        if let Some(full) = sanitize_snippet(raw, usize::MAX) {
                            lost_text |= full.chars().count() > MAX_SNIPPET_CHARS;
                            if !distinct.contains(&full) {
                                distinct.push(full);
                            }
                        }
                    }
                    lost_text |= distinct.len() > MAX_SUMMARY_SNIPPETS;
                    for descendant in &mut old_nodes[(idx + 1)..sub_end] {
                        if let Some(boundary) = descendant
                            .as_mut()
                            .and_then(CompactableNode::boundary_mut)
                            .and_then(Option::take)
                        {
                            retain_boundary(&boundary, parent_node.boundary_mut(), read_reasons);
                        }
                    }
                    let existing_title = parent_node.title().map(str::trim).unwrap_or("");
                    if existing_title != summary {
                        let merged_val = match parent_node.value().map(str::trim) {
                            Some(v) if !v.is_empty() && v != summary => {
                                let combined = format!("{v} {summary}");
                                if combined.chars().count() <= MAX_SUMMARY_CHARS {
                                    combined
                                } else {
                                    lost_text = true;
                                    summary
                                }
                            }
                            _ => summary,
                        };
                        parent_node.set_value(Some(merged_val));
                    }
                    if lost_text && render_loss_marker {
                        let reason = BoundaryReason {
                            kind: cua_driver_contract::IncompleteReasonKind::PolicyOmission,
                            detail:
                                cua_driver_contract::IncompleteReasonDetail::FoldedTextTruncated,
                        };
                        let dropped = NodeBoundary {
                            reasons: vec![reason],
                            children_skipped: crate::tree_boundary::CountEvidence::Unknown,
                            descendants_omitted: Some(true),
                        };
                        retain_boundary(&dropped, parent_node.boundary_mut(), read_reasons);
                        if !read_reasons.contains(&reason) {
                            read_reasons.push(reason);
                            read_reasons.sort_unstable();
                        }
                    }
                    let mut formatted = parent_node.format_line();
                    if lost_text && render_loss_marker {
                        if let Some(boundary) =
                            parent_node.boundary_mut().and_then(|slot| slot.as_ref())
                        {
                            let value =
                                serde_json::to_value(boundary).expect("boundary serializes");
                            formatted.push(' ');
                            formatted.push_str(&crate::window_state_view::boundary_marker(&value));
                        }
                    }
                    out_lines.push((base_depth, formatted));
                    out_nodes.push(parent_node);
                    idx = sub_end;
                    continue;
                }
            }
        }

        out_lines.push(old_lines[idx].clone());
        out_nodes.push(old_nodes[idx].take().unwrap());
        idx += 1;
    }

    *lines = out_lines;
    *nodes = out_nodes;
}

/// Whether a Windows UIA control type must be preserved even when `CachedIsOffscreen` is true.
pub fn is_uia_offscreen_exempt(control_type: &str) -> bool {
    matches!(
        control_type.trim().to_ascii_lowercase().as_str(),
        "edit" | "editcontrol" | "document" | "documentcontrol"
    )
}

/// Whether a Windows UIA control type is a dense collection whose offscreen rows should yield
/// to visible rows and sibling controls.
pub fn is_uia_dense_collection(control_type: &str) -> bool {
    matches!(
        control_type.trim().to_ascii_lowercase().as_str(),
        "list" | "datagrid" | "table" | "tree"
    )
}

/// Whether a Windows UIA control type is a collection row item.
pub fn is_uia_row_control(control_type: &str) -> bool {
    matches!(
        control_type.trim().to_ascii_lowercase().as_str(),
        "listitem" | "dataitem" | "treeitem"
    )
}

/// Scheduling decision for each child of a dense UIA collection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiaChildPlan {
    /// Walk immediately (non-row controls such as sibling `Edit`/`Document`, headers, scrollbars).
    WalkNow,
    /// Defer row traversal until after `WalkNow` siblings; `visible` rows run before offscreen rows.
    Defer { visible: bool },
}

/// Lightweight metadata for planning children of a UIA container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiaChildCandidate<'a> {
    pub control_type: &'a str,
    pub is_offscreen: Option<bool>,
    pub rect: Option<(i32, i32, i32, i32)>,
}

fn rects_intersect(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> bool {
    a.0 < b.2 && a.2 > b.0 && a.1 < b.3 && a.3 > b.1
}

/// Plan child traversal order inside a dense UIA collection (`List`, `DataGrid`, `Table`, `Tree`).
pub fn plan_uia_collection_children(
    parent_rect: Option<(i32, i32, i32, i32)>,
    children: &[UiaChildCandidate<'_>],
) -> Vec<UiaChildPlan> {
    let mut plans: Vec<UiaChildPlan> = children
        .iter()
        .map(|child| {
            if is_uia_offscreen_exempt(child.control_type)
                || !is_uia_row_control(child.control_type)
            {
                return UiaChildPlan::WalkNow;
            }
            let offscreen = child.is_offscreen == Some(true);
            let in_parent_bounds = match (parent_rect, child.rect) {
                (Some(p), Some(r)) => rects_intersect(p, r),
                _ => true,
            };
            UiaChildPlan::Defer {
                visible: !offscreen && in_parent_bounds,
            }
        })
        .collect();

    let any_visible_row = plans
        .iter()
        .any(|p| matches!(p, UiaChildPlan::Defer { visible: true }));
    if !any_visible_row {
        let mut promoted = 0usize;
        for plan in &mut plans {
            if let UiaChildPlan::Defer { visible } = plan {
                if promoted < 20 {
                    *visible = true;
                    promoted += 1;
                }
            }
        }
    }
    plans
}

/// A deferred row chunk ready to be spliced back into its document-order anchor.
pub struct DeferredChunk<N> {
    pub anchor: usize,
    pub seq: usize,
    pub depth: usize,
    pub parent_index: Option<usize>,
    pub visible: bool,
    pub walked: Option<WalkedRows<N>>,
}

/// The rows and nodes a deferred chunk's walk produced.
pub type WalkedRows<N> = (Vec<(usize, String)>, Vec<N>);

/// Spliced rows and nodes, plus each output row's position in the node list
/// (`None` for a synthetic summary row).
pub type SplicedTree<N> = (Vec<(usize, String)>, Vec<N>, Vec<Option<usize>>);

/// Splice deferred row chunks back into `(lines, nodes)` at their original document-order anchors,
/// emitting `- (offscreen rows: N; scroll the list, or pass query to search them)` for unwalked
/// offscreen runs.
pub fn splice_deferred_chunks<N>(
    lines: Vec<(usize, String)>,
    nodes: Vec<N>,
    mut deferred: Vec<DeferredChunk<N>>,
) -> SplicedTree<N> {
    debug_assert_eq!(lines.len(), nodes.len());
    deferred.sort_by_key(|row| (row.anchor, row.seq));
    let mut rows = deferred.into_iter().peekable();
    let mut out_lines = Vec::new();
    let mut out_nodes = Vec::new();
    let mut map = Vec::new();
    let mut original = lines.into_iter().zip(nodes);

    for anchor in 0..=original.len() {
        while rows.peek().is_some_and(|row| row.anchor == anchor) {
            let mut row = rows.next().unwrap();
            if let Some((chunk_lines, chunk_nodes)) = row.walked.take() {
                debug_assert_eq!(chunk_lines.len(), chunk_nodes.len());
                map.extend((0..chunk_nodes.len()).map(|i| Some(out_nodes.len() + i)));
                out_lines.extend(chunk_lines);
                out_nodes.extend(chunk_nodes);
            } else if !row.visible {
                let mut count = 1usize;
                while rows.peek().is_some_and(|next| {
                    next.anchor == anchor
                        && next.depth == row.depth
                        && next.parent_index == row.parent_index
                        && !next.visible
                        && next.walked.is_none()
                }) {
                    rows.next();
                    count += 1;
                }
                out_lines.push((
                    row.depth,
                    format!(
                        "- (offscreen rows: {count}; scroll the list, or pass query to search them)"
                    ),
                ));
                map.push(None);
            }
        }
        if let Some((line, node)) = original.next() {
            map.push(Some(out_nodes.len()));
            out_lines.push(line);
            out_nodes.push(node);
        }
    }
    (out_lines, out_nodes, map)
}

/// Trait for nodes that carry `element_index` and `parent_element_index` for document-order renumbering.
pub trait RenumberableNode {
    fn element_index(&self) -> Option<usize>;
    fn set_element_index(&mut self, index: Option<usize>);
    fn parent_element_index(&self) -> Option<usize>;
    fn set_parent_element_index(&mut self, parent: Option<usize>);
}

/// Renumber `nodes` and `lines` into contiguous `0..K` document order after deferred splicing.
pub fn renumber_indexed_tree<N: RenumberableNode>(
    lines: &mut [(usize, String)],
    nodes: &mut [N],
    map: &[Option<usize>],
) {
    let indices: HashMap<usize, usize> = nodes
        .iter()
        .filter_map(|node| node.element_index())
        .enumerate()
        .map(|(new, old)| (old, new))
        .collect();
    for node in nodes.iter_mut() {
        node.set_element_index(
            node.element_index()
                .and_then(|old| indices.get(&old).copied()),
        );
        node.set_parent_element_index(
            node.parent_element_index()
                .and_then(|old| indices.get(&old).copied()),
        );
    }
    for ((_, line), node_index) in lines.iter_mut().zip(map) {
        if let Some(new) = node_index
            .and_then(|i| nodes.get(i))
            .and_then(|n| n.element_index())
        {
            if let Some((_, tail)) = line.strip_prefix("- [").and_then(|s| s.split_once("] ")) {
                *line = format!("- [{new}] {tail}");
            }
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn description_only_one_time_code_field_is_secure() {
        let meta = FieldSecurityMeta {
            role: "Edit",
            title: Some("DUMMYOTP-482913"),
            description: Some("One-time code"),
            ..Default::default()
        };
        assert!(is_secure_text_field(&meta));
        let (value, _) = redact_if_secure(&meta, Some("DUMMYOTP-482913".into()), None);
        assert_eq!(value.as_deref(), Some(REDACTED_VALUE));
    }

    #[test]
    fn secure_names_are_always_withheld_and_non_secure_names_survive() {
        assert_eq!(
            redact_mirrored_secure_name(
                true,
                Some("DUMMYOTP-482913".into()),
                Some("DUMMYOTP-482913")
            )
            .as_deref(),
            Some(REDACTED_VALUE)
        );
        assert_eq!(
            redact_mirrored_secure_name(true, Some("Verification code".into()), Some("123456"))
                .as_deref(),
            Some(REDACTED_VALUE)
        );
        assert_eq!(
            redact_mirrored_secure_name(false, Some("PLAINVALUE-77".into()), Some("PLAINVALUE-77"))
                .as_deref(),
            Some("PLAINVALUE-77")
        );
        assert_eq!(redact_mirrored_secure_name(true, None, Some("x")), None);
        assert_eq!(
            redact_mirrored_secure_name(true, Some(" \t".into()), Some("x")).as_deref(),
            Some(" \t")
        );
    }

    #[test]
    fn unknown_value_withholds_secure_name() {
        assert_eq!(
            redact_mirrored_secure_name(true, Some("Password".into()), None).as_deref(),
            Some(REDACTED_VALUE)
        );
    }

    #[test]
    fn decorated_partial_mirror_is_withheld() {
        assert_eq!(
            redact_mirrored_secure_name(true, Some("PIN ending 4821".into()), Some("98764821"))
                .as_deref(),
            Some(REDACTED_VALUE)
        );
    }

    #[test]
    fn reformatted_mirror_is_withheld() {
        assert_eq!(
            redact_mirrored_secure_name(true, Some("123 456".into()), Some("123456")).as_deref(),
            Some(REDACTED_VALUE)
        );
    }
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct MockNode {
        element_index: Option<usize>,
        parent_element_index: Option<usize>,
        role: String,
        depth: usize,
        title: Option<String>,
        value: Option<String>,
        description: Option<String>,
    }

    impl CompactableNode for MockNode {
        fn element_index(&self) -> Option<usize> {
            self.element_index
        }
        fn role(&self) -> &str {
            &self.role
        }
        fn depth(&self) -> usize {
            self.depth
        }
        fn set_depth(&mut self, depth: usize) {
            self.depth = depth;
        }
        fn title(&self) -> Option<&str> {
            self.title.as_deref()
        }
        fn value(&self) -> Option<&str> {
            self.value.as_deref()
        }
        fn set_value(&mut self, value: Option<String>) {
            self.value = value;
        }
        fn description(&self) -> Option<&str> {
            self.description.as_deref()
        }
        fn format_line(&self) -> String {
            let mut s = if let Some(idx) = self.element_index {
                format!("- [{idx}] {}", self.role)
            } else {
                format!("- {}", self.role)
            };
            if let Some(t) = &self.title {
                s.push_str(&format!(r#" "{t}""#));
            }
            if let Some(v) = &self.value {
                s.push_str(&format!(r#" = "{v}""#));
            }
            s
        }
    }

    impl RenumberableNode for MockNode {
        fn element_index(&self) -> Option<usize> {
            self.element_index
        }
        fn set_element_index(&mut self, index: Option<usize>) {
            self.element_index = index;
        }
        fn parent_element_index(&self) -> Option<usize> {
            self.parent_element_index
        }
        fn set_parent_element_index(&mut self, parent: Option<usize>) {
            self.parent_element_index = parent;
        }
    }

    #[derive(Clone)]
    struct BoundaryNode {
        node: MockNode,
        boundary: Option<NodeBoundary>,
        offscreen: Option<bool>,
    }
    impl CompactableNode for BoundaryNode {
        fn element_index(&self) -> Option<usize> {
            self.node.element_index
        }
        fn role(&self) -> &str {
            &self.node.role
        }
        fn depth(&self) -> usize {
            self.node.depth
        }
        fn set_depth(&mut self, depth: usize) {
            self.node.depth = depth;
        }
        fn title(&self) -> Option<&str> {
            self.node.title.as_deref()
        }
        fn value(&self) -> Option<&str> {
            self.node.value.as_deref()
        }
        fn set_value(&mut self, value: Option<String>) {
            self.node.value = value;
        }
        fn description(&self) -> Option<&str> {
            self.node.description.as_deref()
        }
        fn format_line(&self) -> String {
            self.node.format_line()
        }
        fn offscreen(&self) -> Option<bool> {
            self.offscreen
        }
        fn boundary_mut(&mut self) -> Option<&mut Option<NodeBoundary>> {
            Some(&mut self.boundary)
        }
    }
    fn u20_node(
        role: &str,
        depth: usize,
        index: Option<usize>,
        text: Option<&str>,
    ) -> BoundaryNode {
        BoundaryNode {
            node: MockNode {
                element_index: index,
                parent_element_index: None,
                role: role.into(),
                depth,
                title: None,
                value: text.map(str::to_owned),
                description: None,
            },
            boundary: None,
            offscreen: None,
        }
    }
    fn u20_boundary(
        detail: cua_driver_contract::IncompleteReasonDetail,
        count: u32,
    ) -> NodeBoundary {
        NodeBoundary {
            reasons: vec![BoundaryReason {
                kind: cua_driver_contract::IncompleteReasonKind::Budget,
                detail,
            }],
            children_skipped: crate::tree_boundary::CountEvidence::Known(count),
            descendants_omitted: None,
        }
    }
    fn u20_fold(nodes: &mut Vec<BoundaryNode>) -> Vec<BoundaryReason> {
        let mut lines = nodes.iter().map(|n| (n.depth(), n.format_line())).collect();
        fold_actionless_text_subtrees_with_reasons(&mut lines, nodes)
    }
    #[test]
    fn u20b_folding_reports_snippet_loss_and_preserves_boundary_count() {
        let mut nodes = vec![u20_node("AXRow", 0, None, None)];
        for n in 0..7 {
            nodes.push(u20_node(
                "AXStaticText",
                1,
                None,
                Some(&format!("item {n}")),
            ));
        }
        nodes[0].boundary = Some(u20_boundary(
            cua_driver_contract::IncompleteReasonDetail::NodeBudget,
            3,
        ));
        let mut lines = nodes.iter().map(|n| (n.depth(), n.format_line())).collect();
        let reasons = fold_actionless_text_subtrees_with_reasons(&mut lines, &mut nodes);
        assert_eq!(
            serde_json::to_value(&reasons).unwrap(),
            serde_json::json!([{"kind":"policy_omission","detail":"folded_text_truncated"}])
        );
        let boundary = nodes[0].boundary.as_ref().unwrap();
        assert_eq!(
            boundary.children_skipped,
            crate::tree_boundary::CountEvidence::Known(3)
        );
        assert_eq!(boundary.descendants_omitted, Some(true));
        assert_eq!(boundary.reasons.len(), 2);
        assert!(!nodes[0].value().unwrap().contains("item 6"));
        assert!(lines[0].1.contains("folded_text_truncated"));
        assert!(lines[0].1.contains("descendants omitted"));
    }
    #[test]
    fn u20b_long_snippet_reports_loss_but_six_short_snippets_do_not() {
        let long = "x".repeat(81);
        let mut nodes = vec![
            u20_node("AXRow", 0, None, None),
            u20_node("AXStaticText", 1, None, Some(&long)),
        ];
        let reasons = u20_fold(&mut nodes);
        assert_eq!(
            serde_json::to_value(reasons).unwrap(),
            serde_json::json!([{"kind":"policy_omission","detail":"folded_text_truncated"}])
        );
        let mut nodes = vec![u20_node("AXRow", 0, None, None)];
        for n in 0..6 {
            nodes.push(u20_node(
                "AXStaticText",
                1,
                None,
                Some(&format!("item {n}")),
            ));
        }
        assert!(u20_fold(&mut nodes).is_empty());
        assert!(nodes[0].boundary.is_none());
    }
    #[test]
    fn u20b_visibility_mismatch_is_retained_and_unknown_still_folds() {
        for (parent, child, expected) in [
            (Some(false), Some(true), 2),
            (Some(true), Some(false), 2),
            (None, Some(true), 1),
            (Some(false), None, 1),
            (Some(true), Some(true), 1),
        ] {
            let mut nodes = vec![
                u20_node("AXRow", 0, None, None),
                u20_node("AXStaticText", 1, None, Some("text")),
            ];
            nodes[0].offscreen = parent;
            nodes[1].offscreen = child;
            assert!(u20_fold(&mut nodes).is_empty());
            assert_eq!(nodes.len(), expected, "{parent:?}, {child:?}");
        }
    }
    #[test]
    fn u20b_duplicate_snippets_are_not_loss_and_oversize_parent_merge_is() {
        let mut nodes = vec![u20_node("AXRow", 0, None, None)];
        for _ in 0..7 {
            nodes.push(u20_node("AXStaticText", 1, None, Some("same")));
        }
        assert!(u20_fold(&mut nodes).is_empty());
        let existing = "p".repeat(200);
        let child = "c".repeat(30);
        let mut nodes = vec![
            u20_node("AXRow", 0, None, Some(&existing)),
            u20_node("AXStaticText", 1, None, Some(&child)),
        ];
        assert_eq!(
            serde_json::to_value(u20_fold(&mut nodes)).unwrap(),
            serde_json::json!([{"kind":"policy_omission","detail":"folded_text_truncated"}])
        );
    }
    #[test]
    fn u20b_legacy_wrapper_discards_reasons() {
        let mut nodes = vec![u20_node("AXRow", 0, None, None).node];
        for n in 0..7 {
            nodes.push(u20_node("AXStaticText", 1, None, Some(&format!("item {n}"))).node);
        }
        let mut lines = nodes.iter().map(|n| (n.depth(), n.format_line())).collect();
        fold_actionless_text_subtrees(&mut lines, &mut nodes);
        assert_eq!(nodes.len(), 1);
        assert_eq!(
            nodes[0].value.as_deref(),
            Some("item 0 item 1 item 2 item 3 item 4 item 5")
        );
        let mut nodes = vec![u20_node("AXRow", 0, None, None)];
        for n in 0..7 {
            nodes.push(u20_node(
                "AXStaticText",
                1,
                None,
                Some(&format!("item {n}")),
            ));
        }
        let mut lines = nodes.iter().map(|n| (n.depth(), n.format_line())).collect();
        fold_actionless_text_subtrees(&mut lines, &mut nodes);
        assert!(nodes[0].boundary.is_none());
        assert!(!lines[0].1.contains("folded_text_truncated"));
    }
    #[test]
    fn u20_elided_wrapper_boundary_reaches_nearest_retained_ancestor() {
        use cua_driver_contract::IncompleteReasonDetail as D;
        let mut nodes = vec![
            u20_node("AXWindow", 0, Some(0), None),
            u20_node("AXGroup", 1, None, None),
            u20_node("AXButton", 2, Some(1), None),
        ];
        nodes[0].boundary = Some(u20_boundary(D::NodeBudget, 2));
        nodes[1].boundary = Some(u20_boundary(D::TimeBudget, 9));
        assert!(u20_fold(&mut nodes).is_empty());
        let boundary = nodes[0].boundary.as_ref().unwrap();
        assert_eq!(boundary.reasons.len(), 2);
        assert_eq!(
            boundary.children_skipped,
            crate::tree_boundary::CountEvidence::Known(2)
        );
        assert_eq!(boundary.descendants_omitted, Some(true));
    }
    #[test]
    fn u20_wrapper_uses_nearest_ancestor_not_the_root() {
        use cua_driver_contract::IncompleteReasonDetail as D;
        let mut nodes = vec![
            u20_node("AXWindow", 0, Some(0), None),
            u20_node("AXButton", 1, Some(1), None),
            u20_node("AXGroup", 2, None, None),
            u20_node("AXButton", 3, Some(2), None),
        ];
        nodes[2].boundary = Some(u20_boundary(D::TimeBudget, 9));
        assert!(u20_fold(&mut nodes).is_empty());
        assert!(nodes[0].boundary.is_none());
        assert_eq!(
            nodes[1].boundary.as_ref().unwrap().reasons[0].detail,
            D::TimeBudget
        );
    }
    #[test]
    fn u20_folded_descendants_boundary_reaches_folding_node() {
        use cua_driver_contract::IncompleteReasonDetail as D;
        let mut nodes = vec![
            u20_node("AXRow", 0, None, None),
            u20_node("AXStaticText", 1, None, Some("hello")),
        ];
        nodes[0].boundary = Some(u20_boundary(D::NodeBudget, 2));
        nodes[1].boundary = Some(u20_boundary(D::TimeBudget, 9));
        assert!(u20_fold(&mut nodes).is_empty());
        assert_eq!(nodes.len(), 1);
        let boundary = nodes[0].boundary.as_ref().unwrap();
        assert_eq!(boundary.reasons.len(), 2);
        assert_eq!(
            boundary.children_skipped,
            crate::tree_boundary::CountEvidence::Known(2)
        );
        assert_eq!(boundary.descendants_omitted, Some(true));
    }
    #[test]
    fn u20_dropped_root_boundary_becomes_read_reason() {
        use cua_driver_contract::IncompleteReasonDetail as D;
        let mut nodes = vec![u20_node("AXGroup", 0, None, None)];
        nodes[0].boundary = Some(u20_boundary(D::NodeBudget, 2));
        assert_eq!(u20_fold(&mut nodes).len(), 1);
        assert!(nodes.is_empty());
    }
    #[test]
    fn u20_tab_plan_transfers_omitted_child_boundary() {
        use cua_driver_contract::IncompleteReasonDetail as D;
        let plan = TabStripCompaction {
            keep: vec![true, false],
            omitted_count: 1,
            first_omitted_index: 1,
        };
        let mut parent = u20_node("AXTabGroup", 0, Some(0), None);
        parent.boundary = Some(u20_boundary(D::NodeBudget, 2));
        let mut children = vec![
            u20_node("AXTab", 1, Some(1), None),
            u20_node("AXTab", 1, Some(2), None),
        ];
        children[1].boundary = Some(u20_boundary(D::TimeBudget, 9));
        assert!(plan
            .retain_omitted_boundaries(&mut children, Some(&mut parent))
            .is_empty());
        let boundary = parent.boundary.unwrap();
        assert_eq!(boundary.reasons.len(), 2);
        assert_eq!(
            boundary.children_skipped,
            crate::tree_boundary::CountEvidence::Known(2)
        );
        assert_eq!(boundary.descendants_omitted, Some(true));
    }
    #[test]
    fn u20_tab_plan_transfers_omitted_descendant_boundary() {
        use cua_driver_contract::IncompleteReasonDetail as D;
        let plan = TabStripCompaction {
            keep: vec![true, false],
            omitted_count: 1,
            first_omitted_index: 1,
        };
        let mut parent = u20_node("AXTabGroup", 0, Some(0), None);
        let mut children = vec![
            u20_node("AXTab", 1, Some(1), None),
            u20_node("AXTab", 1, Some(2), None),
            u20_node("AXStaticText", 2, None, Some("hidden")),
        ];
        children[2].boundary = Some(u20_boundary(D::TimeBudget, 9));
        assert!(plan
            .retain_omitted_boundaries(&mut children, Some(&mut parent))
            .is_empty());
        assert_eq!(parent.boundary.unwrap().reasons.len(), 1);
    }

    #[test]
    fn tab_strip_compaction_retains_selected_and_non_tab_controls_when_at_least_ten_tabs() {
        let mut children: Vec<TabChildMeta<'_>> = (0..12)
            .map(|i| TabChildMeta {
                role: "AXTab",
                role_description: Some("tab"),
                title: Some("Tab"),
                selected: Some(i == 4),
                value: None,
                value_state: None,
            })
            .collect();
        children.push(TabChildMeta {
            role: "AXButton",
            role_description: Some("button"),
            title: Some("New Tab"),
            selected: None,
            value: None,
            value_state: None,
        });

        let plan = compact_tab_strip("AXTabGroup", Some("Tab Bar"), None, None, &children)
            .expect("12 tabs must compact");
        assert_eq!(plan.omitted_count, 11);
        assert_eq!(plan.first_omitted_index, 0);
        assert!(plan.keep[4], "selected tab must be retained");
        assert!(plan.keep[12], "non-tab control must be retained");
        assert!(
            !plan.keep[0] && !plan.keep[11],
            "inactive tabs must be omitted"
        );
        assert!(is_compaction_summary_line(&format_omitted_tabs_line(
            plan.omitted_count
        )));
    }

    #[test]
    fn the_windows_list_view_offscreen_line_is_a_summary_line() {
        // The line Windows' native ListView walk emits for rows it skipped.
        // A query must not match it as if it were an element.
        assert!(is_compaction_summary_line(&format!(
            "    {}",
            format_uncounted_offscreen_rows_line()
        )));
    }

    #[test]
    fn tab_strip_compaction_falls_back_to_first_tab_when_none_selected() {
        let children: Vec<TabChildMeta<'_>> = (0..10)
            .map(|_| TabChildMeta {
                role: "TabItem",
                role_description: None,
                title: Some("Unselected"),
                selected: Some(false),
                value: None,
                value_state: None,
            })
            .collect();
        let plan = compact_tab_strip("Tab", None, None, None, &children)
            .expect("10 tabs must compact with first-tab fallback");
        assert_eq!(plan.omitted_count, 9);
        assert!(plan.keep[0]);
        assert_eq!(plan.first_omitted_index, 1);
    }

    #[test]
    fn tab_strip_below_threshold_is_not_compacted() {
        let children: Vec<TabChildMeta<'_>> = (0..9)
            .map(|i| TabChildMeta {
                role: "AXTab",
                role_description: Some("tab"),
                title: Some("Tab"),
                selected: Some(i == 0),
                value: None,
                value_state: None,
            })
            .collect();
        assert!(compact_tab_strip("AXTabGroup", None, None, None, &children).is_none());
    }

    #[test]
    fn actionless_static_text_row_and_group_fold_into_single_line_within_bounds() {
        let mut nodes = vec![
            MockNode {
                element_index: Some(0),
                parent_element_index: None,
                role: "AXWindow".into(),
                depth: 0,
                title: Some("Chat".into()),
                value: None,
                description: None,
            },
            MockNode {
                element_index: None,
                parent_element_index: Some(0),
                role: "AXGroup".into(),
                depth: 1,
                title: None,
                value: None,
                description: None,
            },
            MockNode {
                element_index: None,
                parent_element_index: Some(0),
                role: "AXRow".into(),
                depth: 2,
                title: None,
                value: None,
                description: None,
            },
            MockNode {
                element_index: None,
                parent_element_index: Some(0),
                role: "AXStaticText".into(),
                depth: 3,
                title: None,
                value: Some("Alice".into()),
                description: None,
            },
            MockNode {
                element_index: None,
                parent_element_index: Some(0),
                role: "AXStaticText".into(),
                depth: 3,
                title: None,
                value: Some("Hello world".into()),
                description: None,
            },
            MockNode {
                element_index: None,
                parent_element_index: Some(0),
                role: "AXStaticText".into(),
                depth: 3,
                title: None,
                value: Some("10:42 AM".into()),
                description: None,
            },
            MockNode {
                element_index: Some(1),
                parent_element_index: Some(0),
                role: "AXButton".into(),
                depth: 1,
                title: Some("Send".into()),
                value: None,
                description: None,
            },
        ];
        let mut lines: Vec<(usize, String)> =
            nodes.iter().map(|n| (n.depth, n.format_line())).collect();

        fold_actionless_text_subtrees(&mut lines, &mut nodes);

        assert_eq!(nodes.len(), 3);
        assert_eq!(lines.len(), 3);
        assert_eq!(nodes[1].role, "AXRow");
        assert_eq!(nodes[1].depth, 1);
        assert_eq!(
            nodes[1].value.as_deref(),
            Some("Alice Hello world 10:42 AM")
        );
        assert_eq!(
            lines[1],
            (1, r#"- AXRow = "Alice Hello world 10:42 AM""#.into())
        );
        assert_eq!(nodes[2].element_index, Some(1));
        assert_eq!(nodes[2].title.as_deref(), Some("Send"));
    }

    #[test]
    fn actionable_or_deep_or_long_subtrees_are_never_folded() {
        let mut nodes = vec![
            MockNode {
                element_index: None,
                parent_element_index: None,
                role: "AXRow".into(),
                depth: 0,
                title: Some("Row".into()),
                value: None,
                description: None,
            },
            MockNode {
                element_index: None,
                parent_element_index: None,
                role: "AXStaticText".into(),
                depth: 1,
                title: None,
                value: Some("Label".into()),
                description: None,
            },
            MockNode {
                element_index: Some(0),
                parent_element_index: None,
                role: "AXButton".into(),
                depth: 1,
                title: Some("Reply".into()),
                value: None,
                description: None,
            },
        ];
        let mut lines: Vec<(usize, String)> =
            nodes.iter().map(|n| (n.depth, n.format_line())).collect();
        fold_actionless_text_subtrees(&mut lines, &mut nodes);
        assert_eq!(
            nodes.len(),
            3,
            "row containing an actionable child must not fold"
        );
    }

    #[test]
    fn secure_text_field_redacts_password_passcode_otp_and_verification_code_values() {
        let cases = [
            FieldSecurityMeta {
                role: "AXSecureTextField",
                ..Default::default()
            },
            FieldSecurityMeta {
                role: "AXTextField",
                subrole: Some("AXSecureTextField"),
                ..Default::default()
            },
            FieldSecurityMeta {
                role: "AXTextField",
                title: Some("Account Password"),
                ..Default::default()
            },
            FieldSecurityMeta {
                role: "Edit",
                placeholder: Some("Enter 6-digit verification code"),
                ..Default::default()
            },
            FieldSecurityMeta {
                role: "AXTextField",
                description: Some("One-Time Code"),
                ..Default::default()
            },
            FieldSecurityMeta {
                role: "Edit",
                identifier: Some("login_otp_input"),
                ..Default::default()
            },
            FieldSecurityMeta {
                role: "AXTextField",
                help: Some("Enter your passcode"),
                ..Default::default()
            },
        ];

        for meta in &cases {
            assert!(
                is_secure_text_field(meta),
                "must detect secure field: {meta:?}"
            );
            let (val, state) = redact_if_secure(
                meta,
                Some("my-secret-123456".into()),
                Some("my-secret-123456".into()),
            );
            assert_eq!(val.as_deref(), Some(REDACTED_VALUE));
            assert_eq!(state.as_deref(), Some(REDACTED_VALUE));
        }
    }

    #[test]
    fn ordinary_fields_with_http_or_output_substrings_are_not_false_positive_redacted() {
        let ordinary = [
            FieldSecurityMeta {
                role: "AXTextField",
                title: Some("Server URL (http://localhost)"),
                description: Some("Output path"),
                identifier: Some("http_output_field"),
                ..Default::default()
            },
            FieldSecurityMeta {
                role: "AXButton",
                title: Some("Reset Password"),
                ..Default::default()
            },
        ];
        for meta in &ordinary {
            assert!(
                !is_secure_text_field(meta),
                "must not false-match: {meta:?}"
            );
            let (val, state) = redact_if_secure(
                meta,
                Some("visible-text".into()),
                Some("visible-text".into()),
            );
            assert_eq!(val.as_deref(), Some("visible-text"));
            assert_eq!(state.as_deref(), Some("visible-text"));
        }
    }

    #[test]
    fn uia_dense_collection_prioritizes_visible_rows_and_sibling_edit_controls_and_renumbers() {
        assert!(is_uia_offscreen_exempt("Edit"));
        assert!(is_uia_offscreen_exempt("Document"));
        assert!(!is_uia_offscreen_exempt("ListItem"));

        let parent_rect = Some((0, 0, 400, 300));
        let children = vec![
            UiaChildCandidate {
                control_type: "ListItem",
                is_offscreen: Some(true),
                rect: Some((0, -200, 400, -150)),
            },
            UiaChildCandidate {
                control_type: "ListItem",
                is_offscreen: Some(false),
                rect: Some((0, 50, 400, 100)),
            },
            UiaChildCandidate {
                control_type: "Edit",
                is_offscreen: Some(true),
                rect: Some((0, 320, 400, 360)),
            },
        ];
        let plans = plan_uia_collection_children(parent_rect, &children);
        assert_eq!(
            plans,
            vec![
                UiaChildPlan::Defer { visible: false },
                UiaChildPlan::Defer { visible: true },
                UiaChildPlan::WalkNow,
            ]
        );

        let base_nodes = vec![
            MockNode {
                element_index: Some(0),
                parent_element_index: None,
                role: "Window".into(),
                depth: 0,
                title: Some("Chat".into()),
                value: None,
                description: None,
            },
            MockNode {
                element_index: Some(1),
                parent_element_index: Some(0),
                role: "List".into(),
                depth: 1,
                title: Some("Messages".into()),
                value: None,
                description: None,
            },
            MockNode {
                element_index: Some(2),
                parent_element_index: Some(0),
                role: "Edit".into(),
                depth: 1,
                title: Some("Type a message".into()),
                value: None,
                description: None,
            },
        ];
        let base_lines: Vec<(usize, String)> = base_nodes
            .iter()
            .map(|n| (n.depth, n.format_line()))
            .collect();

        let visible_row_node = MockNode {
            element_index: Some(3),
            parent_element_index: Some(1),
            role: "ListItem".into(),
            depth: 2,
            title: Some("Visible message".into()),
            value: None,
            description: None,
        };
        let visible_row_line = (2, visible_row_node.format_line());

        let deferred = vec![
            DeferredChunk {
                anchor: 2,
                seq: 0,
                depth: 2,
                parent_index: Some(1),
                visible: false,
                walked: None,
            },
            DeferredChunk {
                anchor: 2,
                seq: 1,
                depth: 2,
                parent_index: Some(1),
                visible: true,
                walked: Some((vec![visible_row_line], vec![visible_row_node])),
            },
        ];

        let (mut spliced_lines, mut spliced_nodes, map) =
            splice_deferred_chunks(base_lines, base_nodes, deferred);
        renumber_indexed_tree(&mut spliced_lines, &mut spliced_nodes, &map);

        assert_eq!(spliced_nodes.len(), 4);
        assert_eq!(spliced_lines.len(), 5);
        assert_eq!(spliced_nodes[2].title.as_deref(), Some("Visible message"));
        assert_eq!(spliced_nodes[2].element_index, Some(2));
        assert_eq!(spliced_nodes[3].role, "Edit");
        assert_eq!(spliced_nodes[3].element_index, Some(3));
        assert!(spliced_lines[2].1.starts_with("- (offscreen rows: 1;"));
        assert_eq!(spliced_lines[3].1, r#"- [2] ListItem "Visible message""#);
        assert_eq!(spliced_lines[4].1, r#"- [3] Edit "Type a message""#);
    }
}
