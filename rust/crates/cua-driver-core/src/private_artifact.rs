//! Recording artifact storage. POSIX mode-bit checks do not inspect ACLs;
//! inherited or existing ACL grants can expose or redirect artifacts.

#[cfg(unix)]
mod platform {
    use std::ffi::{CStr, CString, OsStr};
    use std::fs::File;
    use std::io::{self, Write};
    use std::os::fd::{AsRawFd, FromRawFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
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
            || (s.st_mode & 0o022 != 0 && !root_sticky)
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

#[cfg(unix)]
pub(crate) use platform::PrivateDir;
