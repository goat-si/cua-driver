//! macOS Accessibility (AX) API bindings and tree walker.
//!
//! # AX element indexing
//! Every *actionable* element in the tree is assigned an `element_index` (0-based,
//! depth-first). The index is stable within a single `get_window_state` snapshot
//! and is passed to `click`, `type_text`, etc. to identify the target element
//! without requiring pixel coordinates.
//!
//! # Tree format (treeMarkdown)
//! ```text
//!   - [0] AXButton "OK" [actions=[press]]
//!     - AXStaticText = "OK"
//!   - [1] AXTextField "Name" [value="John"]
//! ```
//! - Indexed elements: `- [N] AXRole "Title" [key=val ...]`
//! - Non-indexed (no actions, not interesting): `- AXRole = "value"`
//! - 2-space indent per depth level

#[allow(dead_code)]
pub(crate) mod actionability;
mod batch_attrs;
pub mod bindings;
pub(crate) mod content_attrs;
pub(crate) mod context_menus;
mod control_attrs;
pub mod dense_rows;
pub mod editable;
pub mod enablement;
pub mod exact_target;
#[cfg(test)]
mod item_owner_tests;
pub(crate) mod label_reader;
pub mod launch;
pub mod reveal_scroll;
pub mod scroll_area;
pub mod snapshot;
pub mod tree;
mod tree_source;
pub(crate) mod vocabulary;
pub mod window_scope;
pub(crate) mod write_outcome;

pub use snapshot::Snapshots;
pub use tree::{
    walk_tree, walk_tree_bounded, AXNode, TreeWalkResult, DEFAULT_MAX_DEPTH, DEFAULT_MAX_ELEMENTS,
};
pub use window_scope::WindowScope;
