//! Find the menu item a key combination stands for.
//!
//! Background key events reach a window's views but never its app's menu key
//! equivalents (see `input::keyboard::hotkey_no_auth`). A background
//! `cmd+s` to TextEdit was reported as "Pressed" and saved nothing. When a
//! combo is a menu item's key equivalent, `hotkey` acts on that menu item
//! through accessibility instead, or refuses when macOS keeps the item
//! disabled in the background.

use core_foundation::base::{CFRelease, CFTypeRef};

use crate::ax::bindings::{
    copy_bool_attr, copy_children, copy_element_attr, copy_number_attr, copy_string_attr,
    AXUIElementCreateApplication, AXUIElementRef, AXUIElementSetMessagingTimeout,
};

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
    fn element(&self, node: Self::Node, name: &str) -> Option<Self::Node>;
    fn children(&self, node: Self::Node) -> Vec<Self::Node>;
    fn string(&self, node: Self::Node, name: &str) -> Option<String>;
    fn number(&self, node: Self::Node, name: &str) -> Option<f64>;
    fn boolean(&self, node: Self::Node, name: &str) -> Option<bool>;
    fn release(&self, node: Self::Node);
}
struct LiveMenuReader;
impl MenuReader for LiveMenuReader {
    type Node = AXUIElementRef;
    fn element(&self, n: Self::Node, a: &str) -> Option<Self::Node> {
        unsafe { copy_element_attr(n, a) }
    }
    fn children(&self, n: Self::Node) -> Vec<Self::Node> {
        unsafe { copy_children(n) }
    }
    fn string(&self, n: Self::Node, a: &str) -> Option<String> {
        unsafe { copy_string_attr(n, a) }
    }
    fn number(&self, n: Self::Node, a: &str) -> Option<f64> {
        unsafe { copy_number_attr(n, a) }
    }
    fn boolean(&self, n: Self::Node, a: &str) -> Option<bool> {
        unsafe { copy_bool_attr(n, a) }
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

/// Find the item in `pid`'s menu bar whose key equivalent is `combo`. An
/// enabled match wins over a disabled one with the same equivalent. The
/// Apple menu is skipped: its commands belong to the system.
pub(super) fn find(pid: i32, combo: &Combo) -> Option<MenuMatch> {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return None;
        }
        let _ = AXUIElementSetMessagingTimeout(app, 1.0);
        let reader = LiveMenuReader;
        let bar = reader.element(app, "AXMenuBar");
        CFRelease(app as CFTypeRef);
        let bar = bar?;
        let mut found: Option<MenuMatch> = None;
        let mut budget = MAX_ITEMS;
        visit_releasing(
            semantic_items(&reader, bar),
            |index, top| {
                let title = reader.string(top, "AXTitle").unwrap_or_default();
                if index > 0 || title != "Apple" {
                    search(&reader, top, vec![title], combo, &mut found, &mut budget, 0);
                }
                found.as_ref().is_some_and(|m| m.enabled)
            },
            |top| CFRelease(top as CFTypeRef),
        );
        CFRelease(bar as CFTypeRef);
        found
    }
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

/// The items under a menu bar item or submenu item (through the untitled
/// `AXMenu` container AppKit puts between them).
fn semantic_items<R: MenuReader>(reader: &R, parent: R::Node) -> Vec<R::Node> {
    let mut out = Vec::new();
    for child in reader.children(parent) {
        if reader.string(child, "AXRole").as_deref() == Some("AXMenu") {
            out.extend(reader.children(child));
            reader.release(child);
        } else {
            out.push(child);
        }
    }
    out
}

fn search<R: MenuReader>(
    reader: &R,
    parent: R::Node,
    path: Vec<String>,
    combo: &Combo,
    found: &mut Option<MenuMatch>,
    budget: &mut usize,
    depth: usize,
) {
    for item in semantic_items(reader, parent) {
        if *budget == 0 || found.as_ref().is_some_and(|m| m.enabled) {
            reader.release(item);
            continue;
        }
        *budget -= 1;
        let title = reader.string(item, "AXTitle").unwrap_or_default();
        let cmd_char = reader.string(item, "AXMenuItemCmdChar").unwrap_or_default();
        if !cmd_char.is_empty() {
            let modifiers = reader.number(item, "AXMenuItemCmdModifiers").unwrap_or(0.0) as i64;
            if combo.matches(&cmd_char, modifiers) {
                let enabled = reader.boolean(item, "AXEnabled") != Some(false);
                if found.is_none() || enabled {
                    let mut item_path = path.clone();
                    item_path.push(title.clone());
                    *found = Some(MenuMatch {
                        path: item_path,
                        enabled,
                    });
                }
            }
        } else if depth < 2 && !title.is_empty() {
            let mut sub_path = path.clone();
            sub_path.push(title);
            search(reader, item, sub_path, combo, found, budget, depth + 1);
        }
        reader.release(item);
    }
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

    struct FailedItemReader;
    impl MenuReader for FailedItemReader {
        type Node = usize;
        fn element(&self, _: usize, _: &str) -> Option<usize> { None }
        fn children(&self, node: usize) -> Vec<usize> {
            if node == 0 { vec![1] } else { vec![] }
        }
        fn string(&self, _: usize, attr: &str) -> Option<String> {
            match attr {
                "AXRole" => Some("AXMenuItem".into()),
                "AXTitle" => Some("Save".into()),
                // Simulate a failed AXMenuItemCmdChar read.
                _ => None,
            }
        }
        fn number(&self, _: usize, _: &str) -> Option<f64> { None }
        fn boolean(&self, _: usize, _: &str) -> Option<bool> { None }
        fn release(&self, _: usize) {}
    }

    #[test]
    fn incomplete_lookup_sends_no_keys() {
        let combo = Combo::parse(&["cmd".to_owned()], "s").unwrap();
        let mut found = None;
        let mut budget = MAX_ITEMS;
        search(&FailedItemReader, 0, vec!["File".into()], &combo, &mut found, &mut budget, 0);
        // The current callers dispatch raw keys on None.
        let keys_sent = found.is_none();
        assert!(!keys_sent, "failed menu read must not route to raw keys");
    }
}
