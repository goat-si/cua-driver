//! Recording artifact storage. POSIX mode-bit checks do not inspect ACLs;
//! inherited or existing ACL grants can expose or redirect artifacts.

#[cfg(unix)]
mod platform {
    use std::collections::VecDeque;
    use std::ffi::{CStr, CString, OsStr};
    use std::fs::File;
    use std::io::{self, Write};
    use std::os::fd::{AsRawFd, FromRawFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Component, Path, PathBuf};

    pub(crate) struct PrivateDir {
        file: File,
        path: PathBuf,
        dev: libc::dev_t,
        ino: libc::ino_t,
    }

    fn name(s: &OsStr) -> io::Result<CString> {
        if s.as_bytes().is_empty() || s.as_bytes().contains(&b'/') || s == "." || s == ".." {
            return Err(fail("invalid artifact name"));
        }
        CString::new(s.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))
    }

    fn fail(message: &str) -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, message)
    }

    fn stat_fd(fd: RawFd) -> io::Result<libc::stat> {
        let mut stat = std::mem::MaybeUninit::uninit();
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { stat.assume_init() })
    }

    fn stat_at(fd: RawFd, name: &CStr) -> io::Result<libc::stat> {
        let mut stat = std::mem::MaybeUninit::uninit();
        if unsafe {
            libc::fstatat(
                fd,
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { stat.assume_init() })
    }

    fn validate_dir(s: &libc::stat, destination: bool, private: bool) -> io::Result<()> {
        let uid = unsafe { libc::geteuid() };
        let root_sticky = s.st_uid == 0 && s.st_mode & libc::S_ISVTX != 0;
        if s.st_mode & libc::S_IFMT != libc::S_IFDIR
            || (s.st_uid != uid && s.st_uid != 0)
            || (s.st_mode & 0o022 != 0 && (destination || !root_sticky))
            || (destination && s.st_uid != uid)
            || (private && s.st_mode & 0o077 != 0)
        {
            return Err(fail("unsafe recording directory"));
        }
        Ok(())
    }

    fn open_at(fd: RawFd, name: &CStr, flags: libc::c_int, mode: libc::mode_t) -> io::Result<File> {
        let result = unsafe {
            libc::openat(
                fd,
                name.as_ptr(),
                flags | libc::O_CLOEXEC,
                mode as libc::c_uint,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(result) })
    }

    fn resolve_ancestors(path: &Path) -> io::Result<PathBuf> {
        let mut pending: VecDeque<_> = path
            .components()
            .map(|c| c.as_os_str().to_owned())
            .collect();
        let mut resolved = PathBuf::from("/");
        let mut links = 0;
        while let Some(part) = pending.pop_front() {
            if part == "/" || part == "." {
                continue;
            }
            if part == ".." {
                resolved.pop();
                continue;
            }
            resolved.push(&part);
            let metadata = std::fs::symlink_metadata(&resolved)?;
            if metadata.file_type().is_symlink() {
                links += 1;
                if links > 40
                    || (metadata.uid() != 0 && metadata.uid() != unsafe { libc::geteuid() })
                {
                    return Err(fail("unsafe recording symlink"));
                }
                let target = std::fs::read_link(&resolved)?;
                resolved.pop();
                if target.is_absolute() {
                    resolved = PathBuf::from("/");
                }
                for component in target.components().rev() {
                    pending.push_front(component.as_os_str().to_owned());
                }
            }
        }
        Ok(resolved)
    }

    impl PrivateDir {
        pub(crate) fn open(path: &Path) -> io::Result<Self> {
            Self::open_inner(path, true)
        }

        fn open_inner(path: &Path, create: bool) -> io::Result<Self> {
            let absolute = if path.is_absolute() {
                path.to_owned()
            } else {
                std::env::current_dir()?.join(path)
            };
            if absolute
                .components()
                .any(|c| matches!(c, Component::ParentDir))
            {
                return Err(fail("unsafe path component"));
            }
            match std::fs::symlink_metadata(&absolute) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(fail("unsafe recording symlink"))
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound && create => {}
                Err(e) => return Err(e),
            }
            let absolute = match (absolute.parent(), absolute.file_name()) {
                (Some(parent), Some(name)) => resolve_ancestors(parent)?.join(name),
                _ => absolute,
            };
            let components = absolute
                .components()
                .filter_map(|c| match c {
                    Component::RootDir | Component::CurDir => None,
                    Component::Normal(n) => Some(Ok(n)),
                    _ => Some(Err(fail("unsafe path component"))),
                })
                .collect::<io::Result<Vec<_>>>()?;
            let root = CString::new("/").unwrap();
            let fd = unsafe {
                libc::open(
                    root.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut file = unsafe { File::from_raw_fd(fd) };
            validate_dir(&stat_fd(file.as_raw_fd())?, components.is_empty(), false)?;
            for (index, component) in components.iter().enumerate() {
                let component = name(component)?;
                let last = index + 1 == components.len();
                let stat = match stat_at(file.as_raw_fd(), &component) {
                    Ok(stat) => stat,
                    Err(e) if e.kind() == io::ErrorKind::NotFound && last && create => {
                        if unsafe { libc::mkdirat(file.as_raw_fd(), component.as_ptr(), 0o700) }
                            != 0
                        {
                            return Err(io::Error::last_os_error());
                        }
                        stat_at(file.as_raw_fd(), &component)?
                    }
                    Err(e) => return Err(e),
                };
                validate_dir(&stat, last, false)?;
                let next = open_at(
                    file.as_raw_fd(),
                    &component,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                    0,
                )?;
                let actual = stat_fd(next.as_raw_fd())?;
                validate_dir(&actual, last, false)?;
                if (stat.st_dev, stat.st_ino) != (actual.st_dev, actual.st_ino) {
                    return Err(fail("directory changed while opening"));
                }
                file = next;
            }
            let stat = stat_fd(file.as_raw_fd())?;
            Ok(Self {
                file,
                path: absolute,
                dev: stat.st_dev,
                ino: stat.st_ino,
            })
        }

        pub(crate) fn subdir(&self, component: &OsStr) -> io::Result<Self> {
            let component_c = name(component)?;
            if unsafe { libc::mkdirat(self.file.as_raw_fd(), component_c.as_ptr(), 0o700) } != 0 {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::AlreadyExists {
                    return Err(e);
                }
            }
            let stat = stat_at(self.file.as_raw_fd(), &component_c)?;
            validate_dir(&stat, true, true)?;
            let file = open_at(
                self.file.as_raw_fd(),
                &component_c,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                0,
            )?;
            let actual = stat_fd(file.as_raw_fd())?;
            validate_dir(&actual, true, true)?;
            if (stat.st_dev, stat.st_ino) != (actual.st_dev, actual.st_ino) {
                return Err(fail("directory changed while opening"));
            }
            Ok(Self {
                file,
                path: self.path.join(component),
                dev: actual.st_dev,
                ino: actual.st_ino,
            })
        }

        fn validate_target(&self, target: &CStr) -> io::Result<()> {
            match stat_at(self.file.as_raw_fd(), target) {
                Ok(stat)
                    if stat.st_mode & libc::S_IFMT == libc::S_IFREG
                        && stat.st_uid == unsafe { libc::geteuid() } =>
                {
                    Ok(())
                }
                Ok(_) => Err(fail("unsafe recording artifact")),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            }
        }

        pub(crate) fn create(&self, target: &OsStr, bytes: &[u8]) -> io::Result<File> {
            let target = name(target)?;
            self.validate_target(&target)?;
            let mut random = [0u8; 16];
            getrandom::fill(&mut random).map_err(io::Error::other)?;
            let tmp =
                CString::new(format!(".cua-{:032x}.tmp", u128::from_ne_bytes(random))).unwrap();
            let mut file = open_at(
                self.file.as_raw_fd(),
                &tmp,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
                0o600,
            )?;
            let result = (|| {
                file.write_all(bytes)?;
                self.validate_target(&target)?;
                if unsafe {
                    libc::renameat(
                        self.file.as_raw_fd(),
                        tmp.as_ptr(),
                        self.file.as_raw_fd(),
                        target.as_ptr(),
                    )
                } != 0
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            })();
            if result.is_err() {
                unsafe {
                    libc::unlinkat(self.file.as_raw_fd(), tmp.as_ptr(), 0);
                }
            }
            result?;
            Ok(file)
        }

        pub(crate) fn verify(&self) -> io::Result<()> {
            let fresh = Self::open_inner(&self.path, false)?;
            if (fresh.dev, fresh.ino) != (self.dev, self.ino) {
                return Err(fail("recording directory changed"));
            }
            Ok(())
        }
    }
}

#[cfg(windows)]
mod windows_platform {
    use std::ffi::OsStr;
    use std::fs::{File, OpenOptions};
    use std::io::{self, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::path::{Component, Path, PathBuf};
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING};

    const OPEN_REPARSE: u32 = 0x0020_0000;
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const REPARSE_ATTRIBUTE: u32 = 0x400;

    pub(crate) struct PrivateDir {
        path: PathBuf,
        _handle: File,
    }

    fn open_checked(path: &Path) -> io::Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(OPEN_REPARSE | BACKUP_SEMANTICS)
            .open(path)?;
        let meta = file.metadata()?;
        if meta.file_attributes() & REPARSE_ATTRIBUTE != 0 || !meta.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe recording directory",
            ));
        }
        Ok(file)
    }

    fn component(name: &OsStr) -> io::Result<()> {
        let mut parts = Path::new(name).components();
        if !matches!(parts.next(), Some(Component::Normal(_))) || parts.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid artifact name",
            ));
        }
        Ok(())
    }

    impl PrivateDir {
        pub(crate) fn open(path: &Path) -> io::Result<Self> {
            Self::open_inner(path, true)
        }

        fn open_inner(path: &Path, create: bool) -> io::Result<Self> {
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::current_dir()?.join(path)
            };
            let mut current = PathBuf::new();
            let parts: Vec<_> = path.components().collect();
            for (index, part) in parts.iter().enumerate() {
                if matches!(part, Component::ParentDir) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "unsafe path component",
                    ));
                }
                current.push(part.as_os_str());
                if matches!(part, Component::Prefix(_)) {
                    continue;
                }
                if create && index + 1 == parts.len() && !current.exists() {
                    std::fs::create_dir(&current)?;
                }
                open_checked(&current)?;
            }
            let handle = open_checked(&path)?;
            Ok(Self {
                path,
                _handle: handle,
            })
        }

        pub(crate) fn subdir(&self, name: &OsStr) -> io::Result<Self> {
            component(name)?;
            let path = self.path.join(name);
            match std::fs::create_dir(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
            let handle = open_checked(&path)?;
            Ok(Self {
                path,
                _handle: handle,
            })
        }

        pub(crate) fn create(&self, name: &OsStr, bytes: &[u8]) -> io::Result<File> {
            component(name)?;
            let target = self.path.join(name);
            match std::fs::symlink_metadata(&target) {
                Ok(meta) if meta.file_attributes() & REPARSE_ATTRIBUTE == 0 && meta.is_file() => {}
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "unsafe recording artifact",
                    ))
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            let mut random = [0u8; 16];
            getrandom::fill(&mut random).map_err(io::Error::other)?;
            let tmp = self
                .path
                .join(format!(".cua-{:032x}.tmp", u128::from_ne_bytes(random)));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .custom_flags(OPEN_REPARSE)
                .open(&tmp)?;
            let result: io::Result<()> = (|| {
                file.write_all(bytes)?;
                let a: Vec<u16> = tmp.as_os_str().encode_wide().chain(Some(0)).collect();
                let b: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
                unsafe {
                    MoveFileExW(
                        PCWSTR(a.as_ptr()),
                        PCWSTR(b.as_ptr()),
                        MOVEFILE_REPLACE_EXISTING,
                    )
                }
                .map_err(io::Error::other)?;
                Ok(())
            })();
            if result.is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
            result?;
            Ok(file)
        }

        pub(crate) fn verify(&self) -> io::Result<()> {
            Self::open_inner(&self.path, false).map(|_| ())
        }
    }
}

#[cfg(windows)]
pub(crate) use windows_platform::PrivateDir;

#[cfg(unix)]
pub(crate) use platform::PrivateDir;

#[cfg(all(test, unix))]
mod tests {
    use super::PrivateDir;
    use std::ffi::OsStr;
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

    #[test]
    fn owned_symlink_ancestor_is_allowed_but_destination_link_is_not() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = root.path().join("link");
        symlink(&real, &link).unwrap();
        assert!(PrivateDir::open(&link).is_err());
        PrivateDir::open(&link.join("recording")).unwrap();
        let indirect = root.path().join("indirect");
        symlink("link", &indirect).unwrap();
        PrivateDir::open(&indirect.join("second-recording")).unwrap();
    }

    #[test]
    fn group_writable_ancestor_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let ancestor = root.path().join("ancestor");
        std::fs::create_dir(&ancestor).unwrap();
        std::fs::set_permissions(&ancestor, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(PrivateDir::open(&ancestor.join("output")).is_err());
        assert!(!ancestor.join("output").exists());
    }

    #[test]
    fn held_dirs_refuse_links_and_write_privately_without_path_reopen() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("output");
        let dir = PrivateDir::open(&output).unwrap();
        let target = root.path().join("target");
        std::fs::write(&target, b"unchanged").unwrap();
        symlink(root.path(), output.join("turn-link")).unwrap();
        assert!(dir.subdir(OsStr::new("turn-link")).is_err());
        let turn = dir.subdir(OsStr::new("turn-00001")).unwrap();
        assert_eq!(
            std::fs::metadata(output.join("turn-00001")).unwrap().mode() & 0o777,
            0o700
        );
        symlink(&target, output.join("turn-00001/before.png")).unwrap();
        assert!(turn.create(OsStr::new("before.png"), b"secret").is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"unchanged");
        let moved = root.path().join("moved");
        std::fs::rename(&output, &moved).unwrap();
        turn.create(OsStr::new("action.json"), b"{} ").unwrap();
        assert_eq!(
            std::fs::metadata(moved.join("turn-00001/action.json"))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
        assert!(dir.verify().is_err());
        assert!(!std::fs::read_dir(moved.join("turn-00001"))
            .unwrap()
            .any(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".cua-")));
    }
}
