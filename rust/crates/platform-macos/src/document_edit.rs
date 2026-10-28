//! Keep a document's edited state in step with accessibility text writes.
//!
//! An `AXValue` write, or an `AXSelectedText` insert, changes an `NSTextView`'s
//! text without going through its edit model: no undo entry is registered and
//! the `NSDocument` is not marked edited. TextEdit then never autosaves the
//! change, and closing or saving keeps the old content on disk (Bench v2
//! DS-07). Key events do go through the edit model.
//!
//! After an AX write that changed the text of a document window (one with an
//! `AXDocument` URL), this sends one net-zero edit as key events (a space and
//! a backspace), which registers the change with the document without
//! changing its content. Each key is read back: the space must be seen in the
//! text before the backspace goes out, and the text must then read as before.
//! An unchanged text alone proves nothing, since two lost keys leave it
//! unchanged too. There is no accessibility readback of the edited state:
//! `AXEdited` on the close button stays false for autosave-in-place documents
//! even after a typed edit (VM check, TextEdit, 11 Oct 2026), so a key edit
//! seen landing is the evidence.

use std::time::Duration;

use crate::ax::bindings::{
    copy_element_attr, copy_string_attr, kAXValueCFRangeType, AXUIElementRef,
    AXUIElementSetAttributeValue, AXValueCreate,
};

/// What the document's edited mark showed after an AX text write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentEdited {
    /// The element is not in a document window, so there is nothing to save.
    NotADocument,
    /// The driver saw its net-zero key edit land: the space appeared in the
    /// text and the backspace removed it again.
    Marked,
    /// The key edit could not be sent to this window, was not seen landing, or
    /// left the text changed. A save may keep the old content.
    NotMarked,
}

impl DocumentEdited {
    /// A sentence for the tool result, or nothing when there is nothing to say.
    pub fn note(self) -> &'static str {
        match self {
            DocumentEdited::NotADocument => "",
            DocumentEdited::Marked => {
                " A net-zero key edit (space, backspace) was sent and read back so the document \
                 registers the change and saves it; an accessibility write alone does not."
            }
            DocumentEdited::NotMarked => {
                " Warning: the document is not marked edited, so saving or closing it may keep \
                 the old content. Type the text with key events instead (click the field, then \
                 type_text), or edit it in the foreground."
            }
        }
    }

    /// The structured value, `None` when the element is not in a document.
    pub fn structured(self) -> Option<bool> {
        match self {
            DocumentEdited::NotADocument => None,
            DocumentEdited::NotMarked => Some(false),
            DocumentEdited::Marked => Some(true),
        }
    }
}

#[repr(C)]
struct CfRange {
    location: isize,
    length: isize,
}

/// Whether `element` sits in a document window (its window has an
/// `AXDocument` URL).
///
/// # Safety
/// `element` must be a live `AXUIElementRef`.
unsafe fn in_document_window(element: AXUIElementRef) -> bool {
    let Some(window) = copy_element_attr(element, "AXWindow") else {
        return false;
    };
    let document = copy_string_attr(window, "AXDocument");
    core_foundation::base::CFRelease(window as _);
    document.is_some_and(|url| !url.is_empty())
}

/// Whether the app's key window (its `AXFocusedWindow`) is the element's
/// window, so pid-routed key events reach this text and not another window.
///
/// # Safety
/// `element` must be a live `AXUIElementRef`.
unsafe fn keys_reach_element_window(pid: i32, element: AXUIElementRef) -> bool {
    let Some(window) = copy_element_attr(element, "AXWindow") else {
        return false;
    };
    let own = crate::ax::bindings::ax_get_window_id(window);
    core_foundation::base::CFRelease(window as _);
    own.is_some() && crate::ax::bindings::focused_window_id_of_pid(pid) == own
}

/// After an AX text write on `element` (in `pid`) that changed its value, make
/// sure its document is marked edited. See the module docs.
///
/// # Safety
/// `element` must be a live `AXUIElementRef` for the duration of the call.
/// `caret_to_end` puts the caret at the end first (after replacing the whole
/// value); otherwise the edit happens at the caret, which an insert leaves
/// collapsed just after the inserted text.
pub unsafe fn ensure_marked_after_ax_write(
    pid: i32,
    element: AXUIElementRef,
    caret_to_end: bool,
) -> DocumentEdited {
    if !in_document_window(element) {
        return DocumentEdited::NotADocument;
    }
    if keys_reach_element_window(pid, element) {
        return send_net_zero_edit(pid, element, caret_to_end);
    }
    // With several windows open, the app's key window is another document,
    // and pid-routed keys go there. Make this window key inside the app
    // through its focus record (no activation, no raise; the user's window
    // keeps key status), send the net-zero edit, and hand key back. The
    // app's AXFocusedWindow does not follow a focus record, so the proof is
    // the document's own modified flag, read through the app's scripting
    // interface (only when Automation is already allowed).
    let Some(own) = element_window_id(element) else {
        return DocumentEdited::NotMarked;
    };
    let Some(path) = document_path(element) else {
        return DocumentEdited::NotMarked;
    };
    let Some(bundle_id) = crate::apps::bundle_id_for_pid(pid) else {
        return DocumentEdited::NotMarked;
    };
    // A document that already has unsaved changes autosaves its whole text,
    // the accessibility write included; no keys are needed.
    match crate::apple_events::document_modified(&bundle_id, &path) {
        Some(true) => return DocumentEdited::Marked,
        // Without the readback the edit cannot be proven; send nothing.
        None => return DocumentEdited::NotMarked,
        Some(false) => {}
    }
    let previous = crate::ax::bindings::focused_window_id_of_pid(pid);
    if !crate::input::skylight::activate_without_raise_with(pid, own, false) {
        return DocumentEdited::NotMarked;
    }
    // Let the key-window switch land first: a space sent before it would go
    // to the window that was key, which the read-back of this one cannot see.
    std::thread::sleep(Duration::from_millis(120));
    let sent = send_net_zero_edit(pid, element, caret_to_end);
    if let Some(previous) = previous {
        crate::input::skylight::activate_without_raise_with(pid, previous, false);
    }
    if sent == DocumentEdited::Marked
        && crate::apple_events::document_modified(&bundle_id, &path) == Some(true)
    {
        DocumentEdited::Marked
    } else {
        DocumentEdited::NotMarked
    }
}

/// The file behind the element's document window.
///
/// # Safety
/// `element` must be a live `AXUIElementRef`.
unsafe fn document_path(element: AXUIElementRef) -> Option<std::path::PathBuf> {
    let window = copy_element_attr(element, "AXWindow")?;
    let url = copy_string_attr(window, "AXDocument");
    core_foundation::base::CFRelease(window as _);
    crate::apple_events::file_url_to_path(&url?)
}

/// Type a space and a backspace into `element`, reading each one back.
unsafe fn send_net_zero_edit(
    pid: i32,
    element: AXUIElementRef,
    caret_to_end: bool,
) -> DocumentEdited {
    if caret_to_end {
        caret_to_end_of(element);
    }
    let _ = crate::input::ax_actions::focus_element(element as usize);
    net_zero_edit(
        |key| crate::input::keyboard::press_key(pid, key, &[]),
        || copy_string_attr(element, "AXValue"),
        || std::thread::sleep(NET_ZERO_POLL_INTERVAL),
    )
}

/// Pause between reads while waiting for a key of the net-zero edit to show.
const NET_ZERO_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Reads per key before giving up on it (about 500 ms at the interval above).
const NET_ZERO_POLLS: usize = 50;

/// The net-zero edit against injected keys, value reads and pacing.
///
/// 1. An unreadable text sends nothing: no key could be checked.
/// 2. Send a space and wait for the text to grow by one character. If it does
///    not, stop: a backspace now would delete the user's last character.
/// 3. Send a backspace and wait for the text to read as before.
fn net_zero_edit(
    mut press: impl FnMut(&str) -> anyhow::Result<()>,
    mut read: impl FnMut() -> Option<String>,
    mut pause: impl FnMut(),
) -> DocumentEdited {
    let Some(before) = read() else {
        return DocumentEdited::NotMarked;
    };
    let before_len = before.chars().count();
    if press("space").is_err() {
        return DocumentEdited::NotMarked;
    }
    // Wait for the text to differ from `from`; `None` when it never does.
    let mut await_change = |from: &str| {
        for poll in 0..NET_ZERO_POLLS {
            if poll > 0 {
                pause();
            }
            match read() {
                Some(now) if now != from => return Some(now),
                _ => {}
            }
        }
        None
    };
    let spaced = match await_change(&before) {
        Some(now) if now.chars().count() == before_len + 1 => now,
        _ => return DocumentEdited::NotMarked,
    };
    if press("delete").is_err() {
        return DocumentEdited::NotMarked;
    }
    match await_change(&spaced) {
        Some(now) if now == before => DocumentEdited::Marked,
        _ => DocumentEdited::NotMarked,
    }
}

/// The CGWindowID of the window `element` sits in.
///
/// # Safety
/// `element` must be a live `AXUIElementRef`.
unsafe fn element_window_id(element: AXUIElementRef) -> Option<u32> {
    let window = copy_element_attr(element, "AXWindow")?;
    let id = crate::ax::bindings::ax_get_window_id(window);
    core_foundation::base::CFRelease(window as _);
    id
}

/// Collapse the selection to the end of the text.
///
/// # Safety
/// `element` must be a live `AXUIElementRef`.
unsafe fn caret_to_end_of(element: AXUIElementRef) {
    let len = copy_string_attr(element, "AXValue")
        .map(|v| v.encode_utf16().count())
        .unwrap_or(0);
    let range = CfRange {
        location: len as isize,
        length: 0,
    };
    let value = AXValueCreate(kAXValueCFRangeType, &range as *const CfRange as *const _);
    if !value.is_null() {
        use core_foundation::{base::TCFType, string::CFString};
        let attr = CFString::new("AXSelectedTextRange");
        let _ = AXUIElementSetAttributeValue(element, attr.as_concrete_TypeRef(), value as _);
        core_foundation::base::CFRelease(value as _);
    }
}

#[cfg(test)]
mod tests {
    use super::{net_zero_edit, DocumentEdited};
    use std::cell::RefCell;

    /// A text field that applies the keys it is given, after `lag` reads.
    struct Field {
        value: RefCell<String>,
        /// Keys sent and not yet applied, with the reads left before each lands.
        pending: RefCell<Vec<(String, usize)>>,
        sent: RefCell<Vec<String>>,
        lag: usize,
        dropped: &'static [&'static str],
    }

    impl Field {
        fn new(text: &str, lag: usize, dropped: &'static [&'static str]) -> Self {
            Self {
                value: RefCell::new(text.to_owned()),
                pending: RefCell::new(Vec::new()),
                sent: RefCell::new(Vec::new()),
                lag,
                dropped,
            }
        }
        fn press(&self, key: &str) -> anyhow::Result<()> {
            self.sent.borrow_mut().push(key.to_owned());
            if !self.dropped.contains(&key) {
                self.pending.borrow_mut().push((key.to_owned(), self.lag));
            }
            Ok(())
        }
        fn read(&self) -> Option<String> {
            let mut pending = self.pending.borrow_mut();
            while let Some((key, left)) = pending.first_mut() {
                if *left > 0 {
                    *left -= 1;
                    break;
                }
                let key = key.clone();
                pending.remove(0);
                let mut value = self.value.borrow_mut();
                match key.as_str() {
                    "space" => value.push(' '),
                    "delete" => {
                        value.pop();
                    }
                    _ => {}
                }
            }
            Some(self.value.borrow().clone())
        }
    }

    fn run(field: &Field) -> DocumentEdited {
        net_zero_edit(|key| field.press(key), || field.read(), || {})
    }

    /// DSH-05: both keys lost leaves the text unchanged, which is no evidence
    /// that the document registered an edit.
    #[test]
    fn lost_keys_are_not_an_edit() {
        let field = Field::new("hello", 0, &["space", "delete"]);
        assert_eq!(run(&field), DocumentEdited::NotMarked);
        // A backspace sent after a space that never landed would delete the
        // user's last character; it is not sent.
        assert_eq!(*field.sent.borrow(), vec!["space".to_owned()]);
        assert_eq!(field.read().as_deref(), Some("hello"));
    }

    #[test]
    fn a_space_seen_landing_then_removed_marks_the_document() {
        let field = Field::new("hello", 2, &[]);
        assert_eq!(run(&field), DocumentEdited::Marked);
        assert_eq!(
            *field.sent.borrow(),
            vec!["space".to_owned(), "delete".to_owned()]
        );
        assert_eq!(field.read().as_deref(), Some("hello"));
    }

    /// The space landed and the backspace did not: the text moved.
    #[test]
    fn a_lost_backspace_is_not_an_edit() {
        let field = Field::new("hello", 0, &["delete"]);
        assert_eq!(run(&field), DocumentEdited::NotMarked);
        assert_eq!(field.read().as_deref(), Some("hello "));
    }

    #[test]
    fn an_unreadable_value_sends_nothing() {
        let sent = RefCell::new(Vec::<String>::new());
        let verdict = net_zero_edit(
            |key| {
                sent.borrow_mut().push(key.to_owned());
                Ok(())
            },
            || None,
            || {},
        );
        assert_eq!(verdict, DocumentEdited::NotMarked);
        assert!(sent.borrow().is_empty());
    }

    #[test]
    fn only_a_document_left_unedited_gets_a_warning() {
        assert_eq!(DocumentEdited::NotADocument.note(), "");
        assert!(DocumentEdited::Marked.note().contains("net-zero key edit"));
        assert!(DocumentEdited::NotMarked
            .note()
            .contains("may keep the old content"));
        assert_eq!(DocumentEdited::NotADocument.structured(), None);
        assert_eq!(DocumentEdited::Marked.structured(), Some(true));
        assert_eq!(DocumentEdited::NotMarked.structured(), Some(false));
    }
}
