//! Lifetime containment for runtimes owned by an embedding host. Standalone
//! daemons deliberately survive their launcher and must never enter this path.

pub struct StartupParent {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pid: libc::pid_t,
}

impl StartupParent {
    pub fn capture() -> Self {
        Self {
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            pid: unsafe { libc::getppid() },
        }
    }

    pub fn install(self, command: &crate::cli::Command) -> anyhow::Result<()> {
        if !cua_driver_core::embedded_mode()
            || !matches!(
                command,
                crate::cli::Command::Serve { .. }
                    | crate::cli::Command::Mcp {
                        direct: true,
                        socket: None,
                        ..
                    }
            )
        {
            return Ok(());
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        unix::install(self.pid)?;
        Ok(())
    }

    /// The SDK's `__private-worker` is host-owned although it runs without
    /// `--embedded`: give it the same private group and parent watch.
    /// Windows hosts contain it with a kill-on-close Job Object instead.
    pub fn install_private_worker(self) -> anyhow::Result<()> {
        cua_driver_core::mark_host_owned_runtime();
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        unix::install_with(self.pid, unix::ParentWatch::Process)?;
        Ok(())
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod unix {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::sync::OnceLock;

    // Only this process holds the writer: CLOEXEC prevents spawned helpers
    // from extending its lifetime. The sentinel reserves our process-group id
    // until cleanup, so a delayed EOF cannot signal a recycled group.
    static LIFETIME_GROUP: OnceLock<cua_driver_core::goat_process_group::ProcessGroupLifetime> =
        OnceLock::new();

    /// What ends a host-owned runtime on Linux. macOS always watches the
    /// parent process with kqueue.
    #[derive(Clone, Copy)]
    pub(super) enum ParentWatch {
        /// `PR_SET_PDEATHSIG`, which fires when the thread that spawned the
        /// driver exits. Embedded hosts keep that thread for the driver's life.
        #[cfg_attr(target_os = "macos", allow(dead_code))]
        CreatingThread,
        /// The parent process itself (a pidfd). The SDK spawns its private
        /// worker on whichever thread constructs the driver, which may exit
        /// while the host and the driver live on.
        Process,
    }

    pub(super) fn install(parent: libc::pid_t) -> anyhow::Result<()> {
        install_with(parent, ParentWatch::CreatingThread)
    }

    pub(super) fn install_with(parent: libc::pid_t, watch: ParentWatch) -> anyhow::Result<()> {
        let pid = unsafe { libc::getpid() };
        if unsafe { libc::setpgid(0, 0) } != 0 && unsafe { libc::getpgrp() } != pid {
            return Err(io::Error::last_os_error().into());
        }
        LIFETIME_GROUP
            .set(cua_driver_core::goat_process_group::ProcessGroupLifetime::new(pid)?)
            .map_err(|_| io::Error::other("embedded lifetime already installed"))?;
        if parent <= 1 || unsafe { libc::getppid() } != parent {
            terminate_group();
        }
        #[cfg(target_os = "linux")]
        if matches!(watch, ParentWatch::Process) {
            match watch_parent_process(parent) {
                Ok(()) => return Ok(()),
                // Kernels before 5.3 have no pidfd; fall back to the thread signal.
                Err(error) if error.raw_os_error() == Some(libc::ENOSYS) => {}
                Err(error) => return Err(error.into()),
            }
        }
        #[cfg(target_os = "macos")]
        let _ = watch;
        watch_parent(parent)?;
        Ok(())
    }

    fn terminate_group() -> ! {
        unsafe {
            libc::kill(-libc::getpid(), libc::SIGKILL);
            libc::_exit(1);
        }
    }

    #[cfg(target_os = "linux")]
    fn watch_parent_process(parent: libc::pid_t) -> io::Result<()> {
        // pidfd_open returns a close-on-exec descriptor that becomes readable
        // once the process exits.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, parent, 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let pidfd = unsafe { OwnedFd::from_raw_fd(raw as libc::c_int) };
        // The parent may have exited, and its pid been reused, before the open.
        if unsafe { libc::getppid() } != parent {
            terminate_group();
        }
        std::thread::Builder::new()
            .name("goat-parent-exit".into())
            .spawn(move || loop {
                let mut poll = libc::pollfd {
                    fd: pidfd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let result = unsafe { libc::poll(&mut poll, 1, -1) };
                if result < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                // Parent exit, or a failed watch: both are terminal, as on macOS.
                terminate_group();
            })?;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn watch_parent(parent: libc::pid_t) -> io::Result<()> {
        // Hosts can inherit blocked/ignored SIGTERM. PDEATHSIG must remain
        // terminal in these explicitly host-owned modes.
        unsafe {
            let mut action = std::mem::zeroed::<libc::sigaction>();
            action.sa_sigaction = libc::SIG_DFL;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut signals = std::mem::zeroed::<libc::sigset_t>();
            libc::sigemptyset(&mut signals);
            libc::sigaddset(&mut signals, libc::SIGTERM);
            let error = libc::pthread_sigmask(libc::SIG_UNBLOCK, &signals, std::ptr::null_mut());
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error));
            }
        }
        if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // Death before prctl does not generate a signal retroactively.
        if unsafe { libc::getppid() } != parent {
            terminate_group();
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn watch_parent(parent: libc::pid_t) -> io::Result<()> {
        let raw = unsafe { libc::kqueue() };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let queue = unsafe { OwnedFd::from_raw_fd(raw) };
        if unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let change = libc::kevent {
            ident: parent as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        if unsafe { libc::kevent(raw, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) } < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                terminate_group();
            }
            return Err(error);
        }
        if unsafe { libc::getppid() } != parent {
            terminate_group();
        }
        std::thread::Builder::new()
            .name("goat-parent-exit".into())
            .spawn(move || {
                let mut event = std::mem::MaybeUninit::<libc::kevent>::uninit();
                loop {
                    let result = unsafe {
                        libc::kevent(
                            queue.as_raw_fd(),
                            std::ptr::null(),
                            0,
                            event.as_mut_ptr(),
                            1,
                            std::ptr::null(),
                        )
                    };
                    if result < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
                    {
                        continue;
                    }
                    // A failed watch is also terminal: continuing would leave
                    // a privileged runtime without its host-lifetime guarantee.
                    terminate_group();
                }
            })?;
        Ok(())
    }
}
