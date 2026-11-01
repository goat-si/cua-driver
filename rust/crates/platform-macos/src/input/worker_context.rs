//! Propagation of the desktop-input worker context across spawned work.

use std::cell::RefCell;
use std::future::Future;

use cua_driver_core::desktop_lease::{worker_hold, WorkerHold};

tokio::task_local! {
    static ASYNC_HOLD: Option<WorkerHold>;
}

thread_local! {
    static THREAD_HOLD: RefCell<Option<WorkerHold>> = const { RefCell::new(None) };
}

// Prefer a newly admitted dispatch to an inherited scope, and clone before
// spawning: neither Tokio task locals nor thread locals cross spawn boundaries.
fn capture() -> Option<WorkerHold> {
    worker_hold()
        .or_else(|| ASYNC_HOLD.try_with(Clone::clone).ok().flatten())
        .or_else(|| THREAD_HOLD.with(|slot| slot.borrow().clone()))
}

pub(crate) fn in_thread_scope<R>(hold: Option<WorkerHold>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<WorkerHold>);
    impl Drop for Restore {
        fn drop(&mut self) {
            THREAD_HOLD.with(|slot| slot.replace(self.0.take()));
        }
    }
    let _restore = Restore(THREAD_HOLD.with(|slot| slot.replace(hold)));
    f()
}

#[cfg(test)]
pub(crate) fn thread_scope_present() -> bool {
    THREAD_HOLD.with(|slot| slot.borrow().is_some())
}

pub(crate) fn wrap_blocking<F, R>(f: F) -> impl FnOnce() -> R + Send + 'static
where
    F: FnOnce() -> R + Send + 'static,
{
    let hold = capture();
    move || in_thread_scope(hold, f)
}

#[allow(dead_code)]
pub(crate) fn spawn_async<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let hold = capture();
    tokio::spawn(ASYNC_HOLD.scope(hold, future))
}

#[allow(dead_code)]
pub(crate) fn spawn_thread<F, R>(f: F) -> std::thread::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    std::thread::spawn(wrap_blocking(f))
}
