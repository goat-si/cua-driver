//! FocusGuard — Rust port of Swift's `FocusGuard.withFocusSuppressed`
//! (`libs/cua-driver/Sources/CuaDriverCore/Focus/FocusGuard.swift`).
//!
//! ## What this does
//!
//! Wraps an individual AX-action call (the inside of a click / type /
//! set_value / drag dispatch) with a **targeted** focus-steal suppression
//! lease. Catches the case where dispatching an AX attribute write
//! triggers a reflexive self-activation in the target app — Safari /
//! WebKit will sometimes pull itself to the front when an
//! `AXSelectedText` write hits a focused input, even when the AX call
//! itself doesn't go through `NSApp.activate(…)`.
//!
//! ## Layering relative to Swift
//!
//! Swift's `FocusGuard.withFocusSuppressed` does three things:
//!
//! 1. **Enablement** — set `AXManualAccessibility` /
//!    `AXEnhancedUserInterface` on the app root so Chromium/Electron
//!    targets respond to attribute-level dispatch.
//! 2. **Synthetic focus** — write `AXFocused` / `AXMain` on the
//!    enclosing window + element before the action, restore after.
//!    Makes AppKit behave as if the action came from a focused
//!    process, preventing reflex activations.
//! 3. **Reactive** — arm `SystemFocusStealPreventer` with a targeted
//!    entry. If the target self-activates anyway, re-activates the
//!    prior frontmost.
//!
//! This Rust port only ships **layer 3** (the reactive suppressor).
//! Layers 1+2 require AX assertion + AX attribute write/restore machinery
//! that isn't yet ported — and empirically the layer-3 reactive guard
//! catches the majority of side-effects when combined with
//! `WindowChangeDetector`'s wildcard lease at the snapshot→detect
//! boundary. This gap is a known, intentional limitation.
//!
//! ## Why this is a separate module
//!
//! `focus_steal::with_suppression` already exists as a thin closure
//! API around the dispatcher. `focus_guard::with_focus_suppressed`
//! wraps it with:
//!
//! 1. A "is target already frontmost?" check so we don't arm a useless
//!    self→self suppressor (matches Swift's `isTargetFrontmost` guard).
//! 2. A 50ms post-action hold that gives any in-flight focus-grab
//!    reflex time to fire and be observed by the suppressor before the
//!    lease is dropped. Matches Swift's `Task.sleep(nanoseconds: 50ms)`,
//!    except that the hold runs detached: the action's result does not
//!    wait for it (it was a fixed 50 ms of every background click).
//! 3. A static `origin` label for tracing — call sites pass a short
//!    string like `"click.AXPress"` so leaked leases / late-firing
//!    observers can be traced back to the caller.

use crate::goat::action_latency as latency;

use std::time::Duration;

use crate::apps;
use crate::focus_steal;

/// Wrap an async closure `f` with a targeted focus-steal suppressor.
///
/// - `target_pid` — the pid the action is dispatched to. `Some(pid)` is
///   the standard case; `None` skips the targeted entry entirely
///   (caller relies on the surrounding `WindowChangeDetector` wildcard
///   lease).
/// - `prior_frontmost` — the pid to restore focus to if the target
///   activates. Typically captured from `apps::frontmost_pid()` before
///   the snapshot.
/// - `origin` — short static label for tracing, e.g. `"click.AXPress"`.
/// - `f` — the action to run with suppression armed.
///
/// Returns whatever `f` returns as soon as it resolves. The lease stays
/// armed for another ~50ms on a detached task so any in-flight reflex
/// activation is still observed before suppression ends.
///
/// If `target_pid` is already the frontmost app (no point fighting
/// ourselves) or `prior_frontmost` is `None` (no app to restore to),
/// the function still runs `f` — just without the lease. This keeps
/// the call sites simple (no per-tool `if frontmost == target` ladders).
pub async fn with_focus_suppressed<F, Fut, R>(
    target_pid: Option<i32>,
    prior_frontmost: Option<i32>,
    origin: &'static str,
    f: F,
) -> R
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = R>,
{
    // Decide whether to arm a targeted lease. Three conditions must hold:
    //   1. We have a known target pid (Some).
    //   2. We have a prior frontmost to restore to (Some).
    //   3. The target isn't already frontmost — Swift's
    //      `isTargetFrontmost` short-circuit, mirrored here. Without
    //      it we'd arm an entry that re-activates the prior frontmost
    //      against a no-op activation, fighting our own previous
    //      restore.
    let should_arm = match (target_pid, prior_frontmost) {
        (Some(tp), Some(pf)) => tp != pf,
        _ => false,
    };

    let _lease = if should_arm {
        // unwrap()s are safe — `should_arm` is true only when both are Some.
        Some(focus_steal::begin_suppression(
            target_pid,
            prior_frontmost.unwrap(),
            origin,
        ))
    } else {
        None
    };

    // Hand key focus back to the user's app if the target opens a menu
    // (popup, select, context menu) while it is in the background.
    let _key_focus = match target_pid {
        Some(tp) if prior_frontmost != Some(tp) => {
            latency::spawn_blocking(move || crate::key_focus_guard::protect(tp))
                .await
                .ok()
                .flatten()
        }
        _ => None,
    };

    let result = latency::timed(latency::Stage::Action, f()).await;

    // Post-action settle — give the reactive observer time to fire on
    // any side-effect activation before we drop the lease. Matches
    // Swift's 50ms sleep in `FocusGuard.withFocusSuppressed`. The hold
    // only keeps the suppressor armed, so it runs detached and the
    // result returns now.
    if let Some(lease) = _lease {
        hold_then_release(lease, POST_ACTION_SETTLE);
    }
    result
}

/// Convenience wrapper that captures the current frontmost via
/// `apps::frontmost_pid()` at call time. Use when the caller doesn't
/// already have a `prior_frontmost` from a surrounding snapshot —
/// e.g. tools that only opt into the layer-3 guard without the
/// snapshot/detect cycle.
///
/// Most action tools should prefer `with_focus_suppressed` with an
/// explicit `prior_frontmost` captured *before* the snapshot, so the
/// wildcard lease + targeted lease both restore to the same pid.
/// How long a targeted lease stays armed after the action returns.
const POST_ACTION_SETTLE: Duration = Duration::from_millis(50);

/// Leases kept armed after their action returned, by hold id.
static DETACHED_HOLDS: std::sync::Mutex<Vec<(u64, focus_steal::SuppressionLease)>> =
    std::sync::Mutex::new(Vec::new());

/// Keep `lease` armed for `hold` on a detached task. The caller is already
/// on a Tokio runtime (it awaited the action there). The next action or
/// intentional activation ends the hold early ([`release_detached_holds`],
/// called from `window_change_detector::end_tail`), so a hold never reverts
/// an activation the driver asked for.
fn hold_then_release(lease: focus_steal::SuppressionLease, hold: Duration) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    DETACHED_HOLDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((id, lease));
    tokio::spawn(async move {
        tokio::time::sleep(hold).await;
        let released = {
            let mut holds = DETACHED_HOLDS.lock().unwrap_or_else(|e| e.into_inner());
            holds
                .iter()
                .position(|(held, _)| *held == id)
                .map(|at| holds.swap_remove(at))
        };
        drop(released);
    });
}

/// End every detached post-action hold now.
pub fn release_detached_holds() {
    let holds = std::mem::take(&mut *DETACHED_HOLDS.lock().unwrap_or_else(|e| e.into_inner()));
    drop(holds);
}

/// Serializes tests that arm or end the process-wide detached holds.
#[cfg(test)]
pub(crate) static HOLD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Detached holds still armed.
#[cfg(test)]
pub(crate) fn detached_hold_count() -> usize {
    DETACHED_HOLDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len()
}

pub async fn with_focus_suppressed_now<F, Fut, R>(
    target_pid: Option<i32>,
    origin: &'static str,
    f: F,
) -> R
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = R>,
{
    let prior = apps::frontmost_pid();
    with_focus_suppressed(target_pid, prior, origin, f).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The next action or intentional activation (`end_tail`) ends a
    /// post-action hold that would otherwise stay armed for its 50 ms.
    /// Installs the live NSWorkspace activation observer, so it runs alone in
    /// the macOS slow lane (goat-si/cua-driver#151).
    #[tokio::test]
    #[ignore = "live: installs an NSWorkspace activation observer; runs alone in the macOS live lane"]
    async fn the_next_action_ends_a_detached_hold() {
        let _serial = HOLD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let lease = focus_steal::begin_suppression(Some(-41), -42, "test.hold");
        hold_then_release(lease, Duration::from_secs(30));
        assert!(detached_hold_count() >= 1);
        crate::window_change_detector::end_tail();
        assert_eq!(detached_hold_count(), 0);
    }

    /// A background key hand-back runs on its own thread right after the
    /// action returns. It is not a new action: the action's own post-action
    /// hold must stay armed through it.
    /// Installs the live NSWorkspace activation observer and reads SkyLight
    /// window owners, so it runs alone in the macOS slow lane (#151).
    #[tokio::test]
    #[ignore = "live: installs an NSWorkspace activation observer, reads SkyLight; runs alone in the macOS live lane"]
    async fn a_key_hand_back_keeps_the_actions_hold() {
        let _serial = HOLD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::window_change_detector::end_tail();
        let lease = focus_steal::begin_suppression(Some(-43), -44, "test.hand_back");
        hold_then_release(lease, Duration::from_secs(30));
        // No such process: the records are not posted, the path still runs.
        crate::input::skylight::hand_key_back(-1, Some(1), 2);
        assert_eq!(detached_hold_count(), 1);
        crate::window_change_detector::end_tail();
    }
}
