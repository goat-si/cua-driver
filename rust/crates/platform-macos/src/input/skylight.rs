//! SkyLight SPI bridge — Rust port of Swift's `SkyLightEventPost`.
//!
//! Two-layer story (matches Swift reference exactly):
//!
//! 1. **Post path** — `SLEventPostToPid` goes through `SLEventPostToPSN` →
//!    `CGSTickleActivityMonitor` → `SLSUpdateSystemActivityWithLocation` →
//!    `IOHIDPostEvent`. The public `CGEventPostToPid` skips the activity-monitor
//!    tickle so Chromium/Catalyst targets don't accept those events as live input.
//!
//! 2. **Authentication** (keyboard only) — on macOS 14+, WindowServer gates
//!    synthetic keyboard events on Chromium-like targets on an attached
//!    `SLSEventAuthenticationMessage`. We build one via the ObjC factory and
//!    attach it with `SLEventSetAuthenticationMessage` before posting.
//!
//! All symbols are resolved once at first use via `dlopen` + `dlsym`.
//! If anything fails to resolve the functions return `false` and callers
//! fall back to the public `CGEvent::post_to_pid`.

use core_graphics::{
    event::{CGEvent, CGEventFlags},
    event_source::{CGEventSource, CGEventSourceStateID},
};
use foreign_types::ForeignType;
use libc::pid_t;
use std::ffi::{c_void, CStr};
use std::os::raw::{c_char, c_int, c_uint};
use std::sync::OnceLock;

use super::key_belief::{BeliefMatch, Ticket, BELIEFS};

// ── Function-pointer typedefs ──────────────────────────────────────────────

/// `void SLEventPostToPid(pid_t, CGEventRef)`
type PostToPidFn = unsafe extern "C" fn(pid_t, *mut c_void);

/// `void SLEventSetAuthenticationMessage(CGEventRef, id)`
type SetAuthMsgFn = unsafe extern "C" fn(*mut c_void, *mut c_void);

/// `void CGEventSetWindowLocation(CGEventRef, double x, double y)`
///
/// NOTE: CGPoint on 64-bit ARM/x86 is two f64 values packed consecutively.
/// We pass them as two separate f64 arguments which has identical ABI.
type SetWindowLocFn = unsafe extern "C" fn(*mut c_void, f64, f64);

/// `void SLEventSetIntegerValueField(CGEventRef, uint32_t field, int64_t value)`
type SetIntFieldFn = unsafe extern "C" fn(*mut c_void, u32, i64);

/// `uint32_t CGSMainConnectionID(void)`
type ConnectionIDFn = unsafe extern "C" fn() -> u32;

/// `uint64_t CGSGetActiveSpace(uint32_t cid)`
type GetActiveSpaceFn = unsafe extern "C" fn(u32) -> u64;

/// `CFArrayRef SLSCopySpacesForWindows(uint32_t cid, int selector, CFArrayRef windowIDs)`
type CopySpacesForWindowsFn = unsafe extern "C" fn(u32, i32, *const c_void) -> *mut c_void;

/// `CFStringRef SLSCopyManagedDisplayForWindow(uint32_t cid, uint32_t wid)`
type CopyManagedDisplayForWindowFn = unsafe extern "C" fn(u32, u32) -> *mut c_void;

/// `uint64_t SLSManagedDisplayGetCurrentSpace(uint32_t cid, CFStringRef display)`
type ManagedDisplayGetCurrentSpaceFn = unsafe extern "C" fn(u32, *const c_void) -> u64;

/// How the exact window became the app's key window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyPrime {
    AlreadyKey,
    /// The in-app key-window records ([`post_window_key_records`]).
    KeyRecords,
    AxFocusedWindow,
}

/// Where an app's keyboard focus is, as Accessibility reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyFocus {
    /// The app's `AXFocusedWindow`.
    pub window: Option<u32>,
    /// The window holding the app's `AXFocusedUIElement`.
    pub element_window: Option<u32>,
}

impl KeyFocus {
    /// Keys posted to the app go to `target`. The focused window is the
    /// predictor: in every LibreOffice trial keys went to `AXFocusedWindow`
    /// (a sibling document or Find & Replace when the switch failed, the
    /// target when it held). The focused element is not used here: VCL keeps
    /// reporting a modeless dialog's field as focused after its document
    /// window has become key and is receiving the keys.
    fn routes_to(self, target: u32) -> bool {
        self.window == Some(target)
    }

    /// `target` is key with nothing left to prime. A focused window without
    /// any focused element is a LibreOffice that has never been active: it
    /// drops every key until the window is primed (CUA-1767).
    fn settled_on(self, target: u32) -> bool {
        self.routes_to(target) && self.element_window.is_some()
    }
}

/// Make `target` the app's key window and PROVE it before any key is posted.
///
/// `focus` re-reads the app's focus; `key_records` posts the in-app key-window
/// records and waits for them to settle; `ax_focus` writes `AXFocusedWindow`.
/// Each step is followed by a re-read, and keys may only be posted when it
/// shows `target` as the window keys route to. Returns `Err` otherwise:
/// refusing is safe, a key that lands in a sibling window (a Find & Replace
/// field, another document) is not.
pub fn prime_exact_window_key(
    target: u32,
    mut focus: impl FnMut() -> KeyFocus,
    mut fast_records: impl FnMut() -> bool,
    mut key_records: impl FnMut() -> bool,
    mut ax_focus: impl FnMut() -> bool,
) -> Result<KeyPrime, String> {
    if focus().settled_on(target) {
        return Ok(KeyPrime::AlreadyKey);
    }
    // The short record form first: most apps take it, and it is confirmed
    // by the same focus read as the full sequence below, which still runs
    // when it does not show the window as key.
    if fast_records() && focus().routes_to(target) {
        return Ok(KeyPrime::KeyRecords);
    }
    // The records go out at most twice: WindowServer delivers them
    // asynchronously, and the second pass catches an app that was still
    // handling the first.
    for _ in 0..2 {
        if key_records() && focus().routes_to(target) {
            return Ok(KeyPrime::KeyRecords);
        }
    }
    if ax_focus() && focus().routes_to(target) {
        return Ok(KeyPrime::AxFocusedWindow);
    }
    let now = focus();
    Err(format!(
        "window {target} could not be confirmed as the app's key window (the app's focused \
         window is {:?}, its focused element is in window {:?}), so no keys were sent: they \
         could have gone to another window of the same app. Close the other window, or use \
         delivery_mode:\"foreground\"",
        now.window, now.element_window
    ))
}

/// Holds an exact window as the app's key window; hands key back when it is
/// dropped or [`finish`](WindowKeyScope::finish)ed (with the same in-app
/// records, so the app is never activated or raised).
///
/// The hand-back runs on its own thread and nothing waits for it: the call
/// that held the scope returns first, also while unwinding from a panic, and
/// a hand-back that fails is only logged. The next scope's entry waits for a
/// running hand-back ([`await_key_restores`]) so two never interleave their
/// records.
pub struct WindowKeyScope {
    pid: pid_t,
    target: u32,
    previous: Option<u32>,
    /// The app's on-screen windows before priming, front to back.
    order_before: Vec<u32>,
    primed: KeyPrime,
    from_belief: bool,
    /// What entering did inside the app, whatever confirmed key at the end.
    entry: KeyEntry,
    /// The hand-back already ran through `finish`.
    done: bool,
    /// The live reads and posts the hand-back uses (fakes in tests).
    ops: ScopeOps,
    /// Held until the hand-back has run; `None` when key never moved.
    live: Option<LiveKeyScope>,
}

/// The side effects of a scope's hand-back.
#[derive(Clone, Copy)]
struct ScopeOps {
    window_order: fn(pid_t) -> Vec<u32>,
    hand_back: fn(pid_t, u32, KeyPrime, u32) -> HandBack,
}

const LIVE_SCOPE_OPS: ScopeOps = ScopeOps {
    window_order: app_window_order,
    hand_back: run_hand_back,
};

/// What entering a [`WindowKeyScope`] actually did inside the app, apart
/// from how key was finally confirmed ([`KeyPrime`]). Records can have been
/// posted before the accessibility fallback was the step that worked, and a
/// receipt must name everything that may have reached the app.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyEntry {
    /// Key-window record posts that succeeded.
    pub records_posted: u8,
    /// A record post call returned failure: it cannot prove the record did
    /// not reach the app.
    pub records_uncertain: bool,
    /// The app's AXFocusedWindow was written.
    pub ax_write: bool,
}

/// How key focus was handed back when a [`WindowKeyScope`] ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandBack {
    /// Key never moved, so there was nothing to hand back.
    NotMoved,
    /// The previous window reads back as the app's key window.
    Restored,
    /// The hand-back was started on its own thread and nothing waited for it:
    /// the call returned first, and a hand-back that fails is only logged.
    Deferred,
    /// No window of the app is left to hand key back to.
    NoTarget,
    /// Posted, but the previous window does not read back as key.
    NotConfirmed,
}

impl HandBack {
    /// One sentence for a tool result.
    pub fn describe(self) -> &'static str {
        match self {
            HandBack::NotMoved => "Key focus inside the app was not moved.",
            HandBack::Restored => "Key focus went back to the app's previous window.",
            HandBack::Deferred => {
                "Key focus is being handed back to the app's previous window in the background; \
                 its outcome is not waited for."
            }
            HandBack::NoTarget => "No other window of the app was left to hand key focus back to.",
            HandBack::NotConfirmed => {
                "Key focus was handed back, but the app's previous window does not read back \
                 as its key window."
            }
        }
    }
}

/// What a scope did to the app's key focus, for a receipt that otherwise
/// says no action was sent.
/// `hand_back` is `None` when the scope never existed (entering failed).
pub fn scope_disclosure(entry: KeyEntry, hand_back: Option<HandBack>) -> String {
    let mut lines = Vec::new();
    if entry.records_posted > 0 {
        lines.push(format!(
            "{} key-window record(s) were posted to the app for the target window.",
            entry.records_posted
        ));
    }
    if entry.records_uncertain {
        lines
            .push("A key-window record post failed and may still have reached the app.".to_owned());
    }
    if entry.ax_write {
        lines.push(
            "The app's focused window was set to the target window through accessibility."
                .to_owned(),
        );
    }
    if let Some(hand_back) = hand_back.filter(|h| !lines.is_empty() || *h != HandBack::NotMoved) {
        lines.push(hand_back.describe().to_owned());
    }
    lines.join(" ")
}

/// Map a hand-back's record posts to its outcome. `posts` is `None` when the
/// records could not be addressed (no SPI, no PSN): nothing was posted.
fn hand_back_outcome(posts: Option<RecordPosts>, confirmed: impl FnOnce() -> bool) -> HandBack {
    match posts {
        Some(_) if confirmed() => HandBack::Restored,
        Some(_) | None => HandBack::NotConfirmed,
    }
}

/// The live reads and posts a key scope's entry and hand-back make. The live
/// environment is [`LiveKeyEnv`]; tests drive the same code with a fake.
trait KeyEnv {
    fn key_focus(&self, pid: pid_t) -> KeyFocus;
    fn window_order(&self, pid: pid_t) -> Vec<u32>;
    fn post_records(
        &self,
        pid: pid_t,
        current: Option<u32>,
        target: u32,
        style: RecordStyle,
    ) -> Option<RecordPosts>;
    /// Wait at most `total` for the app's focus to settle on `target`.
    fn await_focus(&self, pid: pid_t, target: u32, total: std::time::Duration, mode: FocusPollMode);
    /// This app did not show the window as key after the short record form,
    /// so entering and handing back use the full sequence at once.
    fn needs_full_records(&self, pid: pid_t) -> bool;
    fn note_needs_full_records(&self, pid: pid_t);
    fn set_focused_window(&self, pid: pid_t, wid: u32) -> bool;
    fn belief_match(&self, pid: pid_t, wid: u32) -> BeliefMatch;
    fn belief_ticket(&self, pid: pid_t) -> Option<Ticket>;
    fn belief_now(&self) -> std::time::Instant;
    fn publish_belief(
        &self,
        ticket: Ticket,
        wid: u32,
        focus: KeyFocus,
        confirmed_at: std::time::Instant,
    );
}

struct LiveKeyEnv;

/// Apps that did not show a window as key after the short record form. A
/// reused pid only costs it the short attempt's saving, never correctness.
static NEEDS_FULL_RECORDS: std::sync::Mutex<Vec<pid_t>> = std::sync::Mutex::new(Vec::new());

impl KeyEnv for LiveKeyEnv {
    fn key_focus(&self, pid: pid_t) -> KeyFocus {
        key_focus_of_pid(pid)
    }
    fn window_order(&self, pid: pid_t) -> Vec<u32> {
        app_window_order(pid)
    }
    fn post_records(
        &self,
        pid: pid_t,
        current: Option<u32>,
        target: u32,
        style: RecordStyle,
    ) -> Option<RecordPosts> {
        post_key_records(pid, current, target, style)
    }
    fn await_focus(
        &self,
        pid: pid_t,
        target: u32,
        total: std::time::Duration,
        mode: FocusPollMode,
    ) {
        await_key_focus_within(pid, target, total, mode);
    }
    fn needs_full_records(&self, pid: pid_t) -> bool {
        NEEDS_FULL_RECORDS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&pid)
    }
    fn note_needs_full_records(&self, pid: pid_t) {
        let mut apps = NEEDS_FULL_RECORDS.lock().unwrap_or_else(|e| e.into_inner());
        if !apps.contains(&pid) {
            if apps.len() >= 256 {
                apps.remove(0);
            }
            apps.push(pid);
        }
    }
    fn set_focused_window(&self, pid: pid_t, wid: u32) -> bool {
        set_app_focused_window(pid, wid)
    }
    fn belief_match(&self, pid: pid_t, wid: u32) -> BeliefMatch {
        BELIEFS.lookup(
            pid,
            crate::ax::enablement::process_start_stamp(pid),
            wid,
            std::time::Instant::now(),
            focus_generation(),
        )
    }
    fn belief_ticket(&self, pid: pid_t) -> Option<Ticket> {
        let stamp = crate::ax::enablement::process_start_stamp(pid)?;
        let _ = BELIEFS.begin(pid, stamp, focus_generation());
        if !super::key_belief_observer::ensure_observer(pid, stamp) {
            return None;
        }
        Some(BELIEFS.begin(pid, stamp, focus_generation()))
    }
    fn belief_now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }
    fn publish_belief(
        &self,
        ticket: Ticket,
        wid: u32,
        focus: KeyFocus,
        confirmed_at: std::time::Instant,
    ) {
        publish_belief_with(
            wid,
            focus,
            || crate::ax::enablement::process_start_stamp(ticket.pid) == Some(ticket.stamp),
            || super::key_belief_ax::is_proven_standard_window(ticket, wid, focus.window),
            || {
                let _ = BELIEFS.publish(ticket, wid, false, confirmed_at, focus_generation());
            },
        );
    }
}

fn publish_belief_with(
    wid: u32,
    focus: KeyFocus,
    process_matches: impl FnOnce() -> bool,
    standard_window: impl FnOnce() -> bool,
    publish: impl FnOnce(),
) {
    if focus.window != Some(wid)
        || focus.element_window != Some(wid)
        || !process_matches()
        || !standard_window()
    {
        return;
    }
    publish();
}

/// Hand key inside `pid` back from `target` to `previous`. Whether the app is
/// frontmost does not matter: the records go to `pid` only.
fn run_hand_back(pid: pid_t, target: u32, primed: KeyPrime, previous: u32) -> HandBack {
    run_hand_back_with(&LiveKeyEnv, pid, target, primed, previous)
}

fn run_hand_back_with(
    env: &dyn KeyEnv,
    pid: pid_t,
    target: u32,
    primed: KeyPrime,
    previous: u32,
) -> HandBack {
    let confirmed = || {
        env.await_focus(
            pid,
            previous,
            KEY_RECORD_SETTLE + std::time::Duration::from_millis(200),
            FocusPollMode::Full,
        );
        env.key_focus(pid).routes_to(previous)
    };
    match primed {
        KeyPrime::AlreadyKey => HandBack::NotMoved,
        KeyPrime::KeyRecords => {
            // The short form first, like entering; the full sequence when the
            // app does not show the previous window as key after it.
            if !env.needs_full_records(pid) {
                let fast = env.post_records(pid, Some(target), previous, RecordStyle::Fast);
                if fast.is_some_and(RecordPosts::complete) {
                    env.await_focus(pid, previous, FAST_CONFIRM, FocusPollMode::Fast);
                    if env.key_focus(pid).routes_to(previous) {
                        return HandBack::Restored;
                    }
                }
            }
            hand_back_outcome(
                env.post_records(pid, Some(target), previous, RecordStyle::Full),
                confirmed,
            )
        }
        KeyPrime::AxFocusedWindow => {
            if env.set_focused_window(pid, previous) && confirmed() {
                HandBack::Restored
            } else {
                HandBack::NotConfirmed
            }
        }
    }
}

impl WindowKeyScope {
    /// How key was confirmed on entry.
    pub fn primed(&self) -> KeyPrime {
        self.primed
    }

    pub(crate) fn used_belief(&self) -> bool {
        self.from_belief
    }

    /// What entering posted or wrote inside the app.
    pub fn entry(&self) -> KeyEntry {
        self.entry
    }

    /// Start handing key back and return at once with
    /// [`HandBack::Deferred`] (or why there is nothing to hand back).
    pub fn finish(mut self) -> HandBack {
        let outcome = self.hand_back_now();
        self.done = true;
        outcome
    }

    fn hand_back_now(&mut self) -> HandBack {
        if self.primed == KeyPrime::AlreadyKey {
            return HandBack::NotMoved;
        }
        let Some(previous) = restore_target(
            self.target,
            self.previous,
            &self.order_before,
            &(self.ops.window_order)(self.pid),
        ) else {
            drop(self.live.take());
            return HandBack::NoTarget;
        };
        let live = self.live.take();
        spawn_hand_back(self.ops, self.pid, self.target, self.primed, previous, live)
    }
}

/// Hand key back to `previous` on its own thread. The records' pauses and
/// the confirmation read are not part of the action, so nothing waits for
/// them; the next scope's entry does ([`await_key_restores`]). A hand-back
/// that does not complete is logged, never turned into a refusal.
fn spawn_hand_back(
    ops: ScopeOps,
    pid: pid_t,
    target: u32,
    primed: KeyPrime,
    previous: u32,
    live: Option<LiveKeyScope>,
) -> HandBack {
    note_focus_change();
    *KEY_RESTORES.0.lock().unwrap_or_else(|e| e.into_inner()) += 1;
    let restore = move || {
        let outcome = (ops.hand_back)(pid, target, primed, previous);
        if outcome != HandBack::Restored {
            tracing::debug!(pid, target, ?outcome, "key hand-back did not complete");
        }
        note_focus_change();
        drop(live);
        key_restore_done();
    };
    match std::thread::Builder::new()
        .name("cua-key-restore".into())
        .spawn(restore)
    {
        Ok(_) => HandBack::Deferred,
        Err(error) => {
            tracing::warn!("key restore thread failed to start: {error}");
            note_focus_change();
            key_restore_done();
            HandBack::NotConfirmed
        }
    }
}

/// The window key goes back to. Normally the previous key window. LibreOffice
/// hides a document's modeless dialog (Find & Replace) while another of its
/// documents is key, and re-shows it, raising its document, when that
/// document is key again. When the previous key window was such a dialog it
/// is gone now, so key goes to the frontmost window that was on screen
/// before: its document, which brings the dialog and the window order back.
fn restore_target(
    target: u32,
    previous: Option<u32>,
    order_before: &[u32],
    on_screen_now: &[u32],
) -> Option<u32> {
    let present = |w: &u32| *w != target && on_screen_now.contains(w);
    previous
        .filter(present)
        .or_else(|| order_before.iter().copied().find(present))
}

/// `pid`'s on-screen layer-0 windows, front to back.
fn app_window_order(pid: pid_t) -> Vec<u32> {
    // Order, owner and layer only: the Space snapshot is not read.
    let mut windows: Vec<_> = crate::windows::visible_windows_without_spaces()
        .into_iter()
        .filter(|w| w.pid == pid && w.layer == 0 && w.is_on_screen)
        .collect();
    windows.sort_by_key(|w| std::cmp::Reverse(w.z_index));
    windows.into_iter().map(|w| w.window_id).collect()
}

impl Drop for WindowKeyScope {
    fn drop(&mut self) {
        if self.done || self.primed == KeyPrime::AlreadyKey {
            return;
        }
        let _ = self.hand_back_now();
    }
}

/// [`WindowKeyScope`]s that moved key focus, counted from their creation
/// until their hand-back has run. A scope can outlive the mutation lease of
/// the action that made it (declaration order), so a primed-key release
/// checks this count, not only the lease.
static LIVE_KEY_SCOPES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// One count in [`LIVE_KEY_SCOPES`], released when dropped.
struct LiveKeyScope;

impl LiveKeyScope {
    fn new() -> Self {
        LIVE_KEY_SCOPES.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        LiveKeyScope
    }
}

impl Drop for LiveKeyScope {
    fn drop(&mut self) {
        LIVE_KEY_SCOPES.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Hand-backs of [`WindowKeyScope`]s still running. Entering a new scope
/// waits for none to be left, so it never reads or moves key focus
/// mid-restore.
static KEY_RESTORES: (std::sync::Mutex<usize>, std::sync::Condvar) =
    (std::sync::Mutex::new(0), std::sync::Condvar::new());

fn key_restore_done() {
    let mut pending = KEY_RESTORES.0.lock().unwrap_or_else(|e| e.into_inner());
    *pending = pending.saturating_sub(1);
    KEY_RESTORES.1.notify_all();
}

/// Whether a key scope is live or its hand-back is running now. Never waits.
fn key_restores_pending() -> bool {
    LIVE_KEY_SCOPES.load(std::sync::atomic::Ordering::Acquire) > 0
        || *KEY_RESTORES.0.lock().unwrap_or_else(|e| e.into_inner()) > 0
}

/// Wait (at most 2 s) until no key hand-back is running. `false` when one
/// is still running: the caller must not move focus or send input then.
pub fn await_key_restores() -> bool {
    let pending = KEY_RESTORES.0.lock().unwrap_or_else(|e| e.into_inner());
    let (pending, _) = KEY_RESTORES
        .1
        .wait_timeout_while(pending, std::time::Duration::from_secs(2), |n| *n > 0)
        .unwrap_or_else(|e| e.into_inner());
    *pending == 0
}

/// Advanced by every focus record, focused-window write or foreground turn
/// the driver makes. Two equal reads mean the driver moved no app's key
/// focus in between (a person's or an app's own changes are not counted;
/// see the activation count in `focus_steal`).
static FOCUS_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The current [`FOCUS_GENERATION`].
pub fn focus_generation() -> u64 {
    FOCUS_GENERATION.load(std::sync::atomic::Ordering::Acquire)
}

pub(crate) fn note_focus_change() {
    FOCUS_GENERATION.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
}

// ── Primed key focus and its release ─────────────────────────────────────────

/// A window the driver made key inside a background app with a focus record
/// only (no hand-back): a background press, a menu, a pixel click. Kept so
/// the driver lets it go again instead of leaving the app holding key focus
/// behind the person's window. One entry per app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PrimedKey {
    pub(crate) pid: pid_t,
    pub(crate) window_id: u32,
    pub(crate) at: std::time::Instant,
    /// Let go at the next chance (another app was primed, the session ended).
    pub(crate) release_requested: bool,
}

/// How long a primed key window is held for the next action on it.
pub(crate) const PRIMED_KEY_HOLD: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PrimedKeyVerdict {
    /// Still held, for the next action or until an action on the app ends.
    Keep,
    /// Let it go: post the defocus record.
    Release,
    /// Forget it and post nothing: the app is in front (its key window is
    /// the person's now), or WindowServer cannot say which app is.
    Forget,
}

/// What to do with `key` now.
///
/// - `live_front`: whether WindowServer reports the key's app frontmost
///   right now ([`front_process_matches`], not AppKit's possibly stale
///   frontmost app); `None` when it cannot say.
/// - `idle_ms`: time since the person's last physical input; `None` when
///   unknown, which never releases early.
/// - `action_in_flight`: an action holds the app's mutation lease and may
///   rely on the key window, so a release waits for it.
pub(crate) fn primed_key_verdict(
    key: &PrimedKey,
    live_front: Option<bool>,
    idle_ms: Option<u64>,
    now: std::time::Instant,
    action_in_flight: bool,
) -> PrimedKeyVerdict {
    let age = now.saturating_duration_since(key.at);
    let input_since = idle_ms.is_some_and(|idle| u128::from(idle) < age.as_millis());
    let wanted = key.release_requested || input_since || age >= PRIMED_KEY_HOLD;
    match live_front {
        Some(true) => PrimedKeyVerdict::Forget,
        _ if !wanted || action_in_flight => PrimedKeyVerdict::Keep,
        None => PrimedKeyVerdict::Forget,
        Some(false) => PrimedKeyVerdict::Release,
    }
}

/// Whether ending a session lets go of the primed key windows. They carry
/// no session owner, so only the last live session's end does; otherwise
/// the person's input or [`PRIMED_KEY_HOLD`] lets them go.
pub(crate) fn release_on_session_end(live_sessions_left: usize) -> bool {
    live_sessions_left == 0
}

static PRIMED_KEYS: std::sync::Mutex<Vec<PrimedKey>> = std::sync::Mutex::new(Vec::new());
static PRIMED_KEY_WATCH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn primed_keys() -> std::sync::MutexGuard<'static, Vec<PrimedKey>> {
    PRIMED_KEYS.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
pub(crate) fn primed_keys_for_test() -> Vec<PrimedKey> {
    primed_keys().clone()
}

/// Apply every verdict. Runs with the registry locked, so a priming and a
/// release never interleave: a release posted here can never undo a priming
/// recorded after it. A release also holds the app's mutation lease (taken
/// without waiting) from the front check through the post, so no action on
/// that app runs meanwhile; an app with an action in flight keeps its entry.
fn sweep_primed_keys(keys: &mut Vec<PrimedKey>) {
    let now = std::time::Instant::now();
    let idle_ms = crate::tools::physical_input_idle_ms();
    keys.retain(|key| {
        // Nothing to decide unless a release is wanted; the front check and
        // the lease are only taken then.
        if primed_key_verdict(key, Some(false), idle_ms, now, false) == PrimedKeyVerdict::Keep {
            return true;
        }
        let Some(_lease) = crate::background_mutation::try_acquire(key.pid) else {
            return true;
        };
        // A key hand-back outlives its action's lease and may focus this
        // window again after a release: wait for it as for an action.
        match primed_key_verdict(
            key,
            front_process_matches(key.pid, key.window_id),
            idle_ms,
            now,
            key_restores_pending(),
        ) {
            PrimedKeyVerdict::Keep => true,
            PrimedKeyVerdict::Forget => false,
            PrimedKeyVerdict::Release => {
                post_key_release(*key);
                false
            }
        }
    });
}

/// Record that `window_id` of `pid` was made key by a focus record. It
/// replaces the app's previous entry (the new record already moved key inside
/// the app); every other app's primed window is let go.
pub(crate) fn note_primed_key(pid: pid_t, window_id: u32) {
    {
        let mut keys = primed_keys();
        keys.retain(|key| key.pid != pid);
        for key in keys.iter_mut() {
            key.release_requested = true;
        }
        sweep_primed_keys(&mut keys);
        keys.push(PrimedKey {
            pid,
            window_id,
            at: std::time::Instant::now(),
            release_requested: false,
        });
    }
    if !PRIMED_KEY_WATCH.swap(true, std::sync::atomic::Ordering::AcqRel) {
        let spawned = std::thread::Builder::new()
            .name("cua-primed-key".into())
            .spawn(watch_primed_keys);
        if spawned.is_err() {
            PRIMED_KEY_WATCH.store(false, std::sync::atomic::Ordering::Release);
        }
    }
}

/// Whether `window_id` of `pid` is still the primed key window and the
/// person has not touched the keyboard or pointer since it was primed.
pub(crate) fn primed_key_still_held(pid: pid_t, window_id: u32) -> bool {
    let keys = primed_keys();
    keys.iter().any(|key| {
        (key.pid, key.window_id) == (pid, window_id)
            && primed_key_verdict(
                key,
                Some(false),
                crate::tools::physical_input_idle_ms(),
                std::time::Instant::now(),
                false,
            ) == PrimedKeyVerdict::Keep
    })
}

/// A session ended (end_session or a closed transport) and
/// `live_sessions_left` remain: let the primed key windows go when it was the
/// last one. A window whose app still has an action in flight goes when that
/// action ends.
pub(crate) fn release_primed_keys_on_session_end(live_sessions_left: usize) {
    if !release_on_session_end(live_sessions_left) {
        return;
    }
    let mut keys = primed_keys();
    for key in keys.iter_mut() {
        key.release_requested = true;
    }
    sweep_primed_keys(&mut keys);
}

/// Re-applies the verdicts every 100 ms until no primed window is left.
fn watch_primed_keys() {
    loop {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let mut keys = primed_keys();
        sweep_primed_keys(&mut keys);
        if keys.is_empty() {
            PRIMED_KEY_WATCH.store(false, std::sync::atomic::Ordering::Release);
            return;
        }
    }
}

/// The defocus record (0x0d, 0x8a=2) for the primed window, to its own app
/// only. The person's front app receives nothing. Advances the focus
/// generation, so a reuse of the priming is never trusted after it.
fn post_key_release(key: PrimedKey) {
    note_focus_change();
    let Some(post) = post_event_record_to_fn() else {
        return;
    };
    let mut psn = [0u8; 8];
    if !get_process_psn_for_window(key.window_id, key.pid, &mut psn) {
        return;
    }
    let mut defocus = focus_record(key.window_id);
    defocus[0x8A] = 0x02;
    note_post_status(unsafe { post(psn.as_ptr() as *const c_void, defocus.as_ptr()) });
    tracing::debug!(
        pid = key.pid,
        window_id = key.window_id,
        "released the primed key window"
    );
}

fn set_app_focused_window(pid: pid_t, wid: u32) -> bool {
    note_focus_change();
    use crate::ax::bindings::{
        ax_get_window_id, copy_ax_windows, AXUIElementCreateApplication,
        AXUIElementSetAttributeValue,
    };
    use core_foundation::{
        base::{CFRelease, TCFType},
        string::CFString,
    };
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return false;
        }
        let attr = CFString::new("AXFocusedWindow");
        let mut ok = false;
        for window in copy_ax_windows(app) {
            if !ok && ax_get_window_id(window) == Some(wid) {
                ok = AXUIElementSetAttributeValue(
                    app,
                    attr.as_concrete_TypeRef(),
                    window as core_foundation::base::CFTypeRef,
                ) == 0;
            }
            CFRelease(window as _);
        }
        CFRelease(app as _);
        ok
    }
}

/// The app's focus as Accessibility reports it.
fn key_focus_of_pid(pid: pid_t) -> KeyFocus {
    KeyFocus {
        window: crate::ax::bindings::focused_window_id_of_pid(pid),
        element_window: crate::ax::bindings::focused_element_window_id_of_pid(pid),
    }
}

/// Whether keys posted to `pid` go to `target` now (its AXFocusedWindow).
/// Read-only.
pub(crate) fn window_routes_keys(pid: pid_t, target: u32) -> bool {
    key_focus_of_pid(pid).routes_to(target)
}

/// Settle time between the key-window records. Without it the app can drop
/// the focus record while it is still handling the defocus (measured on
/// LibreOffice 26.8: about half the switches failed without a pause).
const KEY_RECORD_SETTLE: std::time::Duration = std::time::Duration::from_millis(120);

/// Move key status inside `pid` from `current` to `target` without activating
/// the app, raising a window or touching the pointer. Every record goes to
/// `pid` only; the user's front app receives nothing and keeps key focus.
///
/// Sequence, measured on LibreOffice (VCL) where each step alone is ignored:
/// 1. defocus record (0x0d, 0x8a=2) for the app's current key window;
/// 2. focus record (0x0d, 0x8a=1) for `target`;
/// 3. the make-key record pair (0x01/0x02) for `target`, as
///    [`make_exact_window_key`] sends after fronting, here without fronting.
///
/// Returns `true` when every record was posted. Whether the app acted on them
/// is for the caller to confirm.
pub fn post_window_key_records(pid: pid_t, current: Option<u32>, target: u32) -> bool {
    crate::window_change_detector::end_tail();
    post_key_records(pid, current, target, RecordStyle::Full).is_some_and(RecordPosts::complete)
}

/// Which key-window records a move of key focus posts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordStyle {
    /// Defocus, focus and the make-key pair with a short pause after the
    /// first two ([`FAST_RECORD_PAUSE`]). Measured on a two-window AppKit
    /// app: every switch took it with pauses of 5 ms and up, while leaving
    /// the defocus record out failed from the second switch on. The caller
    /// confirms by polling the app's focus ([`FAST_CONFIRM`]) and falls back to
    /// [`RecordStyle::Full`] when it does not show the window as key.
    Fast,
    /// The same records with [`KEY_RECORD_SETTLE`] after the first two (an app
    /// can drop the focus record while it is still handling the defocus:
    /// about half the switches failed on LibreOffice 26.8 without it).
    Full,
}

/// Pause between the first records of the short form.
const FAST_RECORD_PAUSE: std::time::Duration = std::time::Duration::from_millis(10);

/// How long the short form is given to show the window as key before the
/// full sequence runs.
const FAST_CONFIRM: std::time::Duration = std::time::Duration::from_millis(40);

impl RecordStyle {
    fn pause(self) -> std::time::Duration {
        match self {
            RecordStyle::Fast => FAST_RECORD_PAUSE,
            RecordStyle::Full => KEY_RECORD_SETTLE,
        }
    }
}

/// The posts of one key-record sequence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RecordPosts {
    /// Post calls that succeeded.
    pub(crate) posted: u8,
    /// Post calls that returned failure (delivery unknown).
    pub(crate) failed: u8,
}

impl RecordPosts {
    pub(crate) fn complete(self) -> bool {
        self.failed == 0
    }
}

/// Post the key-window records (see [`post_window_key_records`]) through
/// `send`. Pure over its closures, so the sequence is tested without a
/// desktop.
fn post_record_sequence(
    current: Option<u32>,
    target: u32,
    mut send: impl FnMut(&[u8; 0xF8]) -> bool,
    mut settle: impl FnMut(),
) -> RecordPosts {
    let mut records: Vec<([u8; 0xF8], bool)> = Vec::with_capacity(4);
    if let Some(current) = current {
        let mut defocus = focus_record(current);
        defocus[0x8A] = 0x02;
        records.push((defocus, true));
    }
    let mut focus = focus_record(target);
    focus[0x8A] = 0x01;
    records.push((focus, true));
    for event_kind in [0x01, 0x02] {
        records.push((make_key_window_record(target, event_kind), false));
    }
    let mut posts = RecordPosts::default();
    for (record, settle_after) in records {
        if send(&record) {
            posts.posted += 1;
        } else {
            posts.failed += 1;
        }
        if settle_after {
            settle();
        }
    }
    posts
}

/// `None` when the records cannot be addressed (no SPI, no PSN for
/// `target`): nothing was posted.
fn post_key_records(
    pid: pid_t,
    current: Option<u32>,
    target: u32,
    style: RecordStyle,
) -> Option<RecordPosts> {
    note_focus_change();
    let post = post_event_record_to_fn()?;
    let mut psn = [0u8; 8];
    if !get_process_psn_for_window(target, pid, &mut psn) {
        return None;
    }
    Some(post_record_sequence(
        current,
        target,
        |record| note_post_status(unsafe { post(psn.as_ptr() as *const c_void, record.as_ptr()) }),
        || std::thread::sleep(style.pause()),
    ))
}

/// Hand key back inside `pid` after a [`WindowKeyScope`]: the same records
/// as [`post_window_key_records`], without ending the previous action's
/// detached window watch. The hand-back belongs to the action that just
/// returned, and its post-action focus hold must outlast it.
#[cfg(test)]
pub(crate) fn hand_key_back(pid: pid_t, current: Option<u32>, target: u32) -> bool {
    post_key_records(pid, current, target, RecordStyle::Full).is_some_and(RecordPosts::complete)
}

/// Wait until the app's focus has settled on `target`, for at most `budget`.
/// The make-key records are handled on the app's main thread, so the change
/// trails the post; a never-active LibreOffice shows it last, when a focused
/// element appears. The caller re-reads and decides.
///
/// Polled from the start: a read that still shows the old focus keeps the
/// wait going, so no fixed settle is needed before the first read.
fn await_key_focus(pid: pid_t, target: u32, budget: std::time::Duration) {
    await_key_focus_within(pid, target, KEY_RECORD_SETTLE + budget, FocusPollMode::Full);
}

#[derive(Clone, Copy)]
enum FocusPollMode {
    Fast,
    Full,
}

/// [`await_key_focus`] with the whole wait given: at most `total`, polled in
/// 2 ms steps for fast confirmation and 4 ms for the full sequence,
/// returning as soon as the app's focus has settled on `target`.
fn await_key_focus_within(
    pid: pid_t,
    target: u32,
    total: std::time::Duration,
    mode: FocusPollMode,
) {
    await_key_focus_with(
        total,
        mode,
        || {
            if matches!(mode, FocusPollMode::Fast) {
                crate::ax::bindings::focused_window_id_of_pid(pid) == Some(target)
            } else {
                key_focus_of_pid(pid).settled_on(target)
            }
        },
        std::time::Instant::now,
        std::thread::sleep,
    );
}

fn await_key_focus_with(
    total: std::time::Duration,
    mode: FocusPollMode,
    mut settled: impl FnMut() -> bool,
    mut now: impl FnMut() -> std::time::Instant,
    mut sleep: impl FnMut(std::time::Duration),
) {
    let deadline = now() + total;
    let step = if matches!(mode, FocusPollMode::Fast) {
        std::time::Duration::from_millis(2)
    } else {
        std::time::Duration::from_millis(4)
    };
    while !settled() {
        let remaining = deadline.saturating_duration_since(now());
        if remaining.is_zero() {
            break;
        }
        sleep(step.min(remaining));
    }
}

/// Make `target_wid` the app's key window (no activation, no raise, no
/// pointer), confirmed by re-reading the app's focus, and return a guard that
/// hands key back. `Err` when it cannot be confirmed.
pub fn enter_exact_window_key(pid: pid_t, target_wid: u32) -> Result<WindowKeyScope, String> {
    enter_scope(pid, target_wid).map_err(|error| error.reason)
}

/// [`enter_exact_window_key`] under the name its callers that also hold a
/// minimized-window restore, a mutation lease or a menu route use. It behaves
/// the same: whether the app is frontmost does not change what it does, and
/// the hand-back runs on its own thread after the call returns.
pub fn enter_exact_window_key_inline(
    pid: pid_t,
    target_wid: u32,
) -> Result<WindowKeyScope, String> {
    enter_scope(pid, target_wid).map_err(|error| error.reason)
}

/// Why entering a key scope failed, with what entering did before it
/// stopped (records posted, a focused-window write) and how key was handed
/// back after it. `reason` is the same text the plain entry functions return.
#[derive(Debug)]
pub struct ScopeEntryError {
    pub reason: String,
    pub entry: KeyEntry,
    pub hand_back: Option<HandBack>,
}

/// [`enter_exact_window_key_inline`] with the failed entry's side effects
/// kept structured for the caller's receipt.
pub fn enter_exact_window_key_inline_detailed(
    pid: pid_t,
    target_wid: u32,
) -> Result<WindowKeyScope, ScopeEntryError> {
    enter_scope(pid, target_wid)
}

fn untouched(reason: String) -> ScopeEntryError {
    ScopeEntryError {
        reason,
        entry: KeyEntry::default(),
        hand_back: None,
    }
}

fn enter_scope(pid: pid_t, target_wid: u32) -> Result<WindowKeyScope, ScopeEntryError> {
    enter_scope_with(
        &LiveKeyEnv,
        LIVE_SCOPE_OPS,
        pid,
        target_wid,
        FastPathEligibility::Ineligible,
    )
}

#[derive(Clone, Copy)]
enum FastPathEligibility {
    Ineligible,
    Eligible,
}

pub(crate) fn enter_untargeted_key_scope(
    pid: pid_t,
    target_wid: u32,
) -> Result<WindowKeyScope, ScopeEntryError> {
    enter_scope_with(
        &LiveKeyEnv,
        LIVE_SCOPE_OPS,
        pid,
        target_wid,
        FastPathEligibility::Eligible,
    )
}

pub(crate) fn invalidate_key_belief(pid: pid_t) {
    match crate::ax::enablement::process_start_stamp(pid) {
        Some(stamp) => BELIEFS.invalidate(pid, stamp),
        None => BELIEFS.forget(pid),
    }
}

fn enter_scope_with(
    env: &dyn KeyEnv,
    ops: ScopeOps,
    pid: pid_t,
    target_wid: u32,
    eligibility: FastPathEligibility,
) -> Result<WindowKeyScope, ScopeEntryError> {
    // Wait for the previous scope's hand-back to finish; refuse (nothing
    // sent) when it does not.
    if !await_key_restores() {
        return Err(untouched(format!(
            "window {target_wid}: the previous key hand-back in this app did not finish within \
             2 s, so no keys were sent"
        )));
    }
    // The app being frontmost changes nothing here: it already owns key
    // focus, so a window that is key is used as it is and any other window
    // is made key with records addressed to this app only.
    let initial_entry = KeyEntry::default();
    crate::goat::action_latency::key_belief(
        matches!(eligibility, FastPathEligibility::Eligible),
        false,
    );
    if matches!(eligibility, FastPathEligibility::Eligible) {
        let cached = env.belief_match(pid, target_wid);
        let usable = matches!(
            cached,
            BeliefMatch::FreshKeyAndActive | BeliefMatch::FreshKeyInactive
        );
        if usable {
            crate::goat::action_latency::key_belief(true, true);
            return Ok(WindowKeyScope {
                pid,
                target: target_wid,
                previous: Some(target_wid),
                order_before: Vec::new(),
                primed: KeyPrime::AlreadyKey,
                from_belief: true,
                entry: initial_entry,
                done: false,
                ops,
                live: None,
            });
        }
    }
    let entry = std::cell::Cell::new(initial_entry);
    let ticket = matches!(eligibility, FastPathEligibility::Eligible)
        .then(|| env.belief_ticket(pid))
        .flatten();
    let confirmed_at = env.belief_now();
    let before = env.key_focus(pid);
    let previous = before.window;
    // The window order is only used to pick the window key goes back to, and
    // nothing is handed back when the target is already key: skip the
    // all-windows enumeration then.
    let order_before = if before.settled_on(target_wid) {
        Vec::new()
    } else {
        env.window_order(pid)
    };
    // The first confirmation reads the focus just read above, not again.
    let mut first_read = Some(before);
    // Posts one record sequence and notes what reached the app, for the
    // receipt of a refusal.
    let post_noted = |style: RecordStyle| {
        crate::window_change_detector::end_tail();
        let posts = env.post_records(pid, previous, target_wid, style);
        let mut noted = entry.get();
        if let Some(posts) = posts {
            noted.records_posted = noted.records_posted.saturating_add(posts.posted);
            noted.records_uncertain |= posts.failed > 0;
        }
        entry.set(noted);
        posts.is_some_and(RecordPosts::complete)
    };
    let fast_tried = std::cell::Cell::new(false);
    let primed = match prime_exact_window_key(
        target_wid,
        || first_read.take().unwrap_or_else(|| env.key_focus(pid)),
        || {
            if env.needs_full_records(pid) {
                return false;
            }
            fast_tried.set(true);
            let ok = post_noted(RecordStyle::Fast);
            if ok {
                // The app is given FAST_CONFIRM, polled in 2 ms steps, to show
                // the window as key.
                env.await_focus(pid, target_wid, FAST_CONFIRM, FocusPollMode::Fast);
            }
            ok
        },
        || {
            if fast_tried.get() {
                // The short form did not do it for this app: its next entry
                // and hand-back go straight to the full sequence.
                env.note_needs_full_records(pid);
            }
            let ok = post_noted(RecordStyle::Full);
            if ok {
                env.await_focus(
                    pid,
                    target_wid,
                    KEY_RECORD_SETTLE + std::time::Duration::from_millis(400),
                    FocusPollMode::Full,
                );
            }
            ok
        },
        || {
            let mut noted = entry.get();
            noted.ax_write = true;
            entry.set(noted);
            let ok = env.set_focused_window(pid, target_wid);
            if ok {
                env.await_focus(
                    pid,
                    target_wid,
                    KEY_RECORD_SETTLE + std::time::Duration::from_millis(200),
                    FocusPollMode::Full,
                );
            }
            ok
        },
    ) {
        Ok(primed) => primed,
        Err(error) => {
            note_focus_change();
            // Records or a focused-window write may still have moved key
            // inside the app: hand it back like a scope's hand-back does, and
            // say what happened.
            let noted = entry.get();
            let hand_back = entry_hand_back_kind(noted).map(|primed| {
                match restore_target(target_wid, previous, &order_before, &env.window_order(pid)) {
                    Some(previous) => spawn_hand_back(ops, pid, target_wid, primed, previous, None),
                    None => HandBack::NoTarget,
                }
            });
            let disclosure = scope_disclosure(noted, hand_back);
            return Err(ScopeEntryError {
                reason: if disclosure.is_empty() {
                    error
                } else {
                    format!("{error}. Before refusing: {disclosure}")
                },
                entry: noted,
                hand_back,
            });
        }
    };
    if matches!(eligibility, FastPathEligibility::Eligible) && primed == KeyPrime::AlreadyKey {
        if let Some(ticket) = ticket {
            env.publish_belief(ticket, target_wid, before, confirmed_at);
        }
    }
    Ok(WindowKeyScope {
        pid,
        target: target_wid,
        previous,
        order_before,
        live: (primed != KeyPrime::AlreadyKey).then(LiveKeyScope::new),
        primed,
        from_belief: false,
        entry: entry.get(),
        done: false,
        ops,
    })
}

/// How to hand key back after a failed entry: records when any record may
/// have reached the app, the focused-window write when only that ran,
/// nothing when entering touched nothing.
fn entry_hand_back_kind(entry: KeyEntry) -> Option<KeyPrime> {
    if entry.records_posted > 0 || entry.records_uncertain {
        Some(KeyPrime::KeyRecords)
    } else if entry.ax_write {
        Some(KeyPrime::AxFocusedWindow)
    } else {
        None
    }
}

// ── NSMenu shortcut activation SPIs ──────────────────────────────────────────

/// `OSStatus SLPSSetFrontProcessWithOptions(const void *psn, uint32_t windowID, uint32_t options)`
type SetFrontProcessFn = unsafe extern "C" fn(*const c_void, u32, u32) -> i32;

/// `OSStatus SLSGetWindowOwner(uint32_t cid, uint32_t wid, uint32_t *out_cid)`
type GetWindowOwnerFn = unsafe extern "C" fn(u32, u32, *mut u32) -> i32;

/// `OSStatus SLSGetConnectionPSN(uint32_t cid, void *psn)`
type GetConnectionPSNFn = unsafe extern "C" fn(u32, *mut c_void) -> i32;

// ── WindowServer connection health (CUA-1777) ───────────────────────────────

/// `SLPSPostEventRecordTo` result for "invalid connection". The daemon gets it
/// when it runs as its own responsible process (a bare binary outside
/// CuaDriver.app, which relaunches itself disclaimed): the process has no
/// usable WindowServer connection for the target, so every no-raise focus
/// record is dropped.
pub const WINDOWSERVER_INVALID_CONNECTION: i32 = 1002;

static WINDOWSERVER_CONNECTION_INVALID: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Record the status of one `SLPSPostEventRecordTo` and report success. A
/// 1002 is remembered for the life of the process so tools and `health_report`
/// can fail loudly instead of continuing without the focus step.
pub(crate) fn note_post_status(status: i32) -> bool {
    if status == WINDOWSERVER_INVALID_CONNECTION {
        WINDOWSERVER_CONNECTION_INVALID.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    status == 0
}

/// True once any focus-record post returned the invalid-connection status.
pub fn windowserver_connection_invalid() -> bool {
    WINDOWSERVER_CONNECTION_INVALID.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) fn reset_windowserver_connection_invalid_for_test() {
    WINDOWSERVER_CONNECTION_INVALID.store(false, std::sync::atomic::Ordering::Relaxed);
}

// ── Focus-without-raise SPIs ──────────────────────────────────────────────────

/// `OSStatus SLPSPostEventRecordTo(const void *psn, const uint8_t *bytes)`
/// Posts a 248-byte synthetic event record into the target process's Carbon
/// event queue. Build the buffer with bytes[0x04]=0xf8, bytes[0x08]=0x0d,
/// target window id at bytes 0x3c–0x3f (little-endian), focus/defocus marker
/// at bytes[0x8a] (0x01 = focus, 0x02 = defocus), all other bytes zero.
type PostEventRecordToFn = unsafe extern "C" fn(*const c_void, *const u8) -> i32;

/// `OSStatus _SLPSGetFrontProcess(void *psn)`
/// Writes the current frontmost process's 8-byte PSN into `psn`.
type GetFrontProcessFn = unsafe extern "C" fn(*mut c_void) -> i32;

/// `OSStatus GetProcessForPID(pid_t, void *psn)`
/// Deprecated but still resolves. Writes the target pid's 8-byte PSN.
type GetProcessForPIDFn = unsafe extern "C" fn(pid_t, *mut c_void) -> i32;

/// Factory: `+[SLSEventAuthenticationMessage messageWithEventRecord:pid:version:]`
/// ObjC send: `(id self, SEL _cmd, void* record, int32 pid, uint32 version) -> id`
type FactoryMsgSendFn = unsafe extern "C" fn(
    *mut c_void, // Class (receiver)
    *mut c_void, // SEL
    *mut c_void, // SLSEventRecord*
    c_int,       // pid
    c_uint,      // version
) -> *mut c_void;

// ── Symbol resolution ──────────────────────────────────────────────────────

/// Load SkyLight once so all dlsym lookups via RTLD_DEFAULT find it.
fn ensure_skylight_loaded() {
    static LOADED: OnceLock<()> = OnceLock::new();
    LOADED.get_or_init(|| {
        let path = b"/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight\0";
        unsafe {
            libc::dlopen(
                path.as_ptr() as *const c_char,
                libc::RTLD_LAZY | libc::RTLD_GLOBAL,
            );
        }
    });
}

/// Look up a symbol by name via RTLD_DEFAULT (after loading SkyLight).
/// Returns `None` when the symbol doesn't resolve.
fn find_sym(name: &[u8]) -> Option<*mut c_void> {
    ensure_skylight_loaded();
    let ptr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr() as *const c_char) };
    if ptr.is_null() {
        None
    } else {
        Some(ptr)
    }
}

/// Reinterpret a raw symbol pointer as a function pointer of type `T`.
/// Safety: caller guarantees T matches the symbol's actual signature.
unsafe fn as_fn<T: Copy>(ptr: *mut c_void) -> T {
    std::mem::transmute_copy::<*mut c_void, T>(&ptr)
}

// ── Lazily-resolved handles ────────────────────────────────────────────────

fn post_to_pid_fn() -> Option<PostToPidFn> {
    static SYM: OnceLock<Option<PostToPidFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"SLEventPostToPid\0").map(|p| unsafe { as_fn(p) }))
}

fn set_auth_msg_fn() -> Option<SetAuthMsgFn> {
    static SYM: OnceLock<Option<SetAuthMsgFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"SLEventSetAuthenticationMessage\0").map(|p| unsafe { as_fn(p) }))
}

fn set_window_loc_fn() -> Option<SetWindowLocFn> {
    static SYM: OnceLock<Option<SetWindowLocFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"CGEventSetWindowLocation\0").map(|p| unsafe { as_fn(p) }))
}

fn set_int_field_fn() -> Option<SetIntFieldFn> {
    static SYM: OnceLock<Option<SetIntFieldFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"SLEventSetIntegerValueField\0").map(|p| unsafe { as_fn(p) }))
}

fn connection_id_fn() -> Option<ConnectionIDFn> {
    static SYM: OnceLock<Option<ConnectionIDFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"CGSMainConnectionID\0").map(|p| unsafe { as_fn(p) }))
}

fn get_active_space_fn() -> Option<GetActiveSpaceFn> {
    static SYM: OnceLock<Option<GetActiveSpaceFn>> = OnceLock::new();
    *SYM.get_or_init(|| {
        find_sym(b"SLSGetActiveSpace\0")
            .or_else(|| find_sym(b"CGSGetActiveSpace\0"))
            .map(|p| unsafe { as_fn(p) })
    })
}

fn copy_spaces_for_windows_fn() -> Option<CopySpacesForWindowsFn> {
    static SYM: OnceLock<Option<CopySpacesForWindowsFn>> = OnceLock::new();
    *SYM.get_or_init(|| {
        find_sym(b"SLSCopySpacesForWindows\0")
            .or_else(|| find_sym(b"CGSCopySpacesForWindows\0"))
            .map(|p| unsafe { as_fn(p) })
    })
}

fn copy_managed_display_for_window_fn() -> Option<CopyManagedDisplayForWindowFn> {
    static SYM: OnceLock<Option<CopyManagedDisplayForWindowFn>> = OnceLock::new();
    *SYM.get_or_init(|| {
        find_sym(b"SLSCopyManagedDisplayForWindow\0")
            .or_else(|| find_sym(b"CGSCopyManagedDisplayForWindow\0"))
            .map(|p| unsafe { as_fn(p) })
    })
}

fn managed_display_get_current_space_fn() -> Option<ManagedDisplayGetCurrentSpaceFn> {
    static SYM: OnceLock<Option<ManagedDisplayGetCurrentSpaceFn>> = OnceLock::new();
    *SYM.get_or_init(|| {
        find_sym(b"SLSManagedDisplayGetCurrentSpace\0")
            .or_else(|| find_sym(b"CGSManagedDisplayGetCurrentSpace\0"))
            .map(|p| unsafe { as_fn(p) })
    })
}

fn factory_msg_send_fn() -> Option<FactoryMsgSendFn> {
    static SYM: OnceLock<Option<FactoryMsgSendFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"objc_msgSend\0").map(|p| unsafe { as_fn(p) }))
}

fn set_front_process_fn() -> Option<SetFrontProcessFn> {
    static SYM: OnceLock<Option<SetFrontProcessFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"SLPSSetFrontProcessWithOptions\0").map(|p| unsafe { as_fn(p) }))
}

fn get_window_owner_fn() -> Option<GetWindowOwnerFn> {
    static SYM: OnceLock<Option<GetWindowOwnerFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"SLSGetWindowOwner\0").map(|p| unsafe { as_fn(p) }))
}

fn get_connection_psn_fn() -> Option<GetConnectionPSNFn> {
    static SYM: OnceLock<Option<GetConnectionPSNFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"SLSGetConnectionPSN\0").map(|p| unsafe { as_fn(p) }))
}

fn post_event_record_to_fn() -> Option<PostEventRecordToFn> {
    static SYM: OnceLock<Option<PostEventRecordToFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"SLPSPostEventRecordTo\0").map(|p| unsafe { as_fn(p) }))
}

fn get_front_process_fn() -> Option<GetFrontProcessFn> {
    static SYM: OnceLock<Option<GetFrontProcessFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"_SLPSGetFrontProcess\0").map(|p| unsafe { as_fn(p) }))
}

fn get_process_for_pid_fn() -> Option<GetProcessForPIDFn> {
    static SYM: OnceLock<Option<GetProcessForPIDFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"GetProcessForPID\0").map(|p| unsafe { as_fn(p) }))
}

/// `true` when `SLEventPostToPid` resolved.
pub fn is_available() -> bool {
    post_to_pid_fn().is_some()
}

/// `true` when the focus-without-raise SPIs resolved, including either the
/// modern window-owner PSN lookup or the deprecated pid fallback.
pub fn is_focus_without_raise_available() -> bool {
    let has_psn_lookup = (connection_id_fn().is_some()
        && get_window_owner_fn().is_some()
        && get_connection_psn_fn().is_some())
        || get_process_for_pid_fn().is_some();
    get_front_process_fn().is_some() && has_psn_lookup && post_event_record_to_fn().is_some()
}

// ── ObjC runtime helpers ───────────────────────────────────────────────────

/// Look up an ObjC class by C-string name via `objc_getClass`.
fn objc_class(name: &CStr) -> *mut c_void {
    type GetClassFn = unsafe extern "C" fn(*const c_char) -> *mut c_void;
    static SYM: OnceLock<Option<GetClassFn>> = OnceLock::new();
    let f = *SYM.get_or_init(|| find_sym(b"objc_getClass\0").map(|p| unsafe { as_fn(p) }));
    match f {
        Some(f) => unsafe { f(name.as_ptr()) },
        None => std::ptr::null_mut(),
    }
}

/// Register / look up an ObjC selector by C-string name via `sel_registerName`.
fn sel_register(name: &CStr) -> *mut c_void {
    type SelRegFn = unsafe extern "C" fn(*const c_char) -> *mut c_void;
    static SYM: OnceLock<Option<SelRegFn>> = OnceLock::new();
    let f = *SYM.get_or_init(|| find_sym(b"sel_registerName\0").map(|p| unsafe { as_fn(p) }));
    match f {
        Some(f) => unsafe { f(name.as_ptr()) },
        None => std::ptr::null_mut(),
    }
}

/// Whether `cls` actually implements `sel`, via `class_respondsToSelector`.
///
/// macOS 14 (Sonoma) compatibility guard: `SLSEventAuthenticationMessage`
/// exists on macOS 14, but `messageWithEventRecord:pid:version:` was only
/// added in macOS 15 (Sequoia). `sel_registerName` always succeeds (it just
/// interns the string), so a `!sel.is_null()` check is not enough — we must
/// confirm the class responds before calling `objc_msgSend`, or the runtime
/// raises `NSInvalidArgumentException: unrecognized selector`. See #1503.
fn class_responds_to_selector(cls: *mut c_void, sel: *mut c_void) -> bool {
    if cls.is_null() || sel.is_null() {
        return false;
    }
    type RespondsToFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool;
    static SYM: OnceLock<Option<RespondsToFn>> = OnceLock::new();
    let f =
        *SYM.get_or_init(|| find_sym(b"class_respondsToSelector\0").map(|p| unsafe { as_fn(p) }));
    match f {
        Some(f) => unsafe { f(cls, sel) },
        None => false,
    }
}

// ── SLSEventRecord extraction ──────────────────────────────────────────────

/// Extract the embedded `SLSEventRecord *` from a `CGEvent`.
///
/// Layout of `__CGEvent` (SkyLight ObjC type encodings):
///   `{CFRuntimeBase, uint32_t, SLSEventRecord *}`
/// On 64-bit: CFRuntimeBase=16, uint32=4, 4 bytes pad → record pointer at offset 24.
/// We probe offsets 24, 32, 16 for resilience across OS versions (same as Swift).
unsafe fn extract_event_record(event_ptr: *mut c_void) -> *mut c_void {
    for &offset in &[24usize, 32, 16] {
        let slot = (event_ptr as *const u8).add(offset).cast::<*mut c_void>();
        let p = std::ptr::read_unaligned(slot);
        if !p.is_null() {
            return p;
        }
    }
    std::ptr::null_mut()
}

// ── Public entry points ────────────────────────────────────────────────────

/// Post `event_ptr` (raw `CGEventRef`) to `pid` via `SLEventPostToPid`.
///
/// `attach_auth_message`: pass `true` for keyboard events (Chromium path),
/// `false` for mouse events (see Swift doc comment on `postToPid`).
///
/// Returns `true` when `SLEventPostToPid` resolved and the post was attempted.
/// Returns `false` when the SPI is absent — caller falls back to `CGEvent::post_to_pid`.
pub(super) fn post_to_pid(pid: pid_t, event_ptr: *mut c_void, attach_auth_message: bool) -> bool {
    let post_fn = match post_to_pid_fn() {
        Some(f) => f,
        None => return false,
    };

    if attach_auth_message {
        // Build and attach SLSEventAuthenticationMessage.
        //
        // macOS 14 (Sonoma) compatibility: the class exists on macOS 14 but
        // `messageWithEventRecord:pid:version:` was added in macOS 15. Guard
        // with `class_respondsToSelector` (a `!sel.is_null()` check is not
        // enough — `sel_registerName` interns any name); when the selector is
        // absent we skip the auth envelope and fall through to the plain
        // `SLEventPostToPid` below. Chromium-class targets may not receive the
        // event on macOS 14, but the daemon no longer crashes. See #1503.
        let cls = objc_class(c"SLSEventAuthenticationMessage");
        let sel = sel_register(c"messageWithEventRecord:pid:version:");
        let factory = factory_msg_send_fn();

        if class_responds_to_selector(cls, sel) {
            if let Some(factory_fn) = factory {
                let record = unsafe { extract_event_record(event_ptr) };
                if !record.is_null() {
                    let msg = unsafe { factory_fn(cls, sel, record, pid as c_int, 0u32) };
                    if !msg.is_null() {
                        if let Some(set_auth) = set_auth_msg_fn() {
                            unsafe { set_auth(event_ptr, msg) };
                        }
                    }
                }
            }
        }
    }

    unsafe { post_fn(pid, event_ptr) };
    true
}

const MENU_VALIDATION_KEY_CODE: u16 = 90;

/// Construct the unbound Cmd+F20 pair used only to make AppKit validate its
/// menus. Both wrappers are owned by the caller and remain alive through the
/// synchronous posts.
fn menu_validation_events() -> Option<(CGEvent, CGEvent)> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok()?;
    let down = CGEvent::new_keyboard_event(source.clone(), MENU_VALIDATION_KEY_CODE, true).ok()?;
    let up = CGEvent::new_keyboard_event(source, MENU_VALIDATION_KEY_CODE, false).ok()?;
    for event in [&down, &up] {
        // HIDSystemState may inherit a person's physical modifiers. The
        // validation chord is exactly Command, never that leaked state.
        event.set_flags(CGEventFlags::CGEventFlagCommand);
        super::tag_synthetic_event(event);
    }
    Some((down, up))
}

/// Execute the menu-validation sequence with each side effect isolated for
/// deterministic tests. The public entry point below supplies the real
/// SkyLight steps; tests can prove every cutoff and post order without a
/// desktop or wall-clock sleeps.
fn prime_background_menu_validation_with(
    activate: impl FnOnce() -> bool,
    note_primed: impl FnOnce(),
    resolve_psn: impl FnOnce() -> bool,
    mut post_record: impl FnMut(u8) -> bool,
    make_events: impl FnOnce() -> Option<(CGEvent, CGEvent)>,
    mut post_event: impl FnMut(&CGEvent, bool) -> bool,
) -> bool {
    if !activate() {
        return false;
    }
    // Activation may already have changed key focus. Register it before any
    // later fallible step so the focus guard can restore it after a failure.
    note_primed();
    if !resolve_psn() {
        return false;
    }
    // Attempt the pair even when the first record fails, but never continue
    // to key events with only a partial key-window sequence.
    let key_down_record = post_record(0x01);
    let key_up_record = post_record(0x02);
    if !(key_down_record && key_up_record) {
        return false;
    }
    // Preconstruct both events so an allocation failure cannot emit a
    // down-only chord.
    let Some((down, up)) = make_events() else {
        return false;
    };
    // Do not short-circuit keyUp after a failed keyDown post. Authentication
    // is always false: this deliberately takes the IOHID/NSMenu route.
    let down_posted = post_event(&down, false);
    let up_posted = post_event(&up, false);
    down_posted && up_posted
}

/// Wake AppKit's background menu validation for `wid` without activating or
/// raising its app. After focus and exact key-window records, one harmless
/// Cmd+F20 keyDown/keyUp pair is posted through the unauthenticated
/// SLEventPostToPid route. The current item-404 specification defines F20 as
/// unbound; no replacement chord or public-transport fallback is used.
///
/// Returns `true` only when activation, both key records, event construction,
/// and both SkyLight posts succeeded or were attempted. This is not proof
/// that AppKit consumed the pair; callers must read the menu item's
/// AXEnabled value to decide whether it may be pressed.
pub fn prime_background_menu_validation(pid: pid_t, wid: u32) -> bool {
    let target_psn = std::cell::Cell::new([0u8; 8]);
    prime_background_menu_validation_with(
        || activate_without_raise_with(pid, wid, false),
        || note_primed_key(pid, wid),
        || {
            if post_event_record_to_fn().is_none() {
                return false;
            }
            let mut psn = [0u8; 8];
            let resolved = get_process_psn_for_window(wid, pid, &mut psn);
            if resolved {
                target_psn.set(psn);
            }
            resolved
        },
        |event_kind| {
            let record = make_key_window_record(wid, event_kind);
            let post = post_event_record_to_fn().expect("record post resolved above");
            note_post_status(unsafe {
                post(target_psn.get().as_ptr() as *const c_void, record.as_ptr())
            })
        },
        menu_validation_events,
        |event, attach_auth| post_to_pid(pid, event.as_ptr() as *mut c_void, attach_auth),
    )
}

/// What a guarded menu validation posted. A failed post call cannot prove
/// the input did not reach the app, so it is kept apart from success.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ValidationPost {
    /// Key-window record posts that succeeded (of 2).
    pub records: u8,
    /// Cmd+F20 key-down/key-up posts that succeeded (of 2).
    pub keys: u8,
    /// Cmd+F20 post calls made, successful or not (a failed call may still
    /// have delivered the event).
    pub key_attempts: u8,
    /// A post call returned failure.
    pub uncertain: bool,
    /// At least one post call was made.
    pub called: bool,
}

/// Whether validation input may have reached the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValidationDelivery {
    /// No post call was made.
    NoneSent,
    /// Every post call made succeeded.
    Sent,
    /// A post call failed; it may still have been delivered.
    Possible,
}

impl ValidationPost {
    pub fn delivery(self) -> ValidationDelivery {
        if !self.called {
            ValidationDelivery::NoneSent
        } else if self.uncertain {
            ValidationDelivery::Possible
        } else {
            ValidationDelivery::Sent
        }
    }

    /// The receipt text for what was posted; empty when nothing was.
    pub fn disclosure(self) -> String {
        if !self.called {
            return String::new();
        }
        let mut text = format!(
            "Menu validation posted {} of 2 key-window records for the target window",
            self.records
        );
        if self.keys > 0 {
            text.push_str(&format!(
                " and {} of 2 Cmd+F20 validation key events to it",
                self.keys
            ));
        }
        text.push('.');
        if self.uncertain {
            text.push_str(
                " A validation post failed; that input may still have reached the target window.",
            );
        }
        text
    }
}

/// The validation sequence over its side effects (tests drive it with
/// fakes). `confirm_key` must show the target as the app's key window before
/// the Cmd+F20 pair. Once the key-down was posted the key-up always follows,
/// so no key is left down.
fn menu_validation_with<E>(
    resolve_psn: impl FnOnce() -> bool,
    mut post_record: impl FnMut(u8) -> bool,
    confirm_key: impl FnOnce() -> bool,
    make_events: impl FnOnce() -> Option<(E, E)>,
    mut post_event: impl FnMut(&E) -> bool,
) -> ValidationPost {
    let mut out = ValidationPost::default();
    if !resolve_psn() {
        return out;
    }
    for event_kind in [0x01, 0x02] {
        out.called = true;
        if post_record(event_kind) {
            out.records += 1;
        } else {
            out.uncertain = true;
        }
    }
    if out.records < 2 || !confirm_key() {
        return out;
    }
    let Some((down, up)) = make_events() else {
        return out;
    };
    for event in [&down, &up] {
        out.key_attempts += 1;
        if post_event(event) {
            out.keys += 1;
        } else {
            out.uncertain = true;
        }
    }
    out
}

/// Wake AppKit's menu validation for `wid` while a [`WindowKeyScope`]
/// already holds it as the app's key window: the key-window records and one
/// Cmd+F20 pair. No activation is used. The caller reads AXEnabled afterwards.
pub fn prime_menu_validation(pid: pid_t, wid: u32) -> ValidationPost {
    let target_psn = std::cell::Cell::new([0u8; 8]);
    menu_validation_with(
        || {
            if post_event_record_to_fn().is_none() {
                return false;
            }
            let mut psn = [0u8; 8];
            let resolved = get_process_psn_for_window(wid, pid, &mut psn);
            if resolved {
                target_psn.set(psn);
            }
            resolved
        },
        |event_kind| {
            let Some(post) = post_event_record_to_fn() else {
                return false;
            };
            note_focus_change();
            let record = make_key_window_record(wid, event_kind);
            note_post_status(unsafe {
                post(target_psn.get().as_ptr() as *const c_void, record.as_ptr())
            })
        },
        || {
            await_key_focus(pid, wid, std::time::Duration::from_millis(200));
            key_focus_of_pid(pid).routes_to(wid)
        },
        menu_validation_events,
        |event| post_to_pid(pid, event.as_ptr() as *mut c_void, false),
    )
}

/// Stamp the event's screen-space `(x, y)` point via
/// `CGEventSetWindowLocation`. Both the SkyLight and public PID routes receive
/// this screen-space value; WindowServer derives the receiving window's local
/// point during routing. Returns `true` when the SPI resolved.
pub(super) fn set_window_location(event_ptr: *mut c_void, x: f64, y: f64) -> bool {
    match set_window_loc_fn() {
        Some(f) => {
            unsafe { f(event_ptr, x, y) };
            true
        }
        None => false,
    }
}

/// Stamp `value` onto `event_ptr` at raw SkyLight field index `field` via
/// `SLEventSetIntegerValueField`. Returns `false` when SPI absent.
pub(super) fn set_integer_field(event_ptr: *mut c_void, field: u32, value: i64) -> bool {
    match set_int_field_fn() {
        Some(f) => {
            unsafe { f(event_ptr, field, value) };
            true
        }
        None => false,
    }
}

/// Return the Skylight main connection ID for the current process.
pub fn main_connection_id() -> Option<u32> {
    connection_id_fn().map(|f| unsafe { f() })
}

fn nonzero_space_id(space_id: Option<u64>) -> Option<u64> {
    space_id.filter(|id| *id != 0)
}

/// A consistent WindowServer connection and active-Space snapshot for one
/// enumeration. Space membership must be queried one window at a time:
/// `SLSCopySpacesForWindows` returns the set union for its input window list,
/// not a positionally aligned result.
pub(crate) struct SpaceQuery {
    connection_id: u32,
    current_space_id: Option<u64>,
    copy_spaces_for_windows: Option<CopySpacesForWindowsFn>,
    copy_managed_display_for_window: Option<CopyManagedDisplayForWindowFn>,
    managed_display_get_current_space: Option<ManagedDisplayGetCurrentSpaceFn>,
}

impl SpaceQuery {
    pub(crate) fn new() -> Option<Self> {
        let connection_id = main_connection_id()?;
        let current_space_id =
            nonzero_space_id(get_active_space_fn().map(|f| unsafe { f(connection_id) }));
        Some(Self {
            connection_id,
            current_space_id,
            copy_spaces_for_windows: copy_spaces_for_windows_fn(),
            copy_managed_display_for_window: copy_managed_display_for_window_fn(),
            managed_display_get_current_space: managed_display_get_current_space_fn(),
        })
    }

    pub(crate) fn current_space_id(&self) -> Option<u64> {
        self.current_space_id
    }

    /// Return every Space containing `window_id`.
    pub(crate) fn window_space_ids(&self, window_id: u32) -> Option<Vec<u64>> {
        use core_foundation::{
            array::CFArray,
            base::{CFGetTypeID, CFTypeRef, TCFType},
            number::CFNumber,
        };

        let copy_spaces = self.copy_spaces_for_windows?;
        let window_number = CFNumber::from(window_id as i64);
        let window_ref = window_number.as_concrete_TypeRef() as *const c_void;
        let windows = CFArray::<CFTypeRef>::from_copyable(&[window_ref]);
        let result_ptr = unsafe {
            copy_spaces(
                self.connection_id,
                0x7,
                windows.as_concrete_TypeRef() as *const c_void,
            )
        };
        if result_ptr.is_null() {
            return None;
        }

        let result: CFArray<CFTypeRef> =
            unsafe { CFArray::wrap_under_create_rule(result_ptr as _) };
        let mut space_ids = Vec::with_capacity(result.len() as usize);
        for item in result.iter() {
            let item = *item;
            if unsafe { CFGetTypeID(item) } != CFNumber::type_id() {
                return None;
            }
            let number = unsafe { CFNumber::wrap_under_get_rule(item as _) };
            let space_id = u64::try_from(number.to_i64()?).ok()?;
            if space_id != 0 {
                space_ids.push(space_id);
            }
        }

        (!space_ids.is_empty()).then_some(space_ids)
    }

    /// Return the active Space on the display WindowServer associates with
    /// `window_id`. This avoids comparing every window against the main
    /// display's active Space when displays use independent Spaces.
    pub(crate) fn current_space_for_window(&self, window_id: u32) -> Option<u64> {
        use core_foundation::{base::TCFType, string::CFString};

        let copy_display = self.copy_managed_display_for_window?;
        let get_current_space = self.managed_display_get_current_space?;
        let display_ptr = unsafe { copy_display(self.connection_id, window_id) };
        if display_ptr.is_null() {
            return None;
        }
        let display = unsafe { CFString::wrap_under_create_rule(display_ptr as _) };
        nonzero_space_id(Some(unsafe {
            get_current_space(
                self.connection_id,
                display.as_concrete_TypeRef() as *const c_void,
            )
        }))
    }
}

// ── Off-Space window capture ──────────────────────────────────────────────────

/// `CFArrayRef SLSHWCaptureWindowList(CGSConnectionID cid, CGWindowID *windows,
/// int count, uint32_t options)`. Returns a +1 array of `CGImageRef`, one per
/// captured window. Unlike ScreenCaptureKit and `screencapture -l`, it returns
/// the content of a window that sits on another Space (CUA-1351); window
/// switchers such as AltTab use it for the same reason. Read-only: it does not
/// switch Spaces, order windows, or activate anything.
type HwCaptureWindowListFn = unsafe extern "C" fn(c_int, *mut u32, c_int, u32) -> *const c_void;

/// Capture through the window's clip shape (rounded corners, Space clipping).
const HW_CAPTURE_IGNORE_GLOBAL_CLIP_SHAPE: u32 = 1 << 11;
/// Backing-store resolution (Retina pixels), matching the ScreenCaptureKit path.
const HW_CAPTURE_BEST_RESOLUTION: u32 = 1 << 8;

fn hw_capture_window_list_fn() -> Option<HwCaptureWindowListFn> {
    static SYM: OnceLock<Option<HwCaptureWindowListFn>> = OnceLock::new();
    *SYM.get_or_init(|| {
        find_sym(b"SLSHWCaptureWindowList\0")
            .or_else(|| find_sym(b"CGSHWCaptureWindowList\0"))
            .map(|p| unsafe { as_fn(p) })
    })
}

/// Capture one window's backing store with `SLSHWCaptureWindowList`.
///
/// `None` when the SPI is missing or WindowServer returns no image.
pub fn hw_capture_window(window_id: u32) -> Option<core_graphics::image::CGImage> {
    use core_foundation::{
        array::CFArray,
        base::{CFGetTypeID, CFRetain, CFTypeRef, TCFType},
    };
    use foreign_types::ForeignType;

    extern "C" {
        fn CGImageGetTypeID() -> core_foundation::base::CFTypeID;
    }

    let capture = hw_capture_window_list_fn()?;
    let cid = c_int::try_from(main_connection_id()?).ok()?;
    let mut window = window_id;
    let array_ptr = unsafe {
        capture(
            cid,
            &mut window,
            1,
            HW_CAPTURE_IGNORE_GLOBAL_CLIP_SHAPE | HW_CAPTURE_BEST_RESOLUTION,
        )
    };
    if array_ptr.is_null() {
        return None;
    }
    let images: CFArray<CFTypeRef> = unsafe { CFArray::wrap_under_create_rule(array_ptr as _) };
    let first = *images.get(0)?;
    if first.is_null() || unsafe { CFGetTypeID(first) } != unsafe { CGImageGetTypeID() } {
        return None;
    }
    // The array owns its element; take our own reference before it drops.
    unsafe {
        CFRetain(first);
        Some(core_graphics::image::CGImage::from_ptr(first as *mut _))
    }
}

// ── Key-focus theft ───────────────────────────────────────────────────────────

/// `CGError SLPSStealKeyFocusReturningID(uint32_t cid, uint32_t options, int32_t *theft_id)`
type StealKeyFocusReturningIdFn = unsafe extern "C" fn(u32, u32, *mut i32) -> i32;

/// `CGError SLPSReleaseKeyFocusWithID(int32_t theft_id)`
type ReleaseKeyFocusWithIdFn = unsafe extern "C" fn(i32) -> i32;

fn steal_key_focus_fn() -> Option<StealKeyFocusReturningIdFn> {
    static SYM: OnceLock<Option<StealKeyFocusReturningIdFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"SLPSStealKeyFocusReturningID\0").map(|p| unsafe { as_fn(p) }))
}

fn release_key_focus_fn() -> Option<ReleaseKeyFocusWithIdFn> {
    static SYM: OnceLock<Option<ReleaseKeyFocusWithIdFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"SLPSReleaseKeyFocusWithID\0").map(|p| unsafe { as_fn(p) }))
}

/// `OSStatus SLPSGetKeyFocusProcess(ProcessSerialNumber *psn, uint32_t *connection)`.
/// Both are out-parameters; the second must be a valid pointer.
type GetKeyFocusProcessFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;

/// `OSStatus GetProcessPID(const ProcessSerialNumber *psn, pid_t *pid)`
type GetProcessPidFn = unsafe extern "C" fn(*const c_void, *mut pid_t) -> i32;

fn get_key_focus_process_fn() -> Option<GetKeyFocusProcessFn> {
    static SYM: OnceLock<Option<GetKeyFocusProcessFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"SLPSGetKeyFocusProcess\0").map(|p| unsafe { as_fn(p) }))
}

fn get_process_pid_fn() -> Option<GetProcessPidFn> {
    static SYM: OnceLock<Option<GetProcessPidFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"GetProcessPID\0").map(|p| unsafe { as_fn(p) }))
}

/// The pid of the process WindowServer gives key focus to: normally the
/// frontmost app, or a non-activating panel (Spotlight-like launchers) or a
/// menu that took it.
pub fn key_focus_pid() -> Option<pid_t> {
    let get = get_key_focus_process_fn()?;
    let to_pid = get_process_pid_fn()?;
    let mut psn = [0u8; 8];
    // Room for whatever the second out-parameter holds (a connection id).
    let mut second = [0u64; 2];
    let err = unsafe {
        get(
            psn.as_mut_ptr() as *mut c_void,
            second.as_mut_ptr() as *mut c_void,
        )
    };
    if err != 0 || psn == [0u8; 8] {
        return None;
    }
    let mut pid: pid_t = 0;
    if unsafe { to_pid(psn.as_ptr() as *const c_void, &mut pid) } != 0 || pid <= 0 {
        return None;
    }
    Some(pid)
}

/// The pid of the process WindowServer has frontmost now. Unlike
/// `NSWorkspace.frontmostApplication`, this does not wait for the caller's
/// AppKit run loop to process an activation notice, so it sees a background
/// app's self-activation at once.
pub fn front_process_pid() -> Option<pid_t> {
    let get_front = get_front_process_fn()?;
    let to_pid = get_process_pid_fn()?;
    let mut psn = [0u8; 8];
    if unsafe { get_front(psn.as_mut_ptr() as *mut c_void) } != 0 || psn == [0u8; 8] {
        return None;
    }
    let mut pid: pid_t = 0;
    if unsafe { to_pid(psn.as_ptr() as *const c_void, &mut pid) } != 0 || pid <= 0 {
        return None;
    }
    Some(pid)
}

/// Whether the key-focus theft SPIs resolve on this system.
pub fn key_focus_theft_available() -> bool {
    steal_key_focus_fn().is_some()
        && release_key_focus_fn().is_some()
        && main_connection_id().is_some()
}

/// Steal key focus for this process's own connection and return the theft id.
///
/// WindowServer numbers thefts from one session-wide counter, so the id tells
/// the caller which id the next theft will get. AppKit uses the same call,
/// with the same options, when it opens a menu in an app that is not active.
pub fn steal_key_focus() -> Option<i32> {
    let steal = steal_key_focus_fn()?;
    let cid = main_connection_id()?;
    let mut id = 0i32;
    let err = unsafe { steal(cid, 0xC80, &mut id) };
    (err == 0 && id != 0).then_some(id)
}

/// Release a key-focus theft by id, whichever connection made it.
/// Returns the CGError (0 on success; 1010 when no such theft is live).
pub fn release_key_focus(theft_id: i32) -> i32 {
    match release_key_focus_fn() {
        Some(release) => unsafe { release(theft_id) },
        None => -1,
    }
}

// ── Focus-without-raise ───────────────────────────────────────────────────────

/// Activate `target_pid`'s window `target_wid` without raising any windows
/// or triggering Space-follow. Derived from yabai (MIT); see
/// THIRD_PARTY_NOTICES.md.
///
/// Recipe:
/// 1. `_SLPSGetFrontProcess` → capture current front PSN.
/// 2. `SLSGetWindowOwner + SLSGetConnectionPSN` → target PSN, with
///    `GetProcessForPID(target_pid)` as an older-system fallback.
/// 3. Post 248-byte defocus record to front PSN (`bytes[0x8a] = 0x02`).
/// 4. Post 248-byte focus record to target PSN (`bytes[0x8a] = 0x01`,
///    `bytes[0x3c..0x3f]` = `target_wid` little-endian).
///
/// Deliberately skips `SLPSSetFrontProcessWithOptions` — see the Swift
/// reference `FocusWithoutRaise.swift` for why omitting it keeps
/// Chromium's user-activation gate open.
///
/// Returns `true` when all SPIs resolved and both posts succeeded.
pub fn activate_without_raise(target_pid: pid_t, target_wid: u32) -> bool {
    activate_without_raise_with(target_pid, target_wid, true)
}

/// [`activate_without_raise`] with the defocus record to the current front
/// process made optional (`defocus_front`).
pub fn activate_without_raise_with(
    target_pid: pid_t,
    target_wid: u32,
    defocus_front: bool,
) -> bool {
    crate::window_change_detector::end_tail();
    // A key hand-back still running would move focus under this record.
    if !await_key_restores() {
        tracing::warn!("activate_without_raise: a key hand-back is still running; not sent");
        return false;
    }
    note_focus_change();
    let post_fn = match post_event_record_to_fn() {
        Some(f) => f,
        None => return false,
    };
    let get_front = match get_front_process_fn() {
        Some(f) => f,
        None => return false,
    };
    // 8-byte PSN buffers (two UInt32s).
    let mut prev_psn = [0u8; 8];
    let mut target_psn = [0u8; 8];

    let ok_prev = unsafe { get_front(prev_psn.as_mut_ptr() as *mut c_void) } == 0;
    if !ok_prev {
        return false;
    }

    if !get_process_psn_for_window(target_wid, target_pid, &mut target_psn) {
        return false;
    }

    // Build the 248-byte event buffer.
    let mut buf = [0u8; 0xF8];
    buf[0x04] = 0xF8;
    buf[0x08] = 0x0D;
    // Stamp target window id in little-endian at bytes 0x3c–0x3f.
    buf[0x3C] = (target_wid & 0xFF) as u8;
    buf[0x3D] = ((target_wid >> 8) & 0xFF) as u8;
    buf[0x3E] = ((target_wid >> 16) & 0xFF) as u8;
    buf[0x3F] = ((target_wid >> 24) & 0xFF) as u8;

    // Step 3: defocus previous front.
    buf[0x8A] = 0x02;
    let defocus_ok = !defocus_front
        || note_post_status(unsafe { post_fn(prev_psn.as_ptr() as *const c_void, buf.as_ptr()) });

    // Step 4: focus target.
    buf[0x8A] = 0x01;
    let focus_ok =
        note_post_status(unsafe { post_fn(target_psn.as_ptr() as *const c_void, buf.as_ptr()) });

    defocus_ok && focus_ok
}

/// Reverse [`activate_without_raise`] once a background click is delivered:
/// defocus `target_wid` and hand key focus back to `previous_pid`'s key window.
///
/// The no-raise recipe posts a defocus record to the user's front process.
/// That process stays frontmost as far as NSWorkspace reports, but its key
/// window stops receiving keyboard input until the user clicks it again, so
/// the activation-based restore never fires. Returns `true` when both posts
/// succeeded.
pub fn restore_focus_after_without_raise(
    previous_pid: pid_t,
    previous_key_wid: Option<u32>,
    target_pid: pid_t,
    target_wid: u32,
    may_post: impl Fn() -> bool,
) -> bool {
    let Some(post_fn) = post_event_record_to_fn() else {
        return false;
    };
    let Some(previous_wid) = previous_key_wid.or_else(|| key_window_of_pid(previous_pid)) else {
        return false;
    };
    let mut previous_psn = [0u8; 8];
    let mut target_psn = [0u8; 8];
    if !get_process_psn_for_window(previous_wid, previous_pid, &mut previous_psn)
        || !get_process_psn_for_window(target_wid, target_pid, &mut target_psn)
    {
        return false;
    }

    guarded_focus_posts(
        may_post,
        || {
            let mut buf = focus_record(target_wid);
            buf[0x8A] = 0x02;
            note_post_status(unsafe { post_fn(target_psn.as_ptr() as *const c_void, buf.as_ptr()) })
        },
        || {
            let mut buf = focus_record(previous_wid);
            buf[0x8A] = 0x01;
            note_post_status(unsafe {
                post_fn(previous_psn.as_ptr() as *const c_void, buf.as_ptr())
            })
        },
    )
}

fn guarded_focus_posts(
    may_post: impl Fn() -> bool,
    defocus: impl FnOnce() -> bool,
    focus: impl FnOnce() -> bool,
) -> bool {
    if !may_post() {
        return false;
    }
    let defocus_ok = defocus();
    if !may_post() {
        return false;
    }
    let focus_ok = focus();
    defocus_ok && focus_ok
}

#[cfg(test)]
#[test]
fn pixel_focus_posts_stop_after_takeover_between_posts() {
    use std::cell::Cell;
    let fresh = Cell::new(true);
    let posts = Cell::new(0);
    let result = guarded_focus_posts(
        || fresh.get(),
        || {
            posts.set(posts.get() + 1);
            fresh.set(false);
            true
        },
        || {
            posts.set(posts.get() + 1);
            true
        },
    );
    assert!(!result);
    assert_eq!(posts.get(), 1);
}

/// The 248-byte focus/defocus event record with `wid` stamped little-endian at
/// bytes 0x3c–0x3f. The caller sets the direction byte at 0x8a.
fn focus_record(wid: u32) -> [u8; 0xF8] {
    let mut buf = [0u8; 0xF8];
    buf[0x04] = 0xF8;
    buf[0x08] = 0x0D;
    buf[0x3C..0x40].copy_from_slice(&wid.to_le_bytes());
    buf
}

/// The CGWindowID of `pid`'s key window: its `AXFocusedWindow`, else its
/// frontmost on-screen layer-0 window.
pub fn key_window_of_pid(pid: pid_t) -> Option<u32> {
    use crate::ax::bindings::{ax_get_window_id, copy_element_attr, AXUIElementCreateApplication};
    let focused = unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            None
        } else {
            let window = copy_element_attr(app, "AXFocusedWindow");
            core_foundation::base::CFRelease(app as _);
            window.and_then(|window| {
                let wid = ax_get_window_id(window);
                core_foundation::base::CFRelease(window as _);
                wid
            })
        }
    };
    focused.or_else(|| {
        crate::windows::visible_windows()
            .into_iter()
            .filter(|w| w.pid == pid && w.layer == 0 && w.is_on_screen)
            .max_by_key(|w| w.z_index)
            .map(|w| w.window_id)
    })
}

// ── NSMenu shortcut activation ────────────────────────────────────────────────

/// Gets the PSN for the process that owns `window_id`.
/// Uses `CGSMainConnectionID` + `SLSGetWindowOwner` + `SLSGetConnectionPSN`.
/// Falls back to `GetProcessForPID(pid)` when the SkyLight path fails.
pub fn get_process_psn_for_window(window_id: u32, pid: libc::pid_t, out_psn: &mut [u8; 8]) -> bool {
    // Try modern path: CGSMainConnectionID → SLSGetWindowOwner → SLSGetConnectionPSN
    if let (Some(get_owner), Some(get_psn), Some(conn_id_fn)) = (
        get_window_owner_fn(),
        get_connection_psn_fn(),
        connection_id_fn(),
    ) {
        let main_cid = unsafe { conn_id_fn() };
        let mut owner_cid: u32 = 0;
        let ok = unsafe { get_owner(main_cid, window_id, &mut owner_cid) } == 0;
        if ok && owner_cid != 0 {
            let psn_ok = unsafe { get_psn(owner_cid, out_psn.as_mut_ptr() as *mut c_void) } == 0;
            if psn_ok {
                return true;
            }
        }
    }
    // Fallback: GetProcessForPID
    if let Some(get_pid_psn) = get_process_for_pid_fn() {
        return unsafe { get_pid_psn(pid, out_psn.as_mut_ptr() as *mut c_void) } == 0;
    }
    false
}

/// Return whether WindowServer currently considers the exact window's process
/// frontmost. Unlike `NSWorkspace.frontmostApplication`, this query does not
/// depend on the caller's AppKit run loop processing an activation update.
pub fn front_process_matches(target_pid: libc::pid_t, target_wid: u32) -> Option<bool> {
    let get_front = get_front_process_fn()?;
    let mut front_psn = [0u8; 8];
    if unsafe { get_front(front_psn.as_mut_ptr() as *mut c_void) } != 0 {
        return None;
    }
    let mut target_psn = [0u8; 8];
    if !get_process_psn_for_window(target_wid, target_pid, &mut target_psn) {
        return None;
    }
    Some(front_psn == target_psn)
}

/// Make `target_pid` and `target_wid` WindowServer-frontmost and leave them
/// there. Unlike [`with_foreground_assist`], this deliberately does not save or
/// restore the previous process. It is the persistent counterpart required by
/// focus-proxy surfaces whose input channel is armed only while genuinely
/// frontmost.
///
/// Returns `true` only when the target PSN resolved and WindowServer accepted
/// `SLPSSetFrontProcessWithOptions`.
pub fn set_front_process_persistently(target_pid: libc::pid_t, target_wid: u32) -> bool {
    crate::window_change_detector::note_intentional_activation();
    let Some(set_front) = set_front_process_fn() else {
        return false;
    };
    let mut target_psn = [0u8; 8];
    if !get_process_psn_for_window(target_wid, target_pid, &mut target_psn) {
        return false;
    }

    // kCPSNoWindows = 0x400. Supplying the exact target window still makes
    // that window's process frontmost while avoiding a broad all-window raise.
    unsafe { set_front(target_psn.as_ptr() as *const c_void, target_wid, 0x400) == 0 }
}

fn make_key_window_record(window_id: u32, event_kind: u8) -> [u8; 0xF8] {
    let mut record = [0u8; 0xF8];
    record[0x04] = 0xF8;
    record[0x08] = event_kind;
    record[0x3A] = 0x10;
    record[0x3C..0x40].copy_from_slice(&window_id.to_le_bytes());
    record[0x20..0x30].fill(0xFF);
    record
}

/// Make one exact application window native-key and frontmost.
///
/// Accessibility's `AXFocusedWindow` can change without AppKit making the
/// corresponding `NSWindow` key. Native menu validation observes the latter,
/// so focus-sensitive commands remain disabled in that split state. This is
/// the bounded exact-window sequence used by established macOS window tools:
/// mark the front-process request as user generated, synthesize the paired
/// make-key records for the requested WindowServer id, then let the caller
/// raise the matching AX window. No other application window is addressed.
pub fn make_exact_window_key(target_pid: libc::pid_t, target_wid: u32) -> bool {
    note_focus_change();
    let Some(set_front) = set_front_process_fn() else {
        return false;
    };
    let Some(post) = post_event_record_to_fn() else {
        return false;
    };
    let mut target_psn = [0u8; 8];
    if !get_process_psn_for_window(target_wid, target_pid, &mut target_psn) {
        return false;
    }

    // kCPSUserGenerated = 0x200. Unlike kCPSNoWindows, this permits AppKit to
    // establish the requested native key window before it validates NSMenu.
    if unsafe { set_front(target_psn.as_ptr() as *const c_void, target_wid, 0x200) } != 0 {
        return false;
    }
    for event_kind in [0x01, 0x02] {
        let record = make_key_window_record(target_wid, event_kind);
        if !note_post_status(unsafe { post(target_psn.as_ptr() as *const c_void, record.as_ptr()) })
        {
            return false;
        }
    }
    true
}

/// Serializes every foreground activation in the daemon.
///
/// Each helper below saves the front process, fronts its target, acts, and
/// restores. Two of them interleaving (an agent issuing `click` and
/// `type_text` in parallel) restore each other's "previous" front process
/// mid-flight, so the second never sees its window focused and fails with
/// "did not become focused" (bench CDB-S02, CDB-G03; CUA-1218). One holder at a
/// time removes that race. Nested calls on the holding thread pass through.
static FOREGROUND_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

thread_local! {
    static HOLDS_FOREGROUND_LOCK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct ForegroundTurn {
    _guard: Option<std::sync::MutexGuard<'static, ()>>,
}

impl Drop for ForegroundTurn {
    fn drop(&mut self) {
        if self._guard.is_some() {
            HOLDS_FOREGROUND_LOCK.with(|held| held.set(false));
        }
    }
}

/// Whether some thread holds a foreground turn right now (the driver is
/// deliberately fronting a target).
pub fn foreground_turn_active() -> bool {
    FOREGROUND_LOCK.try_lock().is_err()
}

fn foreground_turn() -> anyhow::Result<ForegroundTurn> {
    // An intentional activation ends the previous action's detached window
    // watch, whose wildcard lease would otherwise revert it.
    crate::window_change_detector::end_tail();
    // A background key hand-back still running would move key focus inside
    // the app this turn fronts; nothing is sent until it is done.
    if !await_key_restores() {
        anyhow::bail!(
            "a background key hand-back did not finish within 2 s, so the foreground action \
             was not sent"
        );
    }
    note_focus_change();
    crate::window_change_detector::note_intentional_activation();
    if HOLDS_FOREGROUND_LOCK.with(std::cell::Cell::get) {
        return Ok(ForegroundTurn { _guard: None });
    }
    let guard = FOREGROUND_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    HOLDS_FOREGROUND_LOCK.with(|held| held.set(true));
    Ok(ForegroundTurn {
        _guard: Some(guard),
    })
}

/// Tool-agnostic foreground-assist: briefly front `window_id`, wait for the
/// activation to actually land, run `body` (which posts the synthetic input),
/// then restore the prior frontmost process.
///
/// This is the `delivery_mode:"foreground"` rung of the best-effort-background
/// ladder, shared by `type_text` and `click`. Reached only when the agent has
/// seen the background rungs fail (clicks) or the field is unverifiable +
/// focus-sensitive (Catalyst typing).
///
/// ## Why the body waits for the activation
///
/// `set_front` is asynchronous, so running the body straight away means the
/// body's `AXFocused` write races AppKit's own activation. AppKit wins: when
/// activation completes it installs the window's remembered first responder
/// and clobbers the write. The keystrokes then land wherever the app chose —
/// for a Catalyst app such as WhatsApp, the message list rather than the
/// composer, which silently scrolls the transcript instead of typing.
///
/// So this waits for the activation to be observable before running `body`, the
/// same ordering [`with_foreground_hid_activation`] already relies on. The wait
/// is a bounded poll rather than a fixed sleep: an app that activates in 10 ms
/// pays 10 ms, and a slow Catalyst/RDP surface still gets its full budget.
///
/// Returns `Ok(true)` when the brief activation happened, `Ok(false)` when the
/// fronting SPIs are unavailable (the body still ran, just without a front).
pub fn with_foreground_assist(
    target_pid: libc::pid_t,
    target_wid: u32,
    body: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<bool> {
    let _turn = foreground_turn()?;
    let set_front = match set_front_process_fn() {
        Some(f) => f,
        None => {
            // SPIs unavailable — run body anyway without activation.
            body()?;
            return Ok(false);
        }
    };

    let mut prev_psn = [0u8; 8];
    let prev_ok = get_front_process_fn()
        .map(|f| unsafe { f(prev_psn.as_mut_ptr() as *mut c_void) } == 0)
        .unwrap_or(false);

    let mut target_psn = [0u8; 8];
    if !get_process_psn_for_window(target_wid, target_pid, &mut target_psn) {
        body()?;
        return Ok(false);
    }

    unsafe { set_front(target_psn.as_ptr() as *const c_void, target_wid, 0x400) };
    // `set_front` moves WindowServer's front process but does not make the
    // target's NSWindow key, and AppKit installs a first responder only for a
    // key window. Without this the app is "frontmost" to WindowServer while
    // remaining, from AppKit's point of view, unfocused — so the AXFocused
    // write in the body has no responder chain to attach to.
    make_exact_window_key(target_pid, target_wid);
    await_window_focused(target_pid, target_wid);

    let result = body();

    if prev_ok {
        unsafe { set_front(prev_psn.as_ptr() as *const c_void, 0, 0x400) };
    }

    result?;
    Ok(true)
}

/// Upper bound on how long [`with_foreground_assist`] waits for a requested
/// activation to become observable. Chosen to cover a Catalyst app's activation
/// plus key-window install; past this the caller proceeds anyway so a stubborn
/// target degrades to the old behavior instead of hanging.
const ACTIVATION_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(400);

/// Poll interval for [`await_window_focused`]. Short enough that a fast native
/// app pays roughly one tick, long enough not to spin on the WindowServer.
const ACTIVATION_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

/// Block until `target_wid` is the application's focused AX window, or the
/// timeout expires. Returns whether that state was observed.
///
/// The predicate is deliberately `AXFocusedWindow` and not
/// `NSWorkspace.frontmostApplication`. The latter does not observe a
/// SkyLight-level front-process change at all: polling it every 15ms across a
/// full foreground `type_text` against WhatsApp showed zero transitions while
/// the target was demonstrably being fronted, so a frontmost-based wait always
/// burns its whole timeout and never actually gates on anything. `AXFocusedWindow`
/// is the same proof [`preserves_exact_existing_focus`] already trusts to decide
/// whether a window is focused.
///
/// A `false` return is not fatal: the caller proceeds with delivery regardless,
/// because a target that never reports focus is exactly the case the pre-existing
/// best-effort contract already covered.
fn await_window_focused(pid: libc::pid_t, window_id: u32) -> bool {
    let deadline = std::time::Instant::now() + ACTIVATION_WAIT_TIMEOUT;
    loop {
        if crate::ax::bindings::focused_window_id_of_pid(pid) == Some(window_id) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(ACTIVATION_POLL_INTERVAL);
    }
}

/// Why `target_wid` could not take focus, as a caller-actionable suffix for
/// the refusal: another window of the app holds focus (an open modal dialog,
/// e.g. LibreOffice "Delete Contents"), or the target is not a visible window
/// (a closed Chrome omnibox popup) and can never be key. Empty when neither
/// is proven.
fn focus_failure_hint(pid: libc::pid_t, target_wid: u32) -> String {
    let target = crate::windows::window_info_by_id(target_wid);
    let focused =
        crate::ax::bindings::focused_window_id_of_pid(pid).filter(|focused| *focused != target_wid);
    focus_failure_hint_from(
        target.as_ref().map(|window| {
            window.is_on_screen && window.bounds.width >= 2.0 && window.bounds.height >= 2.0
        }),
        focused.map(|window_id| {
            let title = crate::windows::window_info_by_id(window_id)
                .map(|window| window.title)
                .unwrap_or_default();
            (window_id, title)
        }),
    )
}

fn focus_failure_hint_from(
    target_visible: Option<bool>,
    focused_other: Option<(u32, String)>,
) -> String {
    match (target_visible, focused_other) {
        (Some(false), Some((window_id, title))) => format!(
            ": the target is not a visible window and cannot take keyboard focus; the \
             app's focused window is window_id {window_id} \"{title}\", act on that instead"
        ),
        (Some(false), None) => {
            ": the target is not a visible window and cannot take keyboard focus".to_owned()
        }
        (_, Some((window_id, title))) => format!(
            ": window_id {window_id} \"{title}\" of the same app holds focus (an open \
             dialog or panel); act on that window_id, or close it first"
        ),
        _ => String::new(),
    }
}

/// Activate an exact target window for a global HID keyboard action.
///
/// This helper must not run `action` when the private foreground SPI is
/// unavailable: a global HID event has no pid addressing and would otherwise
/// land in whichever application is currently frontmost. The short settles
/// keep the target frontmost until WindowServer has routed both sides of the
/// key chord, then restore the prior process even when the action fails.
pub fn with_foreground_hid_activation(
    target_pid: libc::pid_t,
    target_wid: u32,
    action: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let _turn = foreground_turn()?;
    let set_front = set_front_process_fn()
        .ok_or_else(|| anyhow::anyhow!("foreground HID delivery is unavailable"))?;

    let mut prev_psn = [0u8; 8];
    let prev_ok = get_front_process_fn()
        .map(|f| unsafe { f(prev_psn.as_mut_ptr() as *mut c_void) } == 0)
        .unwrap_or(false);

    let mut target_psn = [0u8; 8];
    if !get_process_psn_for_window(target_wid, target_pid, &mut target_psn) {
        anyhow::bail!("could not resolve target window for foreground HID delivery");
    }

    let focused_window_id = crate::ax::bindings::focused_window_id_of_pid(target_pid);
    if preserves_exact_existing_focus(prev_ok, prev_psn, target_psn, focused_window_id, target_wid)
    {
        // Re-activating an already key exact window can clear Chromium's
        // renderer focus even though WindowServer keeps the app frontmost.
        // The AX window proof lets us deliver directly without weakening the
        // exact-window guard or disturbing the current key target.
        return action();
    }

    let activated = unsafe { set_front(target_psn.as_ptr() as *const c_void, target_wid, 0x400) };
    if activated != 0 {
        anyhow::bail!("WindowServer rejected foreground HID activation");
    }

    make_exact_window_key(target_pid, target_wid);
    crate::ax::bindings::raise_exact_window_of_pid(target_pid, target_wid);
    // A slow app (LibreOffice) can take longer than one wait to install its
    // key window: ask once more before giving up.
    if !await_window_focused(target_pid, target_wid) {
        unsafe { set_front(target_psn.as_ptr() as *const c_void, target_wid, 0x400) };
        make_exact_window_key(target_pid, target_wid);
        crate::ax::bindings::raise_exact_window_of_pid(target_pid, target_wid);
    }
    if !await_window_focused(target_pid, target_wid) {
        if prev_ok {
            unsafe { set_front(prev_psn.as_ptr() as *const c_void, 0, 0x400) };
        }
        anyhow::bail!(
            "exact target window did not become focused for foreground HID delivery{}",
            focus_failure_hint(target_pid, target_wid)
        );
    }

    let result = action();
    std::thread::sleep(std::time::Duration::from_millis(40));

    if prev_ok {
        unsafe { set_front(prev_psn.as_ptr() as *const c_void, 0, 0x400) };
    }

    result
}

fn preserves_exact_existing_focus(
    previous_process_known: bool,
    previous_psn: [u8; 8],
    target_psn: [u8; 8],
    focused_window_id: Option<u32>,
    target_window_id: u32,
) -> bool {
    previous_process_known
        && previous_psn == target_psn
        && focused_window_id == Some(target_window_id)
}

#[cfg(test)]
mod tests {
    use super::{
        focus_failure_hint_from, foreground_turn, make_key_window_record, menu_validation_events,
        note_post_status, preserves_exact_existing_focus, prime_background_menu_validation_with,
        reset_windowserver_connection_invalid_for_test, windowserver_connection_invalid,
        MENU_VALIDATION_KEY_CODE, WINDOWSERVER_INVALID_CONNECTION,
    };

    #[test]
    fn menu_validation_pair_is_exact_cmd_f20_down_and_up() {
        use core_graphics::event::{CGEventType, EventField};

        let (down, up) = menu_validation_events().expect("validation events");
        for (event, expected_type) in [(down, CGEventType::KeyDown), (up, CGEventType::KeyUp)] {
            assert_eq!(event.get_type() as u32, expected_type as u32);
            assert_eq!(
                event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE),
                i64::from(MENU_VALIDATION_KEY_CODE)
            );
            assert_eq!(
                event.get_flags(),
                core_graphics::event::CGEventFlags::CGEventFlagCommand
            );
            assert_eq!(
                event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA),
                crate::input::GOAT_CUA_EVENT_USER_DATA
            );
        }
    }

    #[test]
    fn menu_validation_sequence_registers_then_posts_in_order_without_auth() {
        use core_graphics::event::CGEventType;
        use std::{cell::RefCell, rc::Rc};

        let log = Rc::new(RefCell::new(Vec::new()));
        let result = prime_background_menu_validation_with(
            {
                let log = log.clone();
                move || {
                    log.borrow_mut().push("activate".into());
                    true
                }
            },
            {
                let log = log.clone();
                move || log.borrow_mut().push("note".into())
            },
            {
                let log = log.clone();
                move || {
                    log.borrow_mut().push("resolve".into());
                    true
                }
            },
            {
                let log = log.clone();
                move |kind| {
                    log.borrow_mut().push(format!("record:{kind}"));
                    true
                }
            },
            {
                let log = log.clone();
                move || {
                    log.borrow_mut().push("make".into());
                    menu_validation_events()
                }
            },
            {
                let log = log.clone();
                move |event, auth| {
                    let down = event.get_type() as u32 == CGEventType::KeyDown as u32;
                    log.borrow_mut().push(format!("post:{down}:{auth}"));
                    true
                }
            },
        );
        assert!(result);
        assert_eq!(
            log.borrow().as_slice(),
            [
                "activate",
                "note",
                "resolve",
                "record:1",
                "record:2",
                "make",
                "post:true:false",
                "post:false:false",
            ]
        );
    }

    #[test]
    fn menu_validation_activation_failure_sends_nothing() {
        assert!(!prime_background_menu_validation_with(
            || false,
            || panic!("registration must not run"),
            || panic!("PSN resolution must not run"),
            |_| panic!("record must not run"),
            || panic!("event construction must not run"),
            |_, _| panic!("event post must not run"),
        ));
    }

    #[test]
    fn menu_validation_psn_failure_stops_after_registration() {
        use std::{cell::RefCell, rc::Rc};

        let log = Rc::new(RefCell::new(Vec::<String>::new()));
        let result = prime_background_menu_validation_with(
            {
                let log = log.clone();
                move || {
                    log.borrow_mut().push("activate".into());
                    true
                }
            },
            {
                let log = log.clone();
                move || log.borrow_mut().push("note".into())
            },
            {
                let log = log.clone();
                move || {
                    log.borrow_mut().push("resolve".into());
                    false
                }
            },
            |_| panic!("record must not run"),
            || panic!("event construction must not run"),
            |_, _| panic!("event post must not run"),
        );
        assert!(!result);
        assert_eq!(log.borrow().as_slice(), ["activate", "note", "resolve"]);
    }

    #[test]
    fn menu_validation_attempts_both_records_then_suppresses_f20() {
        use std::{cell::RefCell, rc::Rc};

        for failed_kind in [0x01, 0x02] {
            let records = Rc::new(RefCell::new(Vec::new()));
            let result = prime_background_menu_validation_with(
                || true,
                || (),
                || true,
                {
                    let records = records.clone();
                    move |kind| {
                        records.borrow_mut().push(kind);
                        kind != failed_kind
                    }
                },
                || panic!("F20 construction must not follow a record failure"),
                |_, _| panic!("F20 post must not follow a record failure"),
            );
            assert!(!result);
            assert_eq!(records.borrow().as_slice(), [1, 2]);
        }
    }

    #[test]
    fn menu_validation_construction_failure_posts_nothing() {
        assert!(!prime_background_menu_validation_with(
            || true,
            || (),
            || true,
            |_| true,
            || None,
            |_, _| panic!("no event may be posted after construction fails"),
        ));
    }

    #[test]
    fn menu_validation_posts_key_up_after_key_down_post_fails() {
        use core_graphics::event::CGEventType;
        use std::{cell::RefCell, rc::Rc};

        let posts = Rc::new(RefCell::new(Vec::new()));
        let result = prime_background_menu_validation_with(
            || true,
            || (),
            || true,
            |_| true,
            menu_validation_events,
            {
                let posts = posts.clone();
                move |event, auth| {
                    let down = event.get_type() as u32 == CGEventType::KeyDown as u32;
                    posts.borrow_mut().push((down, auth));
                    !down
                }
            },
        );
        assert!(!result);
        assert_eq!(posts.borrow().as_slice(), [(true, false), (false, false)]);
    }

    #[test]
    fn a_1002_post_status_is_remembered_and_other_failures_are_not() {
        reset_windowserver_connection_invalid_for_test();
        assert!(note_post_status(0));
        assert!(!note_post_status(-1));
        assert!(!windowserver_connection_invalid());
        assert!(!note_post_status(WINDOWSERVER_INVALID_CONNECTION));
        assert!(windowserver_connection_invalid());
        reset_windowserver_connection_invalid_for_test();
    }

    #[test]
    fn focus_failure_names_the_dialog_that_holds_focus() {
        let hint = focus_failure_hint_from(Some(true), Some((3991, "Delete Contents".into())));
        assert!(hint.contains("window_id 3991"), "{hint}");
        assert!(hint.contains("Delete Contents"), "{hint}");
    }

    #[test]
    fn focus_failure_on_an_invisible_target_says_it_cannot_be_key() {
        let hint = focus_failure_hint_from(Some(false), Some((2497, "Morrowglass".into())));
        assert!(hint.contains("not a visible window"), "{hint}");
        assert!(hint.contains("window_id 2497"), "{hint}");
        assert_eq!(focus_failure_hint_from(Some(true), None), "");
        assert_eq!(focus_failure_hint_from(None, None), "");
    }

    #[test]
    fn foreground_turns_serialize_across_threads_and_nest_on_one() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let inside = Arc::new(AtomicUsize::new(0));
        let max_inside = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let inside = inside.clone();
                let max_inside = max_inside.clone();
                std::thread::spawn(move || {
                    let _turn = foreground_turn().unwrap();
                    // A nested helper on the holding thread must not deadlock.
                    let _nested = foreground_turn().unwrap();
                    let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                    max_inside.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    inside.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(max_inside.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn make_key_records_address_only_the_exact_window() {
        let press = make_key_window_record(0x7856_3412, 0x01);
        let release = make_key_window_record(0x7856_3412, 0x02);
        assert_eq!(press.len(), 0xF8);
        assert_eq!(press[0x04], 0xF8);
        assert_eq!(press[0x08], 0x01);
        assert_eq!(release[0x08], 0x02);
        assert_eq!(&press[0x3C..0x40], &[0x12, 0x34, 0x56, 0x78]);
        assert_eq!(press[0x3A], 0x10);
        assert!(press[0x20..0x30].iter().all(|byte| *byte == 0xFF));
    }

    #[test]
    fn exact_existing_focus_avoids_reactivation() {
        let psn = [1, 2, 3, 4, 5, 6, 7, 8];
        assert!(preserves_exact_existing_focus(true, psn, psn, Some(42), 42));
    }

    #[test]
    fn process_or_window_uncertainty_requires_guarded_activation() {
        let target = [1, 2, 3, 4, 5, 6, 7, 8];
        let other = [8, 7, 6, 5, 4, 3, 2, 1];
        assert!(!preserves_exact_existing_focus(
            false,
            target,
            target,
            Some(42),
            42
        ));
        assert!(!preserves_exact_existing_focus(
            true,
            other,
            target,
            Some(42),
            42
        ));
        assert!(!preserves_exact_existing_focus(
            true,
            target,
            target,
            Some(41),
            42
        ));
        assert!(!preserves_exact_existing_focus(
            true, target, target, None, 42
        ));
    }
}

/// Tests that create key scopes or read the process-wide scope, restore and
/// primed-key registries run one at a time: a hand-back still running on its
/// own thread counts there until it finishes.
#[cfg(test)]
static KEY_SCOPE_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
fn key_scope_tests_serial() -> std::sync::MutexGuard<'static, ()> {
    KEY_SCOPE_TESTS.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod key_prime_tests {
    use super::{prime_exact_window_key, restore_target, KeyFocus, KeyPrime};
    use std::cell::Cell;

    fn at(window: Option<u32>, element_window: Option<u32>) -> KeyFocus {
        KeyFocus {
            window,
            element_window,
        }
    }

    #[test]
    fn already_key_sends_nothing() {
        let r = prime_exact_window_key(
            7,
            || at(Some(7), Some(7)),
            || panic!("fast"),
            || panic!("record"),
            || panic!("ax"),
        );
        assert_eq!(r, Ok(KeyPrime::AlreadyKey));
    }

    /// LibreOffice that was never active: AX names the window but no element
    /// has focus, and keys are dropped. It must be primed, not trusted.
    #[test]
    fn focused_window_without_focused_element_is_primed() {
        let focus = Cell::new(at(Some(7), None));
        let records = Cell::new(0);
        let r = prime_exact_window_key(
            7,
            || focus.get(),
            || false,
            || {
                records.set(records.get() + 1);
                focus.set(at(Some(7), Some(7)));
                true
            },
            || panic!("ax"),
        );
        assert_eq!(r, Ok(KeyPrime::KeyRecords));
        assert_eq!(records.get(), 1);
    }

    #[test]
    fn key_records_are_accepted_only_after_the_reread_names_the_target() {
        let focus = Cell::new(at(Some(9), Some(9)));
        let r = prime_exact_window_key(
            7,
            || focus.get(),
            || false,
            || {
                focus.set(at(Some(7), Some(7)));
                true
            },
            || panic!("ax"),
        );
        assert_eq!(r, Ok(KeyPrime::KeyRecords));
    }

    /// The records are delivered asynchronously; a second pass is allowed.
    #[test]
    fn a_late_switch_is_caught_by_the_second_pass() {
        let focus = Cell::new(at(Some(9), Some(9)));
        let passes = Cell::new(0);
        let r = prime_exact_window_key(
            7,
            || focus.get(),
            || false,
            || {
                passes.set(passes.get() + 1);
                if passes.get() == 2 {
                    focus.set(at(Some(7), Some(7)));
                }
                true
            },
            || panic!("ax"),
        );
        assert_eq!(r, Ok(KeyPrime::KeyRecords));
        assert_eq!(passes.get(), 2);
    }

    #[test]
    fn ignored_records_fall_back_to_ax_then_confirm() {
        let focus = Cell::new(at(Some(9), Some(9)));
        let r = prime_exact_window_key(
            7,
            || focus.get(),
            || false,
            || true, // records posted but the app ignored them
            || {
                focus.set(at(Some(7), None));
                true
            },
        );
        assert_eq!(r, Ok(KeyPrime::AxFocusedWindow));
    }

    /// After the switch VCL can keep naming a Find & Replace field as the
    /// focused element while the document window is key and gets the keys.
    #[test]
    fn a_stale_dialog_element_does_not_block_a_confirmed_switch() {
        let focus = Cell::new(at(Some(9), Some(9)));
        let r = prime_exact_window_key(
            7,
            || focus.get(),
            || false,
            || {
                focus.set(at(Some(7), Some(9)));
                true
            },
            || panic!("ax"),
        );
        assert_eq!(r, Ok(KeyPrime::KeyRecords));
    }

    /// The short record form is tried first and, when the re-read names the
    /// target, is all that runs.
    #[test]
    fn a_confirmed_fast_make_key_skips_the_full_sequence() {
        let focus = Cell::new(at(Some(9), Some(9)));
        let r = prime_exact_window_key(
            7,
            || focus.get(),
            || {
                focus.set(at(Some(7), Some(7)));
                true
            },
            || panic!("the full sequence must not run"),
            || panic!("ax"),
        );
        assert_eq!(r, Ok(KeyPrime::KeyRecords));
    }

    /// An app that does not take the short form still gets today's full
    /// sequence, and keys go out only after that is confirmed.
    #[test]
    fn an_unconfirmed_fast_make_key_falls_back_to_the_full_sequence() {
        let focus = Cell::new(at(Some(9), Some(9)));
        let fast = Cell::new(0);
        let full = Cell::new(0);
        let r = prime_exact_window_key(
            7,
            || focus.get(),
            || {
                fast.set(fast.get() + 1);
                true // posted, the app ignored it
            },
            || {
                full.set(full.get() + 1);
                focus.set(at(Some(7), Some(7)));
                true
            },
            || panic!("ax"),
        );
        assert_eq!(r, Ok(KeyPrime::KeyRecords));
        assert_eq!((fast.get(), full.get()), (1, 1));
    }

    #[test]
    fn key_goes_back_to_the_previous_window() {
        assert_eq!(restore_target(7, Some(9), &[9, 7, 5], &[7, 9, 5]), Some(9));
    }

    /// LibreOffice hid the Find & Replace dialog (9) of document 5 when
    /// document 7 became key: hand key to 5, which re-shows the dialog.
    #[test]
    fn a_hidden_previous_dialog_hands_key_to_the_frontmost_remaining_window() {
        assert_eq!(restore_target(7, Some(9), &[9, 5, 7], &[7, 5]), Some(5));
    }

    #[test]
    fn nothing_to_restore_when_only_the_target_is_left() {
        assert_eq!(restore_target(7, Some(7), &[7], &[7]), None);
        assert_eq!(restore_target(7, None, &[], &[7]), None);
    }

    /// The Find & Replace leak: the dialog keeps key, nothing redirects. Must
    /// refuse, never post.
    #[test]
    fn unconfirmed_key_window_refuses_instead_of_leaking() {
        for focused in [at(Some(9), Some(9)), at(None, None), at(Some(9), Some(7))] {
            let r = prime_exact_window_key(7, || focused, || false, || true, || true);
            assert!(r.unwrap_err().contains("no keys were sent"));
        }
    }
}

#[cfg(test)]
mod primed_key_tests {
    use super::{
        note_primed_key, primed_key_verdict, primed_keys_for_test, release_on_session_end,
        PrimedKey, PrimedKeyVerdict, PRIMED_KEY_HOLD,
    };
    use std::time::{Duration, Instant};

    fn primed(pid: i32, age: Duration) -> PrimedKey {
        PrimedKey {
            pid,
            window_id: 7,
            at: Instant::now() - age,
            release_requested: false,
        }
    }

    /// Not front by WindowServer's live answer, no action in flight.
    const BEHIND: Option<bool> = Some(false);

    #[test]
    fn a_fresh_priming_is_kept_while_the_person_is_idle() {
        let key = primed(4, Duration::from_millis(300));
        // Last physical input 5 s ago, before the priming.
        assert_eq!(
            primed_key_verdict(&key, BEHIND, Some(5_000), Instant::now(), false),
            PrimedKeyVerdict::Keep
        );
    }

    #[test]
    fn the_persons_input_after_the_priming_releases_it() {
        let key = primed(4, Duration::from_millis(800));
        assert_eq!(
            primed_key_verdict(&key, BEHIND, Some(100), Instant::now(), false),
            PrimedKeyVerdict::Release
        );
    }

    #[test]
    fn an_old_priming_is_released() {
        let key = primed(4, PRIMED_KEY_HOLD + Duration::from_millis(1));
        assert_eq!(
            primed_key_verdict(&key, BEHIND, Some(60_000), Instant::now(), false),
            PrimedKeyVerdict::Release
        );
    }

    #[test]
    fn a_requested_release_is_released() {
        let mut key = primed(4, Duration::from_millis(10));
        key.release_requested = true;
        assert_eq!(
            primed_key_verdict(&key, BEHIND, Some(60_000), Instant::now(), false),
            PrimedKeyVerdict::Release
        );
    }

    #[test]
    fn an_app_the_person_brought_forward_is_left_alone() {
        // Its key window is the person's now: forget it, post nothing.
        let key = primed(4, Duration::from_millis(800));
        assert_eq!(
            primed_key_verdict(&key, Some(true), Some(100), Instant::now(), false),
            PrimedKeyVerdict::Forget
        );
    }

    #[test]
    fn an_unknown_front_app_never_gets_a_defocus_record() {
        let key = primed(4, PRIMED_KEY_HOLD + Duration::from_millis(1));
        assert_eq!(
            primed_key_verdict(&key, None, Some(100), Instant::now(), false),
            PrimedKeyVerdict::Forget
        );
    }

    #[test]
    fn an_action_in_flight_on_the_app_defers_the_release() {
        // The action may rely on the key window (a WindowKeyScope that found
        // it already key): the release waits until the action's lease ends.
        let key = primed(4, PRIMED_KEY_HOLD + Duration::from_millis(1));
        assert_eq!(
            primed_key_verdict(&key, BEHIND, Some(100), Instant::now(), true),
            PrimedKeyVerdict::Keep
        );
    }

    #[test]
    fn an_unknown_idle_time_does_not_release_early() {
        let key = primed(4, Duration::from_millis(300));
        assert_eq!(
            primed_key_verdict(&key, BEHIND, None, Instant::now(), false),
            PrimedKeyVerdict::Keep
        );
    }

    #[test]
    fn a_session_end_releases_only_when_no_other_session_is_live() {
        assert!(release_on_session_end(0));
        assert!(!release_on_session_end(1));
    }

    /// Tests that publish, sweep or count into the process-wide registry
    /// run one at a time, with the key-scope tests that count into it.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        super::key_scope_tests_serial()
    }

    /// The release takes the app's mutation lease itself, so an action that
    /// holds it (or starts while the release runs) is never cut short.
    #[test]
    fn a_release_waits_while_an_action_holds_the_apps_lease() {
        let _serial = serial();
        let ours = |pid: i32| primed_keys_for_test().iter().any(|k| k.pid == pid);
        let action = crate::background_mutation::try_acquire(-90_010).expect("free lease");
        note_primed_key(-90_010, 1);
        // Another app's priming asks for -90_010's release.
        note_primed_key(-90_011, 2);
        assert!(ours(-90_010), "released during the action");
        drop(action);
        note_primed_key(-90_012, 3);
        assert!(!ours(-90_010), "kept after the action ended");
    }

    /// A key scope can outlive its action's lease (fields drop in reverse
    /// declaration order): from its creation until its hand-back has run, a
    /// release waits as for an action.
    #[test]
    fn a_live_key_scope_counts_as_an_action_in_flight() {
        let _serial = serial();
        let ours = |pid: i32| primed_keys_for_test().iter().any(|k| k.pid == pid);
        assert!(!super::key_restores_pending());
        note_primed_key(-90_020, 1);
        let scope = super::LiveKeyScope::new();
        assert!(super::key_restores_pending());
        note_primed_key(-90_021, 2);
        assert!(ours(-90_020), "released while a key scope was live");
        drop(scope);
        assert!(!super::key_restores_pending());
        note_primed_key(-90_022, 3);
        assert!(!ours(-90_020));
    }

    #[test]
    fn a_new_priming_replaces_its_apps_entry_and_lets_other_apps_go() {
        let _serial = serial();
        // No such processes: WindowServer cannot say who is front, so the
        // other app's entry is dropped without a record being posted.
        note_primed_key(-90_001, 1);
        note_primed_key(-90_001, 2);
        let ours = |keys: &[PrimedKey]| {
            keys.iter()
                .filter(|k| k.pid == -90_001 || k.pid == -90_002)
                .map(|k| (k.pid, k.window_id))
                .collect::<Vec<_>>()
        };
        assert_eq!(ours(&primed_keys_for_test()), vec![(-90_001, 2)]);
        note_primed_key(-90_002, 3);
        assert_eq!(ours(&primed_keys_for_test()), vec![(-90_002, 3)]);
    }
}

#[cfg(test)]
mod wp2_scope_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn the_record_sequence_posts_every_record_in_order() {
        let mut sent = 0;
        let mut settles = 0;
        let posts = post_record_sequence(
            Some(7),
            8,
            |_| {
                sent += 1;
                true
            },
            || settles += 1,
        );
        assert_eq!(sent, 4);
        assert_eq!(settles, 2, "a pause after the defocus and the focus record");
        assert_eq!(
            posts,
            RecordPosts {
                posted: 4,
                failed: 0
            }
        );
        assert!(posts.complete());
        // No previous key window: no defocus record.
        let posts = post_record_sequence(None, 8, |_| true, || {});
        assert_eq!(posts.posted, 3);
    }

    /// Both forms post the same records; they differ in the pause between
    /// them, and the short pause is what the short form saves. The defocus
    /// record must stay: without it an AppKit app took the switch once and
    /// ignored it from then on.
    #[test]
    fn the_short_form_differs_from_the_full_one_only_in_its_pause() {
        let mut order = Vec::new();
        let _ = post_record_sequence(
            Some(7),
            8,
            |record| {
                order.push(record[0x8A]);
                true
            },
            || {},
        );
        assert_eq!(order[0], 0x02, "the defocus record leads");
        assert_eq!(order[1], 0x01, "then the focus record");
        assert!(RecordStyle::Fast.pause() < RecordStyle::Full.pause());
        assert!(
            RecordStyle::Fast.pause() >= std::time::Duration::from_millis(5),
            "pauses of 5 ms and up were measured to work, none was not"
        );
        assert_eq!(RecordStyle::Full.pause(), KEY_RECORD_SETTLE);
        assert!(FAST_CONFIRM < KEY_RECORD_SETTLE);
    }

    #[test]
    fn a_failed_post_is_kept_as_unknown_delivery() {
        let mut n = 0;
        let posts = post_record_sequence(
            Some(7),
            8,
            |_| {
                n += 1;
                n != 2
            },
            || {},
        );
        assert_eq!(
            posts,
            RecordPosts {
                posted: 3,
                failed: 1
            }
        );
        assert!(!posts.complete());
    }

    #[test]
    fn hand_back_outcomes() {
        let full = RecordPosts {
            posted: 4,
            failed: 0,
        };
        assert_eq!(hand_back_outcome(Some(full), || true), HandBack::Restored);
        assert_eq!(
            hand_back_outcome(Some(full), || false),
            HandBack::NotConfirmed
        );
        assert_eq!(
            hand_back_outcome(None, || unreachable!()),
            HandBack::NotConfirmed
        );
    }

    #[test]
    fn disclosure_names_what_entering_did() {
        let none = KeyEntry::default();
        assert_eq!(scope_disclosure(none, Some(HandBack::NotMoved)), "");
        assert_eq!(scope_disclosure(none, None), "");
        let records = KeyEntry {
            records_posted: 4,
            ..KeyEntry::default()
        };
        let text = scope_disclosure(records, Some(HandBack::Restored));
        assert!(text.contains("4 key-window record(s) were posted"));
        assert!(text.contains("went back"));
        assert!(!text.contains("accessibility"));
        let ax_only = KeyEntry {
            ax_write: true,
            ..KeyEntry::default()
        };
        let text = scope_disclosure(ax_only, Some(HandBack::NotConfirmed));
        assert!(!text.contains("record"));
        assert!(text.contains("through accessibility"));
        assert!(text.contains("does not read back"));
        let both = KeyEntry {
            records_posted: 2,
            records_uncertain: true,
            ax_write: true,
        };
        let text = scope_disclosure(both, None);
        assert!(text.contains("2 key-window record(s)"));
        assert!(text.contains("may still have reached"));
        assert!(text.contains("through accessibility"));
        assert!(!text.contains("Key focus"));
    }

    static SLOW_BACK_DONE: AtomicBool = AtomicBool::new(false);
    static FAILED_BACK_RAN: AtomicBool = AtomicBool::new(false);
    static UNWOUND_BACK_RAN: AtomicBool = AtomicBool::new(false);

    static HAND_BACK_GATE: std::sync::LazyLock<(
        std::sync::Mutex<(bool, bool)>,
        std::sync::Condvar,
    )> = std::sync::LazyLock::new(|| {
        (
            std::sync::Mutex::new((false, false)),
            std::sync::Condvar::new(),
        )
    });

    fn gated_hand_back(_: pid_t, _: u32, _: KeyPrime, _: u32) -> HandBack {
        let (lock, cv) = &*HAND_BACK_GATE;
        let mut state = lock.lock().unwrap();
        state.0 = true;
        cv.notify_all();
        while !state.1 {
            state = cv.wait(state).unwrap();
        }
        HandBack::Restored
    }

    #[test]
    fn hand_back_changes_focus_generation_at_start_and_completion() {
        let _serial = super::key_scope_tests_serial();
        let (lock, cv) = &*HAND_BACK_GATE;
        *lock.lock().unwrap() = (false, false);
        let before = focus_generation();
        let outcome = scope_with(gated_hand_back).finish();
        let mut state = lock.lock().unwrap();
        while !state.0 {
            state = cv.wait(state).unwrap();
        }
        let started = focus_generation();
        state.1 = true;
        cv.notify_all();
        drop(state);
        assert!(await_key_restores());
        let finished = focus_generation();
        assert_eq!(outcome, HandBack::Deferred);
        assert_eq!(
            (started > before, finished > started),
            (true, true),
            "hand-back start and completion must both invalidate tickets"
        );
    }

    fn order_with_previous(_: pid_t) -> Vec<u32> {
        vec![1]
    }

    /// A hand-back that takes 300 ms.
    fn slow_hand_back(_: pid_t, _: u32, _: KeyPrime, previous: u32) -> HandBack {
        assert_eq!(previous, 1);
        std::thread::sleep(std::time::Duration::from_millis(300));
        SLOW_BACK_DONE.store(true, Ordering::SeqCst);
        HandBack::Restored
    }

    /// A hand-back that does not confirm.
    fn failing_hand_back(_: pid_t, _: u32, _: KeyPrime, _: u32) -> HandBack {
        FAILED_BACK_RAN.store(true, Ordering::SeqCst);
        HandBack::NotConfirmed
    }

    fn unwound_hand_back(_: pid_t, _: u32, _: KeyPrime, _: u32) -> HandBack {
        UNWOUND_BACK_RAN.store(true, Ordering::SeqCst);
        HandBack::Restored
    }

    fn scope_with(hand_back: fn(pid_t, u32, KeyPrime, u32) -> HandBack) -> WindowKeyScope {
        WindowKeyScope {
            pid: -77_001,
            target: 2,
            previous: Some(1),
            order_before: vec![2, 1],
            primed: KeyPrime::KeyRecords,
            from_belief: false,
            entry: KeyEntry::default(),
            done: false,
            ops: ScopeOps {
                window_order: order_with_previous,
                hand_back,
            },
            live: Some(LiveKeyScope::new()),
        }
    }

    /// The result returns before the hand-back finishes, for every caller:
    /// the hand-back runs on its own thread.
    #[test]
    fn finish_returns_before_the_hand_back_finishes() {
        let _serial = super::key_scope_tests_serial();
        SLOW_BACK_DONE.store(false, Ordering::SeqCst);
        let started = std::time::Instant::now();
        let outcome = scope_with(slow_hand_back).finish();
        assert_eq!(outcome, HandBack::Deferred);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(150),
            "finish waited {:?}",
            started.elapsed()
        );
        assert!(
            !SLOW_BACK_DONE.load(Ordering::SeqCst),
            "the hand-back had already finished"
        );
        // The next scope's entry waits for it, so two never interleave.
        assert!(await_key_restores());
        assert!(SLOW_BACK_DONE.load(Ordering::SeqCst));
    }

    /// Dropping a scope returns just as fast.
    #[test]
    fn dropping_a_scope_does_not_wait_for_the_hand_back() {
        let _serial = super::key_scope_tests_serial();
        SLOW_BACK_DONE.store(false, Ordering::SeqCst);
        let started = std::time::Instant::now();
        drop(scope_with(slow_hand_back));
        assert!(started.elapsed() < std::time::Duration::from_millis(150));
        assert!(!SLOW_BACK_DONE.load(Ordering::SeqCst));
        assert!(await_key_restores());
        assert!(SLOW_BACK_DONE.load(Ordering::SeqCst));
    }

    /// A hand-back that fails is only logged: nothing turns it into a
    /// refusal or a wait.
    #[test]
    fn a_failed_hand_back_is_neither_waited_for_nor_reported_as_a_refusal() {
        let _serial = super::key_scope_tests_serial();
        FAILED_BACK_RAN.store(false, Ordering::SeqCst);
        let outcome = scope_with(failing_hand_back).finish();
        assert_eq!(outcome, HandBack::Deferred);
        assert!(await_key_restores());
        assert!(FAILED_BACK_RAN.load(Ordering::SeqCst));
    }

    /// A scope dropped while unwinding from a panic still hands key back.
    #[test]
    fn a_dropped_scope_still_hands_back_even_on_panic() {
        let _serial = super::key_scope_tests_serial();
        UNWOUND_BACK_RAN.store(false, Ordering::SeqCst);
        let unwound = std::panic::catch_unwind(|| {
            let _scope = scope_with(unwound_hand_back);
            panic!("action failed mid-way");
        });
        assert!(unwound.is_err());
        assert!(await_key_restores());
        assert!(UNWOUND_BACK_RAN.load(Ordering::SeqCst));
    }

    #[test]
    fn a_scope_that_never_moved_key_has_nothing_to_hand_back() {
        let _serial = super::key_scope_tests_serial();
        let mut already_key = scope_with(failing_hand_back);
        already_key.primed = KeyPrime::AlreadyKey;
        assert_eq!(already_key.finish(), HandBack::NotMoved);
    }

    #[test]
    fn validation_posts_the_records_and_the_pair_and_reports_what_was_posted() {
        // Records, the key confirmation and the Cmd+F20 pair, whichever app
        // is in front.
        let mut posted = Vec::new();
        let post = menu_validation_with(
            || true,
            |_| true,
            || true,
            || Some((1u8, 2u8)),
            |e| {
                posted.push(*e);
                true
            },
        );
        assert_eq!(posted, vec![1, 2]);
        assert_eq!((post.records, post.keys), (2, 2));
        assert_eq!(post.delivery(), ValidationDelivery::Sent);

        // Key not confirmed: no Cmd+F20.
        let post = menu_validation_with(
            || true,
            |_| true,
            || false,
            || Some((1u8, 2u8)),
            |_| unreachable!(),
        );
        assert_eq!((post.records, post.keys), (2, 0));

        // Nothing resolvable: nothing called.
        let post = menu_validation_with(
            || false,
            |_| unreachable!(),
            || unreachable!(),
            || Some((1u8, 2u8)),
            |_| unreachable!(),
        );
        assert_eq!(post.delivery(), ValidationDelivery::NoneSent);
        assert_eq!(post.disclosure(), "");

        // A failed record post stops before the pair and is uncertain.
        let mut n = 0;
        let post = menu_validation_with(
            || true,
            |_| {
                n += 1;
                n == 1
            },
            || unreachable!(),
            || Some((1u8, 2u8)),
            |_| unreachable!(),
        );
        assert_eq!(
            post,
            ValidationPost {
                records: 1,
                keys: 0,
                key_attempts: 0,
                uncertain: true,
                called: true
            }
        );
        assert_eq!(post.delivery(), ValidationDelivery::Possible);
        assert!(post.disclosure().contains("may still have reached"));

        // A failed key-down is followed by the key-up.
        let mut posted = Vec::new();
        let post = menu_validation_with(
            || true,
            |_| true,
            || true,
            || Some((1u8, 2u8)),
            |e| {
                posted.push(*e);
                *e == 2
            },
        );
        assert_eq!(posted, vec![1, 2]);
        assert_eq!(
            (post.records, post.keys, post.key_attempts, post.uncertain),
            (2, 1, 2, true)
        );
        assert!(post.disclosure().contains("1 of 2 Cmd+F20"));

        let post = menu_validation_with(|| true, |_| true, || true, || Some((1u8, 2u8)), |_| true);
        assert_eq!(
            post,
            ValidationPost {
                records: 2,
                keys: 2,
                key_attempts: 2,
                uncertain: false,
                called: true
            }
        );
    }
}

#[cfg(test)]
mod entry_hand_back_tests {
    use super::{entry_hand_back_kind, KeyEntry, KeyPrime};

    #[test]
    fn a_failed_entry_hands_back_whatever_may_have_moved_key() {
        assert_eq!(
            entry_hand_back_kind(KeyEntry::default()),
            None,
            "nothing touched"
        );
        let records = KeyEntry {
            records_posted: 2,
            ..KeyEntry::default()
        };
        assert_eq!(entry_hand_back_kind(records), Some(KeyPrime::KeyRecords));
        let uncertain = KeyEntry {
            records_uncertain: true,
            ..KeyEntry::default()
        };
        assert_eq!(entry_hand_back_kind(uncertain), Some(KeyPrime::KeyRecords));
        let ax_only = KeyEntry {
            ax_write: true,
            ..KeyEntry::default()
        };
        assert_eq!(
            entry_hand_back_kind(ax_only),
            Some(KeyPrime::AxFocusedWindow)
        );
        let both = KeyEntry {
            records_posted: 4,
            records_uncertain: false,
            ax_write: true,
        };
        assert_eq!(entry_hand_back_kind(both), Some(KeyPrime::KeyRecords));
    }
}

/// A key scope's entry and hand-back while the target's app is frontmost:
/// keys and chords reach it like any other app.
#[cfg(test)]
#[path = "skylight_scope_tests.rs"]
mod frontmost_scope_tests;
