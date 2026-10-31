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

/// What the document mark did inside a window held as key for one insert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldMark {
    /// The element is not in a document window, so there is nothing to save.
    NotADocument,
    /// Whether the element sits in a document window could not be read;
    /// no key was sent.
    DocumentUnknown,
    /// The space was seen landing and the backspace removed it again.
    Marked,
    /// A check stopped it, or the text or caret could not be read, before
    /// any key: no key was sent.
    NothingSent,
    /// The space went out (or its post failed and may still have landed)
    /// and no backspace followed: a space may remain in the text.
    SpaceMayRemain,
    /// The backspace went out but the text did not read back as before.
    Unconfirmed,
}

impl HeldMark {
    /// A sentence for the tool result, or nothing.
    pub fn note(self) -> &'static str {
        match self {
            HeldMark::NotADocument => "",
            HeldMark::DocumentUnknown => {
                " Warning: whether this window holds a saved document could not be read, so no \
                 key edit was sent to mark it edited; saving or closing it may keep the old text."
            }
            HeldMark::Marked => DocumentEdited::Marked.note(),
            HeldMark::NothingSent => {
                " Warning: the document is not marked edited (no key edit was sent), so saving or \
                 closing it may keep the old text."
            }
            HeldMark::SpaceMayRemain => {
                " Warning: a space sent to mark the document edited may remain in the text after \
                 the insert, and the document may not be marked edited; read the window."
            }
            HeldMark::Unconfirmed => {
                " Warning: the space and backspace sent to mark the document edited did not read \
                 back as a net-zero edit, so the text may differ; read the window."
            }
        }
    }

    /// A stable label for structured results.
    pub fn label(self) -> &'static str {
        match self {
            HeldMark::NotADocument => "not_a_document",
            HeldMark::DocumentUnknown => "document_unknown",
            HeldMark::Marked => "marked",
            HeldMark::NothingSent => "nothing_sent",
            HeldMark::SpaceMayRemain => "space_may_remain",
            HeldMark::Unconfirmed => "unconfirmed",
        }
    }
}

/// [`ensure_marked_after_ax_write`] for a window the caller holds as key
/// with another app in front. It never makes a window key itself: when keys
/// do not reach the element's window it sends nothing. `can_post` (the
/// element still focused in the held window, the window still key, another
/// app still in front) is read before the caret focus and before each key.
///
/// # Safety
/// `element` must be a live `AXUIElementRef` for the duration of the call.
pub unsafe fn ensure_marked_in_held_window(
    pid: i32,
    element: AXUIElementRef,
    mut can_post: impl FnMut() -> bool,
) -> HeldMark {
    match document_window_state(element) {
        Some(false) => return HeldMark::NotADocument,
        None => return HeldMark::DocumentUnknown,
        Some(true) => {}
    }
    if !keys_reach_element_window(pid, element) || !can_post() {
        return HeldMark::NothingSent;
    }
    let _ = crate::input::ax_actions::focus_element(element as usize);
    net_zero_edit_held(
        collapsed_caret_of(element),
        can_post,
        |key| crate::input::keyboard::press_key(pid, key, &[]),
        || crate::ax::write_outcome::read_text_value(element),
        || std::thread::sleep(NET_ZERO_POLL_INTERVAL),
    )
}

/// `Some(true)` when the element's window has an `AXDocument` URL,
/// `Some(false)` when the window reads and has none (no value or the
/// attribute unsupported), `None` when the window or the attribute cannot
/// be read.
///
/// # Safety
/// `element` must be a live `AXUIElementRef`.
unsafe fn document_window_state(element: AXUIElementRef) -> Option<bool> {
    use core_foundation::{base::TCFType, string::CFString};
    let window = copy_element_attr(element, "AXWindow")?;
    let attr = CFString::new("AXDocument");
    let mut value: core_foundation::base::CFTypeRef = std::ptr::null();
    let error = crate::ax::bindings::AXUIElementCopyAttributeValue(
        window,
        attr.as_concrete_TypeRef(),
        &mut value,
    );
    core_foundation::base::CFRelease(window as _);
    let url = crate::ax::bindings::string_from_cf(value);
    if !value.is_null() {
        core_foundation::base::CFRelease(value);
    }
    match error {
        crate::ax::bindings::kAXErrorSuccess => url.map(|url| !url.is_empty()),
        crate::ax::bindings::kAXErrorNoValue
        | crate::ax::bindings::kAXErrorAttributeUnsupported => Some(false),
        _ => None,
    }
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
        collapsed_caret_of(element),
        |key| crate::input::keyboard::press_key(pid, key, &[]),
        || crate::ax::write_outcome::read_text_value(element),
        || std::thread::sleep(NET_ZERO_POLL_INTERVAL),
    )
}

/// The caret's UTF-16 offset when the selection is collapsed; `None` when it
/// cannot be read or text is selected (a space would replace the selection).
///
/// # Safety
/// `element` must be a live `AXUIElementRef`.
unsafe fn collapsed_caret_of(element: AXUIElementRef) -> Option<usize> {
    use core_foundation::{base::TCFType, string::CFString};
    let attr = CFString::new("AXSelectedTextRange");
    let mut value: core_foundation::base::CFTypeRef = std::ptr::null();
    let err = crate::ax::bindings::AXUIElementCopyAttributeValue(
        element,
        attr.as_concrete_TypeRef(),
        &mut value,
    );
    if err != crate::ax::bindings::kAXErrorSuccess || value.is_null() {
        return None;
    }
    let mut range = CfRange {
        location: 0,
        length: 0,
    };
    let ok = crate::ax::bindings::AXValueGetValue(
        value as _,
        kAXValueCFRangeType,
        &mut range as *mut CfRange as *mut _,
    );
    core_foundation::base::CFRelease(value);
    (ok && range.length == 0)
        .then(|| usize::try_from(range.location).ok())
        .flatten()
}

/// `text` with a space inserted at UTF-16 offset `caret`; `None` when the
/// offset is past the end or inside a character.
fn with_space_at(text: &str, caret: usize) -> Option<String> {
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        if units == caret {
            return Some(format!("{} {}", &text[..byte], &text[byte..]));
        }
        if units > caret {
            return None;
        }
        units += ch.len_utf16();
    }
    (units == caret).then(|| format!("{text} "))
}

/// Pause between reads while waiting for a key of the net-zero edit to show.
const NET_ZERO_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Reads per key before giving up on it (about 500 ms at the interval above).
const NET_ZERO_POLLS: usize = 50;

/// [`net_zero_edit`] with `can_post` read right before each key, and an
/// outcome that says whether a space may be left in the text.
fn net_zero_edit_held(
    caret: Option<usize>,
    mut can_post: impl FnMut() -> bool,
    mut press: impl FnMut(&str) -> anyhow::Result<()>,
    mut read: impl FnMut() -> Option<String>,
    mut pause: impl FnMut(),
) -> HeldMark {
    let Some(before) = read() else {
        return HeldMark::NothingSent;
    };
    let Some(expected) = caret.and_then(|caret| with_space_at(&before, caret)) else {
        return HeldMark::NothingSent;
    };
    if !can_post() {
        return HeldMark::NothingSent;
    }
    if press("space").is_err() {
        return HeldMark::SpaceMayRemain;
    }
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
    match await_change(&before) {
        Some(now) if now == expected => {}
        _ => return HeldMark::SpaceMayRemain,
    }
    if !can_post() {
        return HeldMark::SpaceMayRemain;
    }
    if press("delete").is_err() {
        return HeldMark::Unconfirmed;
    }
    match await_change(&expected) {
        Some(now) if now == before => HeldMark::Marked,
        _ => HeldMark::Unconfirmed,
    }
}

/// The net-zero edit against injected keys, value reads and pacing.
///
/// 1. An unreadable text sends nothing: no key could be checked.
/// 2. Send a space and wait for the text to change into exactly the earlier
///    text with a space at the caret. Anything else (no change, or another
///    edit) stops here: a backspace now could delete the user's text.
/// 3. Send a backspace and wait for the text to read as before.
fn net_zero_edit(
    caret: Option<usize>,
    mut press: impl FnMut(&str) -> anyhow::Result<()>,
    mut read: impl FnMut() -> Option<String>,
    mut pause: impl FnMut(),
) -> DocumentEdited {
    let Some(before) = read() else {
        return DocumentEdited::NotMarked;
    };
    // A selection or an unknown caret leaves nowhere to expect the space.
    let Some(expected) = caret.and_then(|caret| with_space_at(&before, caret)) else {
        return DocumentEdited::NotMarked;
    };
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
    match await_change(&before) {
        Some(now) if now == expected => {}
        _ => return DocumentEdited::NotMarked,
    }
    if press("delete").is_err() {
        return DocumentEdited::NotMarked;
    }
    match await_change(&expected) {
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
    let len = crate::ax::write_outcome::read_text_value(element)
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
    use super::{net_zero_edit, net_zero_edit_held, with_space_at, DocumentEdited, HeldMark};
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
        let caret = field.value.borrow().encode_utf16().count();
        net_zero_edit(Some(caret), |key| field.press(key), || field.read(), || {})
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
            Some(0),
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

    /// Review finding: a lost space followed by another one-character edit
    /// (`hello` → `helloX`) is not the space; deleting would remove the X.
    #[test]
    fn another_edit_is_not_taken_for_the_space() {
        let sent = RefCell::new(Vec::<String>::new());
        let reads = RefCell::new(0);
        let verdict = net_zero_edit(
            Some(5),
            |key| {
                sent.borrow_mut().push(key.to_owned());
                Ok(())
            },
            || {
                *reads.borrow_mut() += 1;
                Some(
                    if *reads.borrow() == 1 {
                        "hello"
                    } else {
                        "helloX"
                    }
                    .to_owned(),
                )
            },
            || {},
        );
        assert_eq!(verdict, DocumentEdited::NotMarked);
        assert_eq!(*sent.borrow(), vec!["space".to_owned()]);
    }

    /// A selection, or a caret that cannot be read, sends nothing.
    #[test]
    fn no_collapsed_caret_sends_nothing() {
        let field = Field::new("hello", 0, &[]);
        assert_eq!(
            net_zero_edit(None, |key| field.press(key), || field.read(), || {}),
            DocumentEdited::NotMarked
        );
        assert!(field.sent.borrow().is_empty());
    }

    #[test]
    fn the_space_is_expected_at_the_caret() {
        assert_eq!(with_space_at("hello", 5).as_deref(), Some("hello "));
        assert_eq!(with_space_at("hello", 0).as_deref(), Some(" hello"));
        assert_eq!(with_space_at("한글", 1).as_deref(), Some("한 글"));
        assert_eq!(with_space_at("a😀b", 2), None, "inside a surrogate pair");
        assert_eq!(with_space_at("hello", 6), None);
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

    /// Run the held mark with `can_post` answering from `checks` in order
    /// (true when the list runs out).
    fn run_held(field: &Field, checks: &[bool]) -> (HeldMark, usize) {
        let caret = field.value.borrow().encode_utf16().count();
        let reads = std::cell::Cell::new(0usize);
        let mark = net_zero_edit_held(
            Some(caret),
            || {
                let n = reads.get();
                reads.set(n + 1);
                checks.get(n).copied().unwrap_or(true)
            },
            |key| field.press(key),
            || field.read(),
            || {},
        );
        (mark, reads.get())
    }

    #[test]
    fn held_mark_reads_the_checks_before_each_key() {
        let field = Field::new("hello", 1, &[]);
        let (mark, checks) = run_held(&field, &[]);
        assert_eq!(mark, HeldMark::Marked);
        assert_eq!(
            checks, 2,
            "one check before the space, one before the backspace"
        );
        assert_eq!(field.read().as_deref(), Some("hello"));
    }

    /// Focus moved to a sibling window or field, or the app came to the
    /// front, before the space: nothing is sent.
    #[test]
    fn held_mark_sends_nothing_when_the_first_check_fails() {
        let field = Field::new("hello", 0, &[]);
        assert_eq!(run_held(&field, &[false]).0, HeldMark::NothingSent);
        assert!(field.sent.borrow().is_empty());
    }

    /// The check fails between the keys: the space stays and is reported.
    #[test]
    fn held_mark_reports_a_space_left_when_the_second_check_fails() {
        let field = Field::new("hello", 0, &[]);
        assert_eq!(run_held(&field, &[true, false]).0, HeldMark::SpaceMayRemain);
        assert_eq!(*field.sent.borrow(), vec!["space".to_owned()]);
        assert_eq!(field.read().as_deref(), Some("hello "));
        assert!(HeldMark::SpaceMayRemain.note().contains("may remain"));
    }

    #[test]
    fn held_mark_failures_after_a_key_are_never_nothing_sent() {
        // The space never shows: it may still land later.
        let lost = Field::new("hello", 0, &["space"]);
        assert_eq!(run_held(&lost, &[]).0, HeldMark::SpaceMayRemain);
        // The backspace is lost: the text differs.
        let stuck = Field::new("hello", 0, &["delete"]);
        assert_eq!(run_held(&stuck, &[]).0, HeldMark::Unconfirmed);
        assert!(HeldMark::Unconfirmed.note().contains("may differ"));
        // A failed space post may still have reached the app.
        let failed = net_zero_edit_held(
            Some(5),
            || true,
            |_| anyhow::bail!("post failed"),
            || Some("hello".to_owned()),
            || {},
        );
        assert_eq!(failed, HeldMark::SpaceMayRemain);
        // No caret: nothing is sent and no check is spent.
        let field = Field::new("hello", 0, &[]);
        assert_eq!(
            net_zero_edit_held(
                None,
                || true,
                |key| field.press(key),
                || field.read(),
                || {}
            ),
            HeldMark::NothingSent
        );
        assert!(field.sent.borrow().is_empty());
    }
}
