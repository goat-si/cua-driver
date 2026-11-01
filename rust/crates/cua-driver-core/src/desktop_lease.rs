//! Cross-process exclusion for desktop input.
#![cfg_attr(
    not(feature = "test-support"),
    doc = "Without `test-support`, the deterministic fixtures are unavailable:\n\n```compile_fail\nuse cua_driver_core::desktop_lease::test_support::TestBackend;\n```"
)]

use std::any::Any;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use fs2::FileExt;

/// A guard releases its lease when dropped.
pub type LeaseGuard = Box<dyn Any + Send>;

/// Nonblocking lease acquisition; `None` means another process owns the lease.
pub trait LeaseBackend: Send + Sync {
    fn try_acquire(&self) -> io::Result<Option<LeaseGuard>>;
}

/// The canonical per-user file lock.
pub struct FileLeaseBackend {
    directory: io::Result<PathBuf>,
}

impl Default for FileLeaseBackend {
    fn default() -> Self {
        #[cfg(windows)]
        {
            Self::with_known_folder(canonical_directory)
        }
        #[cfg(not(windows))]
        {
            Self {
                directory: canonical_directory(),
            }
        }
    }
}

impl FileLeaseBackend {
    #[cfg(windows)]
    fn with_known_folder(resolver: impl FnOnce() -> io::Result<PathBuf>) -> Self {
        Self {
            directory: resolver().map(|path| path.join("cua-driver")),
        }
    }

    #[cfg(test)]
    fn at(directory: &Path) -> Self {
        Self {
            directory: Ok(directory.to_owned()),
        }
    }
}

#[cfg(unix)]
fn canonical_directory() -> io::Result<PathBuf> {
    let uid = unsafe { libc::geteuid() };
    Ok(PathBuf::from(format!("/tmp/cua-driver-{uid}")))
}

#[cfg(windows)]
fn canonical_directory() -> io::Result<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath, KF_FLAG_DEFAULT};
    let raw = unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, None) }
        .map_err(io::Error::other)?;
    let path = unsafe { raw.to_string() };
    unsafe { CoTaskMemFree(Some(raw.0.cast())) };
    Ok(PathBuf::from(path.map_err(io::Error::other)?))
}

#[cfg(not(any(unix, windows)))]
fn canonical_directory() -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "desktop lease unsupported",
    ))
}

impl LeaseBackend for FileLeaseBackend {
    fn try_acquire(&self) -> io::Result<Option<LeaseGuard>> {
        let directory = self
            .directory
            .as_ref()
            .map_err(|e| io::Error::new(e.kind(), e.to_string()))?;
        prepare_directory(directory)?;
        let path = directory.join("desktop-input.lock");
        let file = open_lock(&path)?;
        match file.try_lock_exclusive() {
            Ok(()) => {}
            Err(error) if is_lock_contention(&error) => return Ok(None),
            Err(error) => return Err(error),
        }
        validate_locked(&file, &path)?;
        Ok(Some(Box::new(file)))
    }
}

fn is_lock_contention(error: &io::Error) -> bool {
    error.raw_os_error().is_some()
        && error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

#[cfg(unix)]
fn prepare_directory(directory: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    match fs::DirBuilder::new().mode(0o700).create(directory) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let metadata = fs::symlink_metadata(directory)?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe desktop lease directory",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn open_lock(path: &Path) -> io::Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other("missing lease directory"))?;
    let dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory)?;
    let fd_meta = dir.metadata()?;
    let path_meta = fs::symlink_metadata(directory)?;
    if !fd_meta.is_dir()
        || !path_meta.file_type().is_dir()
        || fd_meta.uid() != unsafe { libc::geteuid() }
        || fd_meta.mode() & 0o077 != 0
        || fd_meta.dev() != path_meta.dev()
        || fd_meta.ino() != path_meta.ino()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe desktop lease directory",
        ));
    }
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c"desktop-input.lock".as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn validate_locked(file: &File, path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing lease directory"))?;
    let directory = fs::symlink_metadata(parent)?;
    if !directory.file_type().is_dir()
        || directory.uid() != unsafe { libc::geteuid() }
        || directory.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe desktop lease directory",
        ));
    }
    let fd = file.metadata()?;
    let entry = fs::symlink_metadata(path)?;
    if !entry.file_type().is_file() || fd.dev() != entry.dev() || fd.ino() != entry.ino() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "desktop lease file was replaced",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn prepare_directory(directory: &Path) -> io::Result<()> {
    match fs::create_dir(directory) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    refuse_reparse(directory, true)
}

#[cfg(windows)]
fn refuse_reparse(path: &Path, directory: bool) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    const REPARSE: u32 = 0x400;
    let meta = fs::symlink_metadata(path)?;
    if meta.file_attributes() & REPARSE != 0
        || (directory && !meta.is_dir())
        || (!directory && !meta.is_file())
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe desktop lease path",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn open_lock(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const OPEN_REPARSE: u32 = 0x0020_0000;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .custom_flags(OPEN_REPARSE)
        .open(path)?;
    use std::os::windows::fs::MetadataExt;
    if file.metadata()?.file_attributes() & 0x400 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe desktop lease file",
        ));
    }
    refuse_reparse(path, false)?;
    Ok(file)
}

#[cfg(windows)]
fn validate_locked(file: &File, path: &Path) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    if file.metadata()?.file_attributes() & 0x400 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe desktop lease file",
        ));
    }
    refuse_reparse(
        path.parent()
            .ok_or_else(|| io::Error::other("missing lease directory"))?,
        true,
    )?;
    refuse_reparse(path, false)?;
    use std::os::windows::fs::OpenOptionsExt;
    let current = OpenOptions::new()
        .read(true)
        .custom_flags(0x0020_0000) // FILE_FLAG_OPEN_REPARSE_POINT
        .open(path)?;
    if current.metadata()?.file_attributes() & 0x400 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe desktop lease file",
        ));
    }
    validate_identity(file, &current, file_identity)
}

#[cfg(windows)]
fn file_identity(file: &File) -> io::Result<(u32, u32, u32)> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
        .map_err(io::Error::other)?;
    Ok((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ))
}

#[cfg(windows)]
fn validate_identity(
    locked: &File,
    current: &File,
    read: impl Fn(&File) -> io::Result<(u32, u32, u32)>,
) -> io::Result<()> {
    if read(locked)? != read(current)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "desktop lease file was replaced",
        ));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn prepare_directory(_: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}
#[cfg(not(any(unix, windows)))]
fn open_lock(_: &Path) -> io::Result<File> {
    Err(io::ErrorKind::Unsupported.into())
}
#[cfg(not(any(unix, windows)))]
fn validate_locked(_: &File, _: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct GateClock {
        requested: tokio::sync::Notify,
        release: tokio::sync::Notify,
        sleeps: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LeaseClock for GateClock {
        fn now(&self) -> Duration {
            Duration::ZERO
        }
        async fn sleep(&self, duration: Duration) {
            assert_eq!(duration, CANCELLED_DISPATCH_GRACE);
            self.sleeps.fetch_add(1, Ordering::SeqCst);
            self.requested.notify_one();
            self.release.notified().await;
        }
    }

    async fn cancel_dispatch(
        take_hold: bool,
        owner: &str,
    ) -> (
        Arc<test_support::TestBackend>,
        Arc<GateClock>,
        Option<WorkerHold>,
        &'static tokio::sync::Mutex<()>,
    ) {
        let backend = Arc::new(test_support::TestBackend::default());
        let clock = Arc::new(GateClock::default());
        let manager = LeaseManager::new(backend.clone(), clock.clone());
        let mut lease = manager.acquire(owner).await.unwrap();
        let coordinator: &'static tokio::sync::Mutex<()> =
            Box::leak(Box::new(tokio::sync::Mutex::new(())));
        let guard = coordinator.lock().await;
        lease.activate();
        let mut admission = DispatchAdmission::new(lease, guard, clock.clone());
        let captured = Mutex::new(None);
        let mut invocation = Box::pin(admission.invoke(async {
            if take_hold {
                *captured.lock().unwrap() = worker_hold();
            }
            std::future::pending::<()>().await;
        }));
        assert!(futures_util::poll!(invocation.as_mut()).is_pending());
        drop(invocation);
        drop(admission);
        let hold = captured.into_inner().unwrap();
        (backend, clock, hold, coordinator)
    }

    #[tokio::test]
    async fn cancelled_dispatch_keeps_the_lease_until_its_worker_hold_drops() {
        let (backend, clock, hold, coordinator) = cancel_dispatch(true, "cancel-worker-hold").await;
        let hold = hold.expect("worker hold installed");
        let second = hold.clone();
        assert!(backend.try_acquire().unwrap().is_none());
        assert!(coordinator.try_lock().is_err());
        drop(hold);
        tokio::task::yield_now().await;
        assert!(backend.try_acquire().unwrap().is_none());
        drop(second);
        let guard = coordinator.lock().await;
        assert!(backend.try_acquire().unwrap().is_some());
        assert_eq!(clock.sleeps.load(Ordering::SeqCst), 0);
        drop(guard);
    }

    #[tokio::test]
    async fn cancelled_dispatch_without_worker_hold_releases_after_grace() {
        let (backend, clock, hold, coordinator) = cancel_dispatch(false, "cancel-no-hold").await;
        assert!(hold.is_none());
        clock.requested.notified().await;
        assert!(backend.try_acquire().unwrap().is_none());
        assert!(coordinator.try_lock().is_err());
        clock.release.notify_one();
        let guard = coordinator.lock().await;
        assert!(backend.try_acquire().unwrap().is_some());
        assert_eq!(clock.sleeps.load(Ordering::SeqCst), 1);
        drop(guard);
    }

    #[test]
    fn contention_is_classified_by_raw_os_error() {
        let synthetic = io::Error::from(io::ErrorKind::WouldBlock);
        assert!(!is_lock_contention(&synthetic));
        assert!(is_lock_contention(&fs2::lock_contended_error()));
        assert!(!is_lock_contention(&io::Error::from_raw_os_error(1)));
    }

    #[cfg(windows)]
    #[test]
    fn windows_identity_reader_rejects_replacement() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let file = tempfile::tempfile().unwrap();
        let calls = AtomicUsize::new(0);
        let error = validate_identity(&file, &file, |_| {
            Ok((7, 8, calls.fetch_add(1, Ordering::SeqCst) as u32))
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "desktop lease file was replaced");
        validate_identity(&file, &file, |_| Ok((7, 8, 9))).unwrap();
    }

    #[test]
    fn file_backend_contends_until_release() {
        let root = tempfile::tempdir().unwrap();
        let backend = FileLeaseBackend::at(&root.path().join("lease"));
        let first = backend.try_acquire().unwrap().unwrap();
        assert!(backend.try_acquire().unwrap().is_none());
        drop(first);
        assert!(backend.try_acquire().unwrap().is_some());
    }

    #[cfg(windows)]
    #[test]
    fn windows_known_folder_failure_refuses() {
        let backend = FileLeaseBackend::with_known_folder(|| {
            Err(io::Error::other("known folder unavailable"))
        });
        let error = match backend.try_acquire() {
            Ok(_) => panic!("known folder failure must refuse"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("known folder unavailable"));
    }

    #[cfg(unix)]
    #[test]
    fn file_backend_refuses_symlink_and_replacement() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("lease");
        prepare_directory(&dir).unwrap();
        let path = dir.join("desktop-input.lock");
        let target = root.path().join("target");
        fs::write(&target, b"").unwrap();
        symlink(&target, &path).unwrap();
        assert!(FileLeaseBackend::at(&dir).try_acquire().is_err());
        fs::remove_file(&path).unwrap();
        let file = open_lock(&path).unwrap();
        file.try_lock_exclusive().unwrap();
        fs::rename(&path, dir.join("old.lock")).unwrap();
        File::create(&path).unwrap();
        assert!(validate_locked(&file, &path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn file_backend_refuses_unsafe_directory() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("lease");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(FileLeaseBackend::at(&dir).try_acquire().is_err());
        let link = root.path().join("link");
        symlink(&dir, &link).unwrap();
        assert!(FileLeaseBackend::at(&link).try_acquire().is_err());
    }
}

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

/// Clock for bounded polling; implementations must be monotonic.
#[async_trait::async_trait]
pub trait LeaseClock: Send + Sync {
    fn now(&self) -> Duration;
    async fn sleep(&self, duration: Duration);
}

struct TokioClock(tokio::time::Instant);

#[async_trait::async_trait]
impl LeaseClock for TokioClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await
    }
}

type SharedGuard = Arc<Mutex<LeaseGuard>>;

const CANCELLED_DISPATCH_GRACE: Duration = Duration::from_secs(5);

#[derive(Default)]
struct WorkerTracker {
    taken: AtomicBool,
    count: AtomicUsize,
    changed: tokio::sync::Notify,
}

struct DispatchHold {
    lease: SharedGuard,
    tracker: Arc<WorkerTracker>,
}

tokio::task_local! {
    static DISPATCH_HOLD: DispatchHold;
}

/// Keeps the current desktop lease alive in a non-cancellable worker.
///
/// Call [`worker_hold`] inside `Tool::invoke`, before spawning a worker that
/// can continue injecting input after the invocation future is cancelled.
/// Move the returned hold into the worker closure and keep it until all input
/// has stopped. Spawned tasks do not inherit the task-local dispatch context.
pub struct WorkerHold {
    lease: SharedGuard,
    tracker: Arc<WorkerTracker>,
}

impl Clone for WorkerHold {
    fn clone(&self) -> Self {
        self.tracker.count.fetch_add(1, Ordering::SeqCst);
        Self {
            lease: self.lease.clone(),
            tracker: self.tracker.clone(),
        }
    }
}

impl Drop for WorkerHold {
    fn drop(&mut self) {
        if self.tracker.count.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.tracker.changed.notify_one();
        }
    }
}

/// Acquire a hold for the currently admitted desktop dispatch, if any.
pub fn worker_hold() -> Option<WorkerHold> {
    DISPATCH_HOLD
        .try_with(|dispatch| {
            dispatch.tracker.taken.store(true, Ordering::SeqCst);
            dispatch.tracker.count.fetch_add(1, Ordering::SeqCst);
            WorkerHold {
                lease: dispatch.lease.clone(),
                tracker: dispatch.tracker.clone(),
            }
        })
        .ok()
}

pub(crate) struct DispatchAdmission {
    lease: Option<ActiveLease>,
    coordinator: Option<tokio::sync::MutexGuard<'static, ()>>,
    tracker: Arc<WorkerTracker>,
    clock: Arc<dyn LeaseClock>,
    completed: bool,
}

impl DispatchAdmission {
    pub(crate) fn new(
        lease: ActiveLease,
        coordinator: tokio::sync::MutexGuard<'static, ()>,
        clock: Arc<dyn LeaseClock>,
    ) -> Self {
        Self {
            lease: Some(lease),
            coordinator: Some(coordinator),
            tracker: Arc::new(WorkerTracker::default()),
            clock,
            completed: false,
        }
    }

    pub(crate) async fn invoke<F: std::future::Future>(&mut self, future: F) -> F::Output {
        let hold = DispatchHold {
            lease: self.lease.as_ref().unwrap().guard.clone(),
            tracker: self.tracker.clone(),
        };
        let output = DISPATCH_HOLD.scope(hold, future).await;
        self.completed = true;
        output
    }
}

impl Drop for DispatchAdmission {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let lease = self.lease.take().unwrap();
        let coordinator = self.coordinator.take().unwrap();
        let tracker = self.tracker.clone();
        let clock = self.clock.clone();
        tokio::spawn(async move {
            if tracker.taken.load(Ordering::SeqCst) {
                loop {
                    let notified = tracker.changed.notified();
                    if tracker.count.load(Ordering::SeqCst) == 0 {
                        break;
                    }
                    notified.await;
                }
            } else {
                clock.sleep(CANCELLED_DISPATCH_GRACE).await;
            }
            drop(coordinator);
            drop(lease);
        });
    }
}

#[derive(Default)]
struct Owners {
    active: HashMap<String, Weak<Mutex<LeaseGuard>>>,
    retained: HashMap<String, SharedGuard>,
}

fn owners() -> &'static Mutex<Owners> {
    static OWNERS: OnceLock<Mutex<Owners>> = OnceLock::new();
    OWNERS.get_or_init(|| Mutex::new(Owners::default()))
}

/// Retain the current dispatch's lease while native keys or buttons remain held.
/// The owner must be the trusted runtime session key used at dispatch. The
/// platform must release only after all of that owner's held input is released.
pub fn retain_lease(owner: &str) -> io::Result<()> {
    let mut owners = owners().lock().unwrap();
    let guard = owners
        .active
        .get(owner)
        .and_then(Weak::upgrade)
        .or_else(|| owners.retained.get(owner).cloned())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no active desktop lease for owner")
        })?;
    owners.retained.insert(owner.to_owned(), guard);
    Ok(())
}

/// Release a retained lease after native input cleanup has completed.
pub fn release_lease(owner: &str) {
    owners().lock().unwrap().retained.remove(owner);
}

/// The lease owner for sessionless dispatches, derived from a trusted runtime scope.
pub fn runtime_owner(runtime_scope: &str) -> String {
    format!("__cua_desktop_runtime:{runtime_scope}")
}

/// A lease remains active for the full desktop coordinator guard's lifetime.
pub(crate) struct ActiveLease {
    owner: String,
    guard: SharedGuard,
    active: bool,
}

impl Drop for ActiveLease {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut owners = owners().lock().unwrap();
        if owners
            .active
            .get(&self.owner)
            .and_then(Weak::upgrade)
            .is_some_and(|guard| Arc::ptr_eq(&guard, &self.guard))
        {
            owners.active.remove(&self.owner);
        }
    }
}

impl ActiveLease {
    pub(crate) fn activate(&mut self) {
        owners()
            .lock()
            .unwrap()
            .active
            .insert(self.owner.clone(), Arc::downgrade(&self.guard));
        self.active = true;
    }
}

#[derive(Debug)]
pub enum LeaseFailure {
    Busy,
    Unavailable(String),
}

/// An external interactive dispatch holds this guard until its native batch
/// finishes. Call `retain_lease` before dropping it if input remains held.
pub struct OwnedLease {
    _lease: ActiveLease,
    _coordinator: tokio::sync::MutexGuard<'static, ()>,
}

/// Admit an interactive batch that bypasses ToolRegistry. `owner` must be a
/// trusted runtime session key (or `runtime_owner` for a sessionless runtime).
/// The platform must perform this before delivering any native input.
pub async fn acquire_owned(owner: &str) -> Result<OwnedLease, LeaseFailure> {
    static MANAGER: OnceLock<LeaseManager> = OnceLock::new();
    let mut lease = MANAGER
        .get_or_init(LeaseManager::production)
        .acquire(owner)
        .await?;
    let coordinator = crate::tool::desktop_action_coordinator();
    let coordinator = match coordinator.try_lock() {
        Ok(guard) => guard,
        Err(_) => coordinator.lock().await,
    };
    lease.activate();
    Ok(OwnedLease {
        _lease: lease,
        _coordinator: coordinator,
    })
}

pub struct LeaseManager {
    backend: Arc<dyn LeaseBackend>,
    clock: Arc<dyn LeaseClock>,
}

impl LeaseManager {
    pub(crate) fn clock(&self) -> Arc<dyn LeaseClock> {
        self.clock.clone()
    }
    pub(crate) fn new(backend: Arc<dyn LeaseBackend>, clock: Arc<dyn LeaseClock>) -> Self {
        Self { backend, clock }
    }

    pub(crate) fn production() -> Self {
        Self::new(
            Arc::new(FileLeaseBackend::default()),
            Arc::new(TokioClock(tokio::time::Instant::now())),
        )
    }

    pub(crate) async fn acquire(&self, owner: &str) -> Result<ActiveLease, LeaseFailure> {
        let retained = owners().lock().unwrap().retained.get(owner).cloned();
        let guard = if let Some(guard) = retained {
            guard
        } else {
            let deadline = self.clock.now() + Duration::from_secs(5);
            loop {
                match self.backend.try_acquire() {
                    Ok(Some(guard)) => break Arc::new(Mutex::new(guard)),
                    Ok(None) => {}
                    Err(error) => return Err(LeaseFailure::Unavailable(error.to_string())),
                }
                let now = self.clock.now();
                if now >= deadline {
                    return Err(LeaseFailure::Busy);
                }
                self.clock
                    .sleep(Duration::from_millis(10).min(deadline - now))
                    .await;
            }
        };
        Ok(ActiveLease {
            owner: owner.to_owned(),
            guard,
            active: false,
        })
    }
}

/// Deterministic lease fixtures for platform tests. In the platform crate's
/// `Cargo.toml`, enable `cua-driver-core = { path = "../cua-driver-core",
/// features = ["test-support"] }` under `[dev-dependencies]`.
///
/// ```no_run
/// use std::sync::Arc;
/// use cua_driver_core::{desktop_lease::test_support::{TestBackend, TestClock, is_retained}, tool::ToolRegistry};
/// let backend = Arc::new(TestBackend::default());
/// backend.hold();
/// let mut registry = ToolRegistry::with_test_lease(backend, Arc::new(TestClock::default()));
/// assert!(!is_retained("some-owner"));
/// # async fn assert_busy(registry: &ToolRegistry) {
/// // After registering `invoke_menu`, a held backend refuses before dispatch.
/// let result = registry.invoke("invoke_menu", serde_json::json!({"pid": 42})).await;
/// assert_eq!(result.structured_content.unwrap()["code"], "desktop_busy");
/// # }
/// # let _ = &mut registry;
/// ```
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    pub struct TestBackend {
        held: Arc<AtomicBool>,
        pub(crate) attempts: AtomicUsize,
        pub(crate) unavailable: AtomicBool,
        pub(crate) release_after: AtomicUsize,
    }

    struct Held(Arc<AtomicBool>);
    impl Drop for Held {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }

    impl TestBackend {
        pub fn hold(&self) {
            self.held.store(true, Ordering::SeqCst);
        }
    }
    impl LeaseBackend for TestBackend {
        fn try_acquire(&self) -> io::Result<Option<LeaseGuard>> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if self.unavailable.load(Ordering::SeqCst) {
                return Err(io::Error::other("synthetic failure"));
            }
            let release_after = self.release_after.load(Ordering::SeqCst);
            if release_after != 0 && attempt > release_after {
                self.held.store(false, Ordering::SeqCst);
            }
            if self
                .held
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                Ok(Some(Box::new(Held(self.held.clone()))))
            } else {
                Ok(None)
            }
        }
    }
    #[derive(Default)]
    pub struct TestClock(Mutex<Duration>);
    #[async_trait::async_trait]
    impl LeaseClock for TestClock {
        fn now(&self) -> Duration {
            *self.0.lock().unwrap()
        }
        async fn sleep(&self, duration: Duration) {
            *self.0.lock().unwrap() += duration;
        }
    }
    pub fn manager() -> LeaseManager {
        LeaseManager::new(
            Arc::new(TestBackend::default()),
            Arc::new(TestClock::default()),
        )
    }

    /// Whether `owner` currently holds a retained lease.
    pub fn is_retained(owner: &str) -> bool {
        owners().lock().unwrap().retained.contains_key(owner)
    }
}
