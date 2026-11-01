use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use cua_driver_core::authorization::PermissionMode;
use cua_driver_core::desktop_lease::{
    test_support::{TestBackend, TestClock},
    LeaseBackend, LeaseClock,
};
use cua_driver_core::protocol::ToolResult;
use cua_driver_core::session_authorization::{SessionAuthorizationRegistry, SessionModeCeiling};
use cua_driver_core::tool::{Tool, ToolDef, ToolRegistry};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::goat::action_latency;
use crate::input::worker_context;

#[derive(Clone, Copy)]
enum Route {
    Blocking,
    Traced,
    Async,
    Thread,
    Nested,
    Multiple,
    Panic,
    Scope,
}

type NativeWorkers = Vec<(oneshot::Sender<()>, std::thread::JoinHandle<()>)>;

enum Worker {
    Started(Box<Worker>, oneshot::Receiver<()>),
    Tokio(tokio::task::JoinHandle<()>),
    Thread(std::thread::JoinHandle<()>),
    Nested(
        tokio::task::JoinHandle<()>,
        oneshot::Receiver<std::thread::JoinHandle<()>>,
    ),
    Multiple(NativeWorkers),
    MultiParent(
        tokio::task::JoinHandle<()>,
        oneshot::Receiver<NativeWorkers>,
    ),
    Panic(tokio::task::JoinHandle<()>, oneshot::Receiver<bool>),
}

impl Worker {
    async fn finish(self) {
        match self {
            Self::Tokio(handle) => handle.await.unwrap(),
            Self::Thread(handle) => handle.join().unwrap(),
            Self::Started(..)
            | Self::Nested(..)
            | Self::Multiple(..)
            | Self::MultiParent(..)
            | Self::Panic(..) => {
                unreachable!()
            }
        }
    }

    async fn prepare(self) -> Self {
        let Self::Started(worker, started) = self else {
            panic!("missing start signal");
        };
        started.await.expect("native work started");
        match *worker {
            Self::Nested(parent, child) => {
                parent.await.unwrap();
                Self::Thread(child.await.unwrap())
            }
            Self::MultiParent(parent, children) => {
                parent.await.unwrap();
                Self::Multiple(children.await.unwrap())
            }
            other => other,
        }
    }
}

struct Probe {
    def: ToolDef,
    route: Route,
    release: Mutex<Option<oneshot::Receiver<()>>>,
    ready: Mutex<Option<oneshot::Sender<Worker>>>,
    cancelled: bool,
    backend: Arc<TestBackend>,
}

impl Probe {
    fn start(&self) {
        let release = self.release.lock().unwrap().take().unwrap();
        let ready = self.ready.lock().unwrap().take().unwrap();
        let (started_tx, started_rx) = oneshot::channel();
        let ready_worker = move |worker| {
            let _ = ready.send(Worker::Started(Box::new(worker), started_rx));
        };
        match self.route {
            Route::Blocking | Route::Traced => {
                let work = move || {
                    let _ = started_tx.send(());
                    let _ = release.blocking_recv();
                };
                let worker = if matches!(self.route, Route::Traced) {
                    action_latency::spawn_action_blocking(work)
                } else {
                    action_latency::spawn_blocking(work)
                };
                ready_worker(Worker::Tokio(worker));
            }
            Route::Async => {
                let worker = worker_context::spawn_async(async move {
                    let _ = started_tx.send(());
                    let _ = release.await;
                });
                ready_worker(Worker::Tokio(worker));
            }
            Route::Thread => {
                let worker = worker_context::spawn_thread(move || {
                    let _ = started_tx.send(());
                    let _ = release.blocking_recv();
                });
                ready_worker(Worker::Thread(worker));
            }
            Route::Nested => {
                let (child_tx, child_rx) = oneshot::channel();
                let parent = action_latency::spawn_blocking(move || {
                    let child = worker_context::spawn_thread(move || {
                        let _ = started_tx.send(());
                        let _ = release.blocking_recv();
                    });
                    let _ = child_tx.send(child);
                });
                ready_worker(Worker::Nested(parent, child_rx));
            }
            Route::Multiple => {
                drop(release);
                let (children_tx, children_rx) = oneshot::channel();
                let parent = action_latency::spawn_blocking(move || {
                    let mut workers = Vec::new();
                    let mut started = Some(started_tx);
                    for _ in 0..2 {
                        let (tx, rx) = oneshot::channel::<()>();
                        let signal = started.take();
                        let handle = worker_context::spawn_thread(move || {
                            if let Some(signal) = signal {
                                let _ = signal.send(());
                            }
                            let _ = rx.blocking_recv();
                        });
                        workers.push((tx, handle));
                    }
                    let _ = children_tx.send(workers);
                });
                ready_worker(Worker::MultiParent(parent, children_rx));
            }
            Route::Panic => {
                struct Cleanup(Arc<TestBackend>, Option<oneshot::Sender<bool>>);
                impl Drop for Cleanup {
                    fn drop(&mut self) {
                        let held = self.0.try_acquire().unwrap().is_none();
                        let _ = self.1.take().unwrap().send(held);
                    }
                }
                let backend = self.backend.clone();
                let (tx, rx) = oneshot::channel();
                let handle = action_latency::spawn_blocking(move || {
                    let _cleanup = Cleanup(backend, Some(tx));
                    let _ = started_tx.send(());
                    let _ = release.blocking_recv();
                    panic!("synthetic native worker panic");
                });
                ready_worker(Worker::Panic(handle, rx));
            }
            Route::Scope => {
                let handle = action_latency::spawn_blocking(move || {
                    assert!(worker_context::thread_scope_present());
                    let panic = std::panic::catch_unwind(|| {
                        worker_context::in_thread_scope(None, || {
                            assert!(!worker_context::thread_scope_present());
                            panic!("synthetic nested panic");
                        });
                    });
                    assert!(panic.is_err());
                    assert!(worker_context::thread_scope_present());
                    let _ = started_tx.send(());
                    let _ = release.blocking_recv();
                });
                ready_worker(Worker::Tokio(handle));
            }
        }
    }
}

#[async_trait]
impl Tool for Probe {
    fn def(&self) -> &ToolDef {
        &self.def
    }

    async fn invoke(&self, _: Value) -> ToolResult {
        if matches!(self.route, Route::Traced) {
            action_latency::with_test_trace(async {
                self.start();
            })
            .await;
        } else {
            self.start();
        }
        if self.cancelled {
            std::future::pending::<()>().await;
        }
        ToolResult::text("done")
    }
}

type Fixture = (
    Arc<ToolRegistry>,
    Arc<TestBackend>,
    Arc<TestClock>,
    oneshot::Sender<()>,
    oneshot::Receiver<Worker>,
);

fn registry(route: Route, cancelled: bool) -> Fixture {
    let backend = Arc::new(TestBackend::default());
    let clock = Arc::new(TestClock::default());
    let mut registry = ToolRegistry::with_test_lease(backend.clone(), clock.clone());
    let (release_tx, release_rx) = oneshot::channel();
    let (ready_tx, ready_rx) = oneshot::channel();
    registry.register(Box::new(Probe {
        def: ToolDef {
            name: "click".into(),
            description: "fake worker".into(),
            input_schema: json!({"type":"object"}),
            read_only: false,
            destructive: false,
            idempotent: false,
            open_world: false,
        },
        route,
        release: Mutex::new(Some(release_rx)),
        ready: Mutex::new(Some(ready_tx)),
        cancelled,
        backend: backend.clone(),
    }));
    (Arc::new(registry), backend, clock, release_tx, ready_rx)
}

async fn invoke(registry: Arc<ToolRegistry>) {
    let ceiling = SessionModeCeiling::for_trusted_sessions(
        [PermissionMode::Unrestricted],
        true,
        Duration::from_secs(60),
        Duration::from_secs(30),
    )
    .unwrap();
    let context = SessionAuthorizationRegistry::with_ceiling(ceiling)
        .compatibility_context(PermissionMode::Unrestricted, None)
        .unwrap();
    let result = registry
        .invoke_with_context(
            "click",
            json!({"pid":42,"window_id":7,"x":10,"y":20}),
            context,
        )
        .await;
    assert_ne!(result.is_error, Some(true));
}

async fn check_route(route: Route, cancelled: bool) {
    let (registry, backend, clock, release, ready) = registry(route, cancelled);
    let dispatch = tokio::spawn(invoke(registry));
    let worker = ready
        .await
        .expect("worker started through registry")
        .prepare()
        .await;
    if cancelled {
        dispatch.abort();
        assert!(dispatch.await.unwrap_err().is_cancelled());
        clock.sleep(Duration::from_secs(6)).await;
        // The cancelled dispatch schedules its cleanup before this marker.
        tokio::spawn(async {}).await.unwrap();
    } else {
        dispatch.await.unwrap();
    }
    let held = backend.try_acquire().unwrap().is_none();
    let _ = release.send(());
    worker.finish().await;
    assert!(held, "backend must remain held while native work is alive");
}

#[tokio::test]
async fn untraced_spawn_blocking_keeps_lease() {
    check_route(Route::Blocking, false).await;
}
#[tokio::test]
async fn traced_spawn_action_blocking_keeps_lease() {
    check_route(Route::Traced, false).await;
}
#[tokio::test]
async fn detached_async_inherits_hold() {
    check_route(Route::Async, false).await;
}
#[tokio::test]
async fn native_thread_inherits_hold() {
    check_route(Route::Thread, false).await;
}
#[tokio::test]
async fn nested_child_outlives_parent() {
    check_route(Route::Nested, false).await;
}
#[tokio::test]
async fn cancelled_dispatch_stays_locked_past_grace() {
    check_route(Route::Blocking, true).await;
}

#[tokio::test]
async fn last_clone_releases_lease() {
    let (registry, backend, _, release, ready) = registry(Route::Multiple, false);
    let dispatch = tokio::spawn(invoke(registry));
    let Worker::Multiple(mut workers) = ready.await.unwrap().prepare().await else {
        panic!("wrong worker");
    };
    dispatch.await.unwrap();
    let (release_first, first) = workers.remove(0);
    release_first.send(()).unwrap();
    first.join().unwrap();
    let held = backend.try_acquire().unwrap().is_none();
    let (release_last, last) = workers.remove(0);
    release_last.send(()).unwrap();
    last.join().unwrap();
    let available = backend.try_acquire().unwrap().is_some();
    drop(release);
    assert!(held, "last worker must keep lease alive");
    assert!(available, "last worker must release lease");
}

#[tokio::test]
async fn worker_panic_runs_cleanup_before_unlock() {
    let (registry, backend, _, release, ready) = registry(Route::Panic, false);
    let dispatch = tokio::spawn(invoke(registry));
    let Worker::Panic(worker, cleanup) = ready.await.unwrap().prepare().await else {
        panic!("wrong worker");
    };
    dispatch.await.unwrap();
    release.send(()).unwrap();
    assert!(worker.await.unwrap_err().is_panic());
    assert!(
        cleanup.await.unwrap(),
        "cleanup must run while lease is held"
    );
    assert!(backend.try_acquire().unwrap().is_some());
}

#[tokio::test]
async fn inherited_scope_restored_after_panic() {
    check_route(Route::Scope, false).await;
}

#[tokio::test]
async fn missing_dispatch_context_creates_no_lease() {
    let backend = Arc::new(TestBackend::default());
    let (release, wait) = oneshot::channel::<()>();
    let worker = action_latency::spawn_blocking(move || {
        assert!(!worker_context::thread_scope_present());
        let _ = wait.blocking_recv();
    });
    assert_eq!(backend.attempts(), 0);
    release.send(()).unwrap();
    worker.await.unwrap();
    assert_eq!(backend.attempts(), 0);
    assert!(backend.try_acquire().unwrap().is_some());
}
