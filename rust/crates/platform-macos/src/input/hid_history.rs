//! Track our own HID posts so they are not mistaken for physical input.
use std::{
    collections::VecDeque,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
struct Post {
    at: Instant,
    idle_before: Option<u64>,
}
#[derive(Default)]
struct History {
    posts: VecDeque<Post>,
    discarded_through: Option<Instant>,
}
static HISTORY: OnceLock<Mutex<History>> = OnceLock::new();

/// Record the physical clock immediately before a HID post. Pid-routed events
/// do not use this path. A bounded history fails closed if it overflowed.
pub(crate) fn post(send: impl FnOnce()) {
    let mut history = HISTORY
        .get_or_init(|| Mutex::new(History::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let idle_before = crate::tools::physical_input_idle_ms();
    let at = Instant::now();
    history.posts.push_back(Post { at, idle_before });
    while history.posts.len() > 50_000
        || history
            .posts
            .front()
            .is_some_and(|post| at.duration_since(post.at) > Duration::from_secs(3))
    {
        if let Some(post) = history.posts.pop_front() {
            history.discarded_through = Some(post.at);
        }
    }
    send();
}

fn quiet_samples(
    start: Instant,
    before: Option<u64>,
    now: Instant,
    idle: Option<u64>,
    posts: impl IntoIterator<Item = Post>,
) -> Option<bool> {
    let mut previous_at = start;
    let mut previous_idle = before?;
    for post in posts {
        if post.at < start || post.at > now {
            continue;
        }
        let elapsed =
            u64::try_from(post.at.duration_since(previous_at).as_millis()).unwrap_or(u64::MAX);
        if post.idle_before?.saturating_add(2) < previous_idle.saturating_add(elapsed) {
            return Some(false);
        }
        previous_at = post.at;
        previous_idle = 0;
    }
    let elapsed = u64::try_from(now.duration_since(previous_at).as_millis()).unwrap_or(u64::MAX);
    Some(idle?.saturating_add(2) >= previous_idle.saturating_add(elapsed))
}

pub(crate) fn quiet_since(
    start: Instant,
    before: Option<u64>,
    now: Instant,
    idle: Option<u64>,
) -> Option<bool> {
    let history = HISTORY
        .get_or_init(|| Mutex::new(History::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if history.discarded_through.is_some_and(|at| at >= start) {
        return None;
    }
    quiet_samples(start, before, now, idle, history.posts.iter().copied())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn own_hid_resets_are_excluded_but_physical_input_is_not() {
        let start = Instant::now();
        let post = Post {
            at: start + Duration::from_millis(20),
            idle_before: Some(1020),
        };
        let now = start + Duration::from_millis(50);
        assert_eq!(
            quiet_samples(start, Some(1000), now, Some(30), [post]),
            Some(true)
        );
        assert_eq!(
            quiet_samples(start, Some(1000), now, Some(5), [post]),
            Some(false)
        );
        let prior_user = Post {
            idle_before: Some(5),
            ..post
        };
        assert_eq!(
            quiet_samples(start, Some(1000), now, Some(30), [prior_user]),
            Some(false)
        );
    }
}
