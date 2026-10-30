//! Background pixel focus hand-back and its injectable operations.
use super::click::PixelActivationPolicy;
use crate::{apps, goat::action_latency as latency};

/// Return the prior foreground pid that should be restored after a raw
/// background pixel click.
///
/// This decision deliberately depends on observed application state rather
/// than the private focus recipe's return value. The recipe can be unavailable
/// or partially fail while the raw click still makes the target AppKit-active;
/// in that case the allow-target suppression lease will not restore it for us.
pub(super) fn background_pixel_restore_pid(
    activation_policy: PixelActivationPolicy,
    prior_front: Option<i32>,
    target_pid: i32,
    observed_front: Option<i32>,
) -> Option<i32> {
    if activation_policy == PixelActivationPolicy::AllowTargetWithoutRaise
        && prior_front != Some(target_pid)
        && observed_front == Some(target_pid)
    {
        prior_front
    } else {
        None
    }
}

/// Hand the user's frontmost app and key window back after a raw background
/// pixel action that ran the focus-without-raise prologue (click, drag).
///
/// The no-raise record can make NSWorkspace report the target as active even
/// though its window never moved in z-order. Once the input has been queued,
/// restore the prior app if the target is still reported frontmost. Base this
/// on observed state, not `focus_without_raise`: the private recipe can report
/// failure after partially activating the target, and the raw input can
/// self-activate even when that recipe is unavailable. Do not overwrite a
/// different app here; the wildcard suppression lease handles genuine side
/// effects.
pub(super) async fn restore_user_focus_after_background_pixel(
    pid: i32,
    prior_front: Option<i32>,
    prior_key_wid: Option<u32>,
    window_id: Option<u32>,
    focus_without_raise: bool,
) {
    if prior_front == Some(pid) {
        return;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    restore_user_focus_after_background_pixel_now(
        pid,
        prior_front,
        prior_key_wid,
        window_id,
        focus_without_raise,
        None,
    )
    .await;
}

#[async_trait::async_trait]
pub(super) trait PixelRestoreOps: Sync {
    fn front(&self) -> Option<i32>;
    fn key_focus(&self) -> Option<i32>;
    fn allowed(&self, expected: Option<i32>) -> bool;
    async fn activate(&self, pid: i32);
    async fn focus(&self, previous_pid: i32, prior_key_wid: Option<u32>, pid: i32, wid: u32);
    fn key_window(&self, pid: i32) -> Option<u32>;
}

struct SystemPixelRestoreOps {
    pid: i32,
    baseline: Option<(super::pixel_restore::RestoreBaseline, Option<i32>)>,
}

#[async_trait::async_trait]
impl PixelRestoreOps for SystemPixelRestoreOps {
    fn front(&self) -> Option<i32> {
        apps::frontmost_pid()
    }
    fn key_focus(&self) -> Option<i32> {
        crate::input::skylight::key_focus_pid()
    }
    fn key_window(&self, pid: i32) -> Option<u32> {
        crate::input::skylight::key_window_of_pid(pid)
    }
    fn allowed(&self, expected: Option<i32>) -> bool {
        self.baseline
            .is_none_or(|(guard, _)| guard.valid_after_reactivation(expected, self.pid))
    }
    async fn activate(&self, pid: i32) {
        let _ = apps::activate_pid(pid);
        wait_until_frontmost(pid, std::time::Duration::from_millis(300)).await;
    }
    async fn focus(&self, previous_pid: i32, prior_key_wid: Option<u32>, pid: i32, wid: u32) {
        let baseline = self.baseline;
        let _ = latency::spawn_action_blocking(move || {
            crate::input::skylight::restore_focus_after_without_raise(
                previous_pid,
                prior_key_wid,
                pid,
                wid,
                || {
                    baseline.is_none_or(|(guard, _)| {
                        guard.valid_after_reactivation(Some(previous_pid), pid)
                    })
                },
            )
        })
        .await;
    }
}

pub(super) async fn restore_user_focus_after_background_pixel_now(
    pid: i32,
    prior_front: Option<i32>,
    prior_key_wid: Option<u32>,
    window_id: Option<u32>,
    focus_without_raise: bool,
    baseline: Option<(super::pixel_restore::RestoreBaseline, Option<i32>)>,
) -> bool {
    let ops = SystemPixelRestoreOps { pid, baseline };
    restore_user_focus_with(
        &ops,
        pid,
        prior_front,
        prior_key_wid,
        window_id,
        focus_without_raise,
        baseline.and_then(|(_, expected)| expected),
    )
    .await
}

pub(super) async fn restore_user_focus_with(
    ops: &impl PixelRestoreOps,
    pid: i32,
    prior_front: Option<i32>,
    prior_key_wid: Option<u32>,
    window_id: Option<u32>,
    focus_without_raise: bool,
    expected_front: Option<i32>,
) -> bool {
    if prior_front == Some(pid) {
        let key_matches = ops.key_focus() == Some(pid)
            && prior_key_wid.is_none_or(|wid| ops.key_window(pid) == Some(wid));
        if ops.front() == Some(pid) && !key_matches && ops.allowed(Some(pid)) {
            if let Some(wid) = window_id {
                ops.focus(pid, prior_key_wid, pid, wid).await;
            }
        }
        return ops.front() == Some(pid)
            && ops.key_focus() == Some(pid)
            && prior_key_wid.is_none_or(|wid| ops.key_window(pid) == Some(wid));
    }
    let reactivated = if let Some(previous_pid) = background_pixel_restore_pid(
        PixelActivationPolicy::AllowTargetWithoutRaise,
        prior_front,
        pid,
        ops.front(),
    ) {
        if !ops.allowed(expected_front) {
            return false;
        }
        ops.activate(previous_pid).await;
        true
    } else {
        false
    };
    if let (Some(previous_pid), Some(wid)) = (prior_front, window_id) {
        let key_moved = user_focus_needs_restore(
            reactivated,
            focus_without_raise,
            ops.key_focus(),
            previous_pid,
        );
        if key_moved && ops.front() == Some(previous_pid) && ops.allowed(Some(previous_pid)) {
            ops.focus(previous_pid, prior_key_wid, pid, wid).await;
        }
    }
    prior_front.is_some_and(|previous_pid| {
        ops.front() == Some(previous_pid)
            && ops.key_focus() == Some(previous_pid)
            && prior_key_wid.is_none_or(|wid| ops.key_window(previous_pid) == Some(wid))
    })
}

/// Whether the user's window needs the focus record back after a background
/// pixel action: their app was re-activated, or WindowServer's key focus
/// (`key_focus`) left it. Not when it never moved, since the record then makes
/// their app become active again.
pub(super) fn user_focus_needs_restore(
    reactivated: bool,
    focus_without_raise: bool,
    key_focus: Option<i32>,
    previous_pid: i32,
) -> bool {
    reactivated || (focus_without_raise && key_focus.is_some_and(|pid| pid != previous_pid))
}

/// Poll until `pid` is the frontmost application or `budget` runs out.
async fn wait_until_frontmost(pid: i32, budget: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + budget;
    loop {
        if apps::frontmost_pid() == Some(pid) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    }
}
