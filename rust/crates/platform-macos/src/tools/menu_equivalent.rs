//! Find the menu item a key combination stands for.
//!
//! Background key events reach a window's views but never its app's menu key
//! equivalents (see `input::keyboard::hotkey_no_auth`). A background
//! `cmd+s` to TextEdit was reported as "Pressed" and saved nothing. When a
//! combo is a menu item's key equivalent, `hotkey` acts on that menu item
//! through accessibility instead, or refuses when macOS keeps the item
//! disabled in the background.

use crate::ax::bindings::{
    AXUIElementCreateApplication, AXUIElementRef, AXUIElementSetMessagingTimeout,
};
use core_foundation::base::{CFRelease, CFTypeRef};
#[path = "menu_lookup_reader.rs"]
mod menu_lookup_reader;
use menu_lookup_reader::Read;

/// `kAXMenuItemModifier*` bits of `AXMenuItemCmdModifiers`.
const MOD_SHIFT: i64 = 1 << 0;
const MOD_OPTION: i64 = 1 << 1;
const MOD_CONTROL: i64 = 1 << 2;
const MOD_NO_COMMAND: i64 = 1 << 3;

/// Upper bound on menu items read per lookup, so a huge dynamic menu cannot
/// stall the call.
const MAX_ITEMS: usize = 2000;

/// A hotkey as a menu key equivalent compares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Combo {
    shift: bool,
    option: bool,
    control: bool,
    /// The uppercase character of the base key.
    character: String,
}

fn canonical_menu_cmd_char(key: &str) -> Option<String> {
    let mut chars = key.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        if !ch.is_whitespace() && !ch.is_control() {
            return Some(ch.to_uppercase().collect());
        }
    }
    match key.trim().to_ascii_lowercase().as_str() {
        "backspace" | "delete" => Some("\u{0008}".to_owned()),
        "forward_delete" | "forwarddelete" => Some("\u{007f}".to_owned()),
        "return" | "enter" => Some("\r".to_owned()),
        "escape" | "esc" => Some("\u{001b}".to_owned()),
        "up" | "arrowup" => Some("\u{001e}".to_owned()),
        "down" | "arrowdown" => Some("\u{001f}".to_owned()),
        "left" | "arrowleft" => Some("\u{001c}".to_owned()),
        "right" | "arrowright" => Some("\u{001d}".to_owned()),
        _ => None,
    }
}

impl Combo {
    /// A combo with Command and one printable or named menu-equivalent key.
    pub(super) fn parse(modifiers: &[String], key: &str) -> Option<Self> {
        let mut cmd = false;
        let mut combo = Combo {
            shift: false,
            option: false,
            control: false,
            character: String::new(),
        };
        for modifier in modifiers {
            match modifier.to_ascii_lowercase().as_str() {
                "cmd" | "command" | "super" | "meta" | "win" => cmd = true,
                "shift" => combo.shift = true,
                "option" | "alt" => combo.option = true,
                "ctrl" | "control" => combo.control = true,
                _ => return None,
            }
        }
        if !cmd {
            return None;
        }
        combo.character = canonical_menu_cmd_char(key)?;
        Some(combo)
    }

    /// Whether a menu item with this `AXMenuItemCmdChar` and
    /// `AXMenuItemCmdModifiers` fires on this combo.
    fn matches(&self, cmd_char: &str, modifiers: i64) -> bool {
        modifiers & MOD_NO_COMMAND == 0
            && cmd_char.to_uppercase() == self.character
            && (modifiers & MOD_SHIFT != 0) == self.shift
            && (modifiers & MOD_OPTION != 0) == self.option
            && (modifiers & MOD_CONTROL != 0) == self.control
    }
}

/// A menu item found for a combo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MenuMatch {
    /// Titles from the menu bar item down to the item.
    pub path: Vec<String>,
    pub enabled: bool,
}

trait MenuReader {
    type Node: Copy;
    fn element(&self, node: Self::Node, name: &str) -> Read<Self::Node>;
    fn children(&self, node: Self::Node) -> Read<Vec<Self::Node>>;
    fn string(&self, node: Self::Node, name: &str) -> Read<String>;
    fn number(&self, node: Self::Node, name: &str) -> Read<f64>;
    fn boolean(&self, node: Self::Node, name: &str) -> Read<bool>;
    fn release(&self, node: Self::Node);
}
struct LiveMenuReader;
impl MenuReader for LiveMenuReader {
    type Node = AXUIElementRef;
    fn element(&self, n: Self::Node, a: &str) -> Read<Self::Node> {
        unsafe { menu_lookup_reader::element(n, a) }
    }
    fn children(&self, n: Self::Node) -> Read<Vec<Self::Node>> {
        unsafe { menu_lookup_reader::children(n) }
    }
    fn string(&self, n: Self::Node, a: &str) -> Read<String> {
        unsafe { menu_lookup_reader::string(n, a) }
    }
    fn number(&self, n: Self::Node, a: &str) -> Read<f64> {
        unsafe { menu_lookup_reader::number(n, a) }
    }
    fn boolean(&self, n: Self::Node, a: &str) -> Read<bool> {
        unsafe { menu_lookup_reader::boolean(n, a) }
    }
    fn release(&self, n: Self::Node) {
        unsafe { CFRelease(n as CFTypeRef) }
    }
}
pub(super) fn spawn_lookup<F, R>(f: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    crate::goat::action_latency::spawn_action_blocking(f)
}

/// Find a menu equivalent, retaining uncertainty when the menu could not be read.
pub(super) fn find(pid: i32, combo: &Combo) -> MenuLookup {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return MenuLookup::Incomplete("the application could not be read");
        }
        let _ = AXUIElementSetMessagingTimeout(app, 1.0);
        let reader = LiveMenuReader;
        let result = find_from_app(&reader, app, combo);
        reader.release(app);
        result
    }
}
fn find_from_app<R: MenuReader>(reader: &R, app: R::Node, combo: &Combo) -> MenuLookup {
    match reader.element(app, "AXMenuBar") {
        Read::Absent => MenuLookup::Absent,
        Read::Failed(_error) => MenuLookup::Incomplete("the menu bar could not be read"),
        Read::WrongType => MenuLookup::Incomplete("the menu bar could not be read"),
        Read::Present(bar) => {
            let found = find_in_bar(reader, bar, combo);
            reader.release(bar);
            found
        }
    }
}

fn find_in_bar<R: MenuReader>(reader: &R, bar: R::Node, combo: &Combo) -> MenuLookup {
    let mut state = Search {
        found: None,
        incomplete: false,
        budget: MAX_ITEMS,
    };
    let items = semantic_items(reader, bar, &mut state);
    visit_releasing(
        items,
        |index, top| {
            let title = match reader.string(top, "AXTitle") {
                Read::Present(title) => title,
                Read::Absent | Read::Failed(_) | Read::WrongType => {
                    state.incomplete = true;
                    return false;
                }
            };
            if title.is_empty() {
                state.incomplete = true;
            } else if index > 0 || title != "Apple" {
                search(reader, top, vec![title], combo, &mut state, 0);
            }
            state.enabled()
        },
        |top| reader.release(top),
    );
    state.result()
}

/// Visit retained items in order until `visit` returns true (stop), then
/// release every item: the visited ones and the ones never reached.
fn visit_releasing<T: Copy>(
    items: Vec<T>,
    mut visit: impl FnMut(usize, T) -> bool,
    mut release: impl FnMut(T),
) {
    let mut stopped = false;
    for (index, item) in items.into_iter().enumerate() {
        if !stopped {
            stopped = visit(index, item);
        }
        release(item);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum MenuLookup {
    Found(MenuMatch),
    Absent,
    Incomplete(&'static str),
}

struct Search {
    found: Option<MenuMatch>,
    incomplete: bool,
    budget: usize,
}

impl Search {
    fn enabled(&self) -> bool {
        self.found.as_ref().is_some_and(|m| m.enabled)
    }
    fn result(self) -> MenuLookup {
        match self.found {
            Some(found) if found.enabled || !self.incomplete => MenuLookup::Found(found),
            _ if self.incomplete => MenuLookup::Incomplete("the menu could not be fully read"),
            _ => MenuLookup::Absent,
        }
    }
}

fn semantic_items<R: MenuReader>(reader: &R, parent: R::Node, state: &mut Search) -> Vec<R::Node> {
    let children = match reader.children(parent) {
        Read::Present(children) => children,
        Read::Absent => return Vec::new(),
        Read::Failed(_) | Read::WrongType => {
            state.incomplete = true;
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for child in children {
        match reader.string(child, "AXRole") {
            Read::Present(role) if role == "AXMenu" => {
                match reader.children(child) {
                    Read::Present(items) => out.extend(items),
                    Read::Absent => (),
                    Read::Failed(_) | Read::WrongType => state.incomplete = true,
                }
                reader.release(child);
            }
            Read::Present(_) => out.push(child),
            Read::Absent | Read::Failed(_) | Read::WrongType => {
                state.incomplete = true;
                reader.release(child);
            }
        }
    }
    out
}

fn search<R: MenuReader>(
    reader: &R,
    parent: R::Node,
    path: Vec<String>,
    combo: &Combo,
    state: &mut Search,
    depth: usize,
) {
    for item in semantic_items(reader, parent, state) {
        if state.enabled() {
            reader.release(item);
            continue;
        }
        if state.budget == 0 {
            state.incomplete = true;
            reader.release(item);
            continue;
        }
        state.budget -= 1;
        let title = match reader.string(item, "AXTitle") {
            Read::Present(title) => title,
            Read::Absent => String::new(),
            Read::Failed(_) | Read::WrongType => {
                state.incomplete = true;
                reader.release(item);
                continue;
            }
        };
        let cmd_char = match reader.string(item, "AXMenuItemCmdChar") {
            Read::Present(value) => value,
            Read::Absent => String::new(),
            Read::Failed(_) | Read::WrongType => {
                state.incomplete = true;
                reader.release(item);
                continue;
            }
        };
        if !cmd_char.is_empty() {
            if cmd_char.to_uppercase() == combo.character {
                let modifiers = match reader.number(item, "AXMenuItemCmdModifiers") {
                    Read::Present(n)
                        if n.is_finite() && n.fract() == 0.0 && (0.0..=15.0).contains(&n) =>
                    {
                        n as i64
                    }
                    _ => {
                        state.incomplete = true;
                        reader.release(item);
                        continue;
                    }
                };
                if combo.matches(&cmd_char, modifiers) {
                    if title.is_empty() {
                        state.incomplete = true;
                        reader.release(item);
                        continue;
                    }
                    let enabled = match reader.boolean(item, "AXEnabled") {
                        Read::Present(enabled) => enabled,
                        _ => {
                            state.incomplete = true;
                            reader.release(item);
                            continue;
                        }
                    };
                    if state.found.is_none() || enabled {
                        let mut item_path = path.clone();
                        item_path.push(title.clone());
                        state.found = Some(MenuMatch {
                            path: item_path,
                            enabled,
                        });
                    }
                }
            }
        }
        if !state.enabled() {
            if depth < 2 && !title.is_empty() {
                let mut sub_path = path.clone();
                sub_path.push(title);
                search(reader, item, sub_path, combo, state, depth + 1);
            } else {
                let remaining = semantic_items(reader, item, state);
                if !remaining.is_empty() {
                    state.incomplete = true;
                }
                for node in remaining {
                    reader.release(node);
                }
            }
        }
        reader.release(item);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum InitialLookup {
    Skipped,
    Lookup(MenuLookup),
}

pub(super) fn lookup_native_menu(native: bool, find: impl FnOnce() -> MenuLookup) -> InitialLookup {
    if native {
        InitialLookup::Lookup(find())
    } else {
        InitialLookup::Skipped
    }
}

pub(super) fn lookup_worker_result<E>(result: Result<InitialLookup, E>) -> InitialLookup {
    result.unwrap_or(InitialLookup::Lookup(MenuLookup::Incomplete(
        "the lookup worker failed",
    )))
}

pub(super) fn lookup_refusal(tool: &str) -> cua_driver_core::protocol::ToolResult {
    super::hotkey::menu_refusal_without_keys(super::pre_dispatch_refusal(
        "background_unavailable",
        format!("{tool}: the app's menu could not be fully read. Retry, or use invoke_menu. No key or menu action was sent."),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn combo(modifiers: &[&str], key: &str) -> Option<Combo> {
        let modifiers: Vec<String> = modifiers.iter().map(|m| m.to_string()).collect();
        Combo::parse(&modifiers, key)
    }

    #[test]
    fn command_combos_accept_printable_and_named_menu_keys() {
        assert!(combo(&["cmd"], "s").is_some());
        assert!(combo(&["command", "shift"], "S").is_some());
        assert!(combo(&["cmd"], ",").is_some());
        assert_eq!(combo(&["ctrl"], "a"), None);
        assert!(combo(&["cmd"], "return").is_some());
        assert!(combo(&["cmd"], "up").is_some());
        assert_eq!(combo(&["cmd", "hyper"], "s"), None);
    }

    #[test]
    fn named_keys_map_to_their_menu_command_characters() {
        for (key, character) in [
            ("backspace", "\u{0008}"),
            ("delete", "\u{0008}"),
            ("forward_delete", "\u{007f}"),
            ("forwarddelete", "\u{007f}"),
            ("return", "\r"),
            ("enter", "\r"),
            ("escape", "\u{001b}"),
            ("esc", "\u{001b}"),
            ("up", "\u{001e}"),
            ("arrowup", "\u{001e}"),
            ("down", "\u{001f}"),
            ("arrowdown", "\u{001f}"),
            ("left", "\u{001c}"),
            ("arrowleft", "\u{001c}"),
            ("right", "\u{001d}"),
            ("arrowright", "\u{001d}"),
        ] {
            let parsed = combo(&["cmd"], key).unwrap_or_else(|| panic!("{key}"));
            assert_eq!(parsed.character, character, "{key}");
            assert!(parsed.matches(character, 0), "{key}");
            assert!(!parsed.matches(character, MOD_SHIFT), "{key}");
            assert_eq!(combo(&["ctrl"], key), None, "{key}");
        }
        assert_eq!(combo(&["cmd"], "hyper"), None);
    }

    #[test]
    fn modifiers_must_match_exactly() {
        let save = combo(&["cmd"], "s").unwrap();
        assert!(save.matches("S", 0));
        assert!(save.matches("s", 0));
        assert!(!save.matches("S", MOD_SHIFT), "Save As is cmd+shift+s");
        assert!(!save.matches("S", MOD_NO_COMMAND));
        assert!(!save.matches("A", 0));
        let save_as = combo(&["cmd", "shift"], "s").unwrap();
        assert!(save_as.matches("S", MOD_SHIFT));
        assert!(!save_as.matches("S", 0));
        let minimize_all = combo(&["cmd", "option"], "m").unwrap();
        assert!(minimize_all.matches("M", MOD_OPTION));
        assert!(!minimize_all.matches("M", MOD_OPTION | MOD_CONTROL));
    }

    #[test]
    fn an_early_match_still_releases_every_top_level_item() {
        let mut visited = Vec::new();
        let mut released = Vec::new();
        visit_releasing(
            vec![10, 11, 12, 13],
            |index, item| {
                visited.push(item);
                index == 1
            },
            |item| released.push(item),
        );
        assert_eq!(visited, vec![10, 11], "nothing is searched after the match");
        assert_eq!(
            released,
            vec![10, 11, 12, 13],
            "every retained item is released"
        );

        let mut released = Vec::new();
        visit_releasing(vec![1, 2], |_, _| false, |item| released.push(item));
        assert_eq!(released, vec![1, 2]);
    }
}

#[cfg(test)]
mod lookup_tristate_tests {
    use super::*;
    use std::cell::Cell;

    #[derive(Clone)]
    struct Node {
        bar: Read<usize>,
        title: Read<String>,
        role: Read<String>,
        command: Read<String>,
        modifiers: Read<f64>,
        enabled: Read<bool>,
        children: Read<Vec<usize>>,
    }
    impl Node {
        fn new(title: &str, command: &str, children: Vec<usize>) -> Self {
            Self {
                bar: Read::Absent,
                title: Read::Present(title.into()),
                role: Read::Present("AXMenuItem".into()),
                command: Read::Present(command.into()),
                modifiers: Read::Present(0.0),
                enabled: Read::Present(true),
                children: Read::Present(children),
            }
        }
    }
    struct Fake(Vec<Node>);
    impl MenuReader for Fake {
        type Node = usize;
        fn element(&self, node: usize, _: &str) -> Read<usize> {
            self.0[node].bar.clone()
        }
        fn children(&self, n: usize) -> Read<Vec<usize>> {
            self.0[n].children.clone()
        }
        fn string(&self, n: usize, attr: &str) -> Read<String> {
            match attr {
                "AXRole" => self.0[n].role.clone(),
                "AXTitle" => self.0[n].title.clone(),
                "AXMenuItemCmdChar" => self.0[n].command.clone(),
                _ => panic!("unexpected attribute"),
            }
        }
        fn number(&self, n: usize, _: &str) -> Read<f64> {
            self.0[n].modifiers.clone()
        }
        fn boolean(&self, n: usize, _: &str) -> Read<bool> {
            self.0[n].enabled.clone()
        }
        fn release(&self, _: usize) {}
    }
    fn run(nodes: Vec<Node>, budget: usize) -> MenuLookup {
        let combo = Combo::parse(&["cmd".into()], "s").unwrap();
        let mut state = Search {
            found: None,
            incomplete: false,
            budget,
        };
        search(&Fake(nodes), 0, vec!["File".into()], &combo, &mut state, 0);
        state.result()
    }
    fn tree() -> Vec<Node> {
        vec![
            Node::new("File", "", vec![1]),
            Node::new("Save", "s", vec![]),
        ]
    }
    #[test]
    fn missing_menu_bar_is_absent() {
        let combo = Combo::parse(&["cmd".into()], "s").unwrap();
        assert_eq!(find_from_app(&Fake(tree()), 0, &combo), MenuLookup::Absent);
    }
    #[test]
    fn unreadable_menu_bar_is_incomplete() {
        let combo = Combo::parse(&["cmd".into()], "s").unwrap();
        for bad in [Read::Failed(-25204), Read::WrongType] {
            let mut nodes = tree();
            nodes[0].bar = bad;
            assert!(matches!(
                find_from_app(&Fake(nodes), 0, &combo),
                MenuLookup::Incomplete(_)
            ));
        }
    }
    #[test]
    fn invalid_modifiers_and_unreadable_enablement_are_incomplete() {
        for bad in [f64::NAN, 1.5, 16.0, -1.0] {
            let mut nodes = tree();
            nodes[1].modifiers = Read::Present(bad);
            assert!(matches!(run(nodes, 20), MenuLookup::Incomplete(_)));
        }
        let mut nodes = tree();
        nodes[1].enabled = Read::Absent;
        assert!(matches!(run(nodes, 20), MenuLookup::Incomplete(_)));
    }
    #[test]
    fn an_enabled_match_is_found() {
        assert!(matches!(
            run(tree(), 20),
            MenuLookup::Found(MenuMatch { enabled: true, .. })
        ));
    }
    #[test]
    fn a_menu_without_the_combo_is_absent() {
        let mut n = tree();
        n[1].command = Read::Present("a".into());
        assert_eq!(run(n, 20), MenuLookup::Absent);
    }
    #[test]
    fn a_failed_item_read_is_incomplete() {
        let mut n = tree();
        n[1].command = Read::Failed(-25204);
        assert!(matches!(run(n, 20), MenuLookup::Incomplete(_)));
    }
    #[test]
    fn budget_exhaustion_is_incomplete() {
        assert!(matches!(run(tree(), 0), MenuLookup::Incomplete(_)));
    }
    #[test]
    fn incomplete_lookup_sends_no_keys() {
        let mut n = tree();
        n[1].command = Read::Failed(-25204);
        let keys = Cell::new(0);
        if matches!(run(n, 20), MenuLookup::Absent) {
            keys.set(keys.get() + 1);
        }
        assert_eq!(keys.get(), 0);
    }
    #[test]
    fn a_command_item_does_not_hide_a_matching_descendant() {
        let mut nodes = tree();
        nodes[1].command = Read::Present("x".into());
        nodes[1].children = Read::Present(vec![2]);
        nodes.push(Node::new("Save", "s", vec![]));
        assert!(matches!(
            run(nodes, 20),
            MenuLookup::Found(MenuMatch { enabled: true, .. })
        ));
    }
    #[test]
    fn unnamed_match_is_incomplete() {
        let mut nodes = tree();
        nodes[1].title = Read::Absent;
        assert!(matches!(run(nodes, 20), MenuLookup::Incomplete(_)));
    }
    #[test]
    fn disabled_candidate_with_failed_branch_is_incomplete() {
        let mut n = tree();
        n[0].children = Read::Present(vec![1, 2]);
        n[1].enabled = Read::Present(false);
        let mut other = Node::new("Other", "", vec![]);
        other.command = Read::Failed(-25204);
        n.push(other);
        assert!(matches!(run(n, 20), MenuLookup::Incomplete(_)));
    }
    fn depth_tree() -> Vec<Node> {
        vec![
            Node::new("File", "", vec![1]),
            Node::new("A", "", vec![2]),
            Node::new("B", "", vec![3]),
            Node::new("C", "", vec![]),
        ]
    }
    #[test]
    fn depth_limit_leaf_is_complete() {
        assert_eq!(run(depth_tree(), 20), MenuLookup::Absent);
    }
    #[test]
    fn depth_limit_with_children_is_incomplete() {
        let mut n = depth_tree();
        n[3].children = Read::Present(vec![4]);
        n.push(Node::new("D", "s", vec![]));
        assert!(matches!(run(n, 20), MenuLookup::Incomplete(_)));
    }
    #[test]
    fn depth_limit_unreadable_children_is_incomplete() {
        let mut n = depth_tree();
        n[3].children = Read::Failed(-25204);
        assert!(matches!(run(n, 20), MenuLookup::Incomplete(_)));
    }
    #[test]
    fn enabled_candidate_can_win_after_a_failed_sibling() {
        let mut n = tree();
        n[0].children = Read::Present(vec![2, 1]);
        let mut other = Node::new("Bad", "", vec![]);
        other.command = Read::WrongType;
        n.push(other);
        assert!(matches!(
            run(n, 20),
            MenuLookup::Found(MenuMatch { enabled: true, .. })
        ));
    }
}
