//! Cross-process exclusion for desktop input.

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
        Self {
            directory: canonical_directory(),
        }
    }
}

impl FileLeaseBackend {
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
    Ok(PathBuf::from(path.map_err(io::Error::other)?).join("cua-driver"))
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
            Err(error) if error.kind() == fs2::lock_contended_error().kind() => return Ok(None),
            Err(error) => return Err(error),
        }
        validate_locked(&file, &path)?;
        Ok(Some(Box::new(file)))
    }
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
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(unix)]
fn validate_locked(file: &File, path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
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
    refuse_reparse(path, false)?;
    Ok(file)
}

#[cfg(windows)]
fn validate_locked(_file: &File, path: &Path) -> io::Result<()> {
    refuse_reparse(path, false)
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

    #[test]
    fn file_backend_contends_until_release() {
        let root = tempfile::tempdir().unwrap();
        let backend = FileLeaseBackend::at(&root.path().join("lease"));
        let first = backend.try_acquire().unwrap().unwrap();
        assert!(backend.try_acquire().unwrap().is_none());
        drop(first);
        assert!(backend.try_acquire().unwrap().is_some());
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
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

/// Clock for bounded polling; implementations must be monotonic.
#[async_trait::async_trait]
pub trait LeaseClock: Send + Sync {
    fn now(&self) -> Duration;
    async fn sleep(&self, duration: Duration);
}

#[cfg(not(test))]
struct TokioClock(tokio::time::Instant);

#[cfg(not(test))]
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
}

impl Drop for ActiveLease {
    fn drop(&mut self) {
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

#[derive(Debug)]
pub(crate) enum LeaseFailure {
    Busy,
    Unavailable(String),
}

pub(crate) struct LeaseManager {
    backend: Arc<dyn LeaseBackend>,
    clock: Arc<dyn LeaseClock>,
}

impl LeaseManager {
    pub(crate) fn new(backend: Arc<dyn LeaseBackend>, clock: Arc<dyn LeaseClock>) -> Self {
        Self { backend, clock }
    }

    #[cfg(not(test))]
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
        owners()
            .lock()
            .unwrap()
            .active
            .insert(owner.to_owned(), Arc::downgrade(&guard));
        Ok(ActiveLease {
            owner: owner.to_owned(),
            guard,
        })
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    pub(crate) struct TestBackend {
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
        pub(crate) fn hold(&self) {
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
    pub(crate) struct TestClock(Mutex<Duration>);
    #[async_trait::async_trait]
    impl LeaseClock for TestClock {
        fn now(&self) -> Duration {
            *self.0.lock().unwrap()
        }
        async fn sleep(&self, duration: Duration) {
            *self.0.lock().unwrap() += duration;
        }
    }
    pub(crate) fn manager() -> LeaseManager {
        LeaseManager::new(
            Arc::new(TestBackend::default()),
            Arc::new(TestClock::default()),
        )
    }
}
