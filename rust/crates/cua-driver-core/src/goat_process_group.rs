//! Parent-owned Unix process groups, including descendants that ignore SIGTERM.
//! The caller must already have created the group and keep its leader unreaped
//! until this guard is installed. User applications do not belong in these groups.

use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

pub struct ProcessGroupLifetime {
    // Closing the sole writer (also on owner SIGKILL) tells the sentinel to
    // kill the group. CLOEXEC keeps executed helpers from retaining it.
    _writer: OwnedFd,
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    #[cfg(target_os = "linux")]
    let result = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    #[cfg(target_os = "macos")]
    let result = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    #[cfg(target_os = "macos")]
    for fd in [&read, &write] {
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((read, write))
}

/// Gate a single owned-child spawn before exec. `Command::spawn` waits for
/// exec, so an owner thread installs the sentinel while the child is gated.
/// Dropping this pending launch closes the child endpoints and cancels it.
pub struct PreparedProcessGroup {
    _pid_writer: OwnedFd,
    _release_reader: OwnedFd,
    result: std::sync::mpsc::Receiver<io::Result<ProcessGroupLifetime>>,
}

impl PreparedProcessGroup {
    /// The command is for one spawn only and must not outlive this pending
    /// launch. The platform adapter may still set credentials before exec.
    pub fn prepare(command: &mut std::process::Command) -> io::Result<Self> {
        use std::io::Write;
        use std::os::unix::process::CommandExt;

        let (pid_read, pid_write) = pipe()?;
        let (release_read, release_write) = pipe()?;
        let child_writer = pid_write.as_raw_fd();
        let child_reader = release_read.as_raw_fd();
        let owner_reader = pid_read.as_raw_fd();
        let owner_writer = release_write.as_raw_fd();
        let owner = unsafe { libc::getpid() };
        #[cfg(target_os = "macos")]
        let max_fd = unsafe { libc::getdtablesize() };
        unsafe {
            command.pre_exec(move || {
                // Darwin has no pipe2. A pre_exec fork must also cover
                // descriptors another thread
                // created just before fork and has not marked CLOEXEC yet.
                #[cfg(target_os = "macos")]
                for fd in 3..max_fd {
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                }
                // The fork child must close its inherited copy of the owner
                // writer or owner SIGKILL would never produce gate EOF.
                libc::close(owner_reader);
                libc::close(owner_writer);
                let pid = libc::getpid();
                // Another fork child can temporarily hold a gate writer even
                // with CLOEXEC. Independently observe owner death while gated.
                #[cfg(target_os = "linux")]
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) < 0
                    || libc::getppid() != owner
                {
                    return Err(io::Error::from_raw_os_error(libc::ECANCELED));
                }
                #[cfg(target_os = "macos")]
                let queue = arm_owner_watch(owner, child_reader, pid);
                if libc::write(
                    child_writer,
                    (&pid as *const libc::pid_t).cast(),
                    std::mem::size_of_val(&pid),
                ) != std::mem::size_of_val(&pid) as isize
                {
                    return Err(io::Error::last_os_error());
                }
                let mut byte = 0u8;
                #[cfg(target_os = "macos")]
                {
                    let owner_gone = wait_for_owner(queue);
                    libc::close(queue);
                    if owner_gone {
                        return Err(io::Error::from_raw_os_error(libc::ECANCELED));
                    }
                }
                loop {
                    let count = libc::read(child_reader, (&mut byte as *mut u8).cast(), 1);
                    if count < 0 && *errno() == libc::EINTR {
                        continue;
                    }
                    if count != 1 || byte != 1 {
                        return Err(io::Error::from_raw_os_error(libc::ECANCELED));
                    }
                    // The installed group sentinel now owns lifetime. Clearing
                    // the temporary signal lets the spawning thread retire
                    // after exec without killing a healthy browser.
                    #[cfg(target_os = "linux")]
                    if libc::prctl(libc::PR_SET_PDEATHSIG, 0, 0, 0, 0) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    return Ok(());
                }
            });
        }
        let (tx, result) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("goat-group-spawn".into())
            .spawn(move || {
                let result = (|| {
                    let mut pid = [0u8; std::mem::size_of::<libc::pid_t>()];
                    std::fs::File::from(pid_read).read_exact(&mut pid)?;
                    let group = libc::pid_t::from_ne_bytes(pid);
                    #[cfg(test)]
                    if std::env::var_os("GOAT_GROUP_PAUSE_BEFORE_SENTINEL").is_some() {
                        println!("OWNED={group}");
                        std::io::stdin().read_exact(&mut [0u8])?;
                    }
                    let lifetime = ProcessGroupLifetime::new(group)?;
                    std::fs::File::from(release_write).write_all(&[1])?;
                    Ok(lifetime)
                })();
                let _ = tx.send(result);
            })?;
        Ok(Self {
            _pid_writer: pid_write,
            _release_reader: release_read,
            result,
        })
    }

    pub fn finish(self) -> io::Result<ProcessGroupLifetime> {
        self.result
            .recv()
            .map_err(|_| io::Error::other("owned group supervisor exited"))?
    }
}

impl ProcessGroupLifetime {
    pub fn new(group: libc::pid_t) -> io::Result<Self> {
        if group <= 1 {
            return Err(io::Error::other("invalid owned process group"));
        }
        let (read, write) = pipe()?;
        let (ready_read, ready_write) = pipe()?;
        let retained = read.as_raw_fd();
        let ready = ready_write.as_raw_fd();
        let max_fd = unsafe { libc::getdtablesize() };
        if max_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        #[cfg(target_os = "macos")]
        let owner = unsafe { libc::getpid() };
        let sentinel = unsafe { libc::fork() };
        if sentinel < 0 {
            return Err(io::Error::last_os_error());
        }
        if sentinel == 0 {
            // After a multithreaded fork only async-signal-safe syscalls are
            // allowed: no allocation, logging, destructors or platform APIs.
            unsafe {
                if libc::setpgid(0, group) < 0 {
                    libc::_exit(1);
                }
                for fd in 0..max_fd {
                    if fd != retained && fd != ready {
                        libc::close(fd);
                    }
                }
                #[cfg(target_os = "macos")]
                let queue = arm_owner_watch(owner, retained, group);
                let mut byte = 1u8;
                if libc::write(ready, (&byte as *const u8).cast(), 1) != 1 {
                    libc::kill(-group, libc::SIGKILL);
                    libc::_exit(1);
                }
                libc::close(ready);
                #[cfg(target_os = "macos")]
                wait_for_owner(queue);
                #[cfg(target_os = "linux")]
                loop {
                    let count = libc::read(retained, (&mut byte as *mut u8).cast(), 1);
                    if count == 0 || (count < 0 && *errno() != libc::EINTR) {
                        break;
                    }
                }
                libc::kill(-group, libc::SIGKILL);
                libc::_exit(1);
            }
        }
        drop(read);
        drop(ready_write);
        let mut ready_file = std::fs::File::from(ready_read);
        if let Err(error) = ready_file.read_exact(&mut [0u8]) {
            unsafe {
                libc::kill(-group, libc::SIGKILL);
                while libc::waitpid(sentinel, std::ptr::null_mut(), 0) < 0
                    && *errno() == libc::EINTR
                {}
            }
            return Err(error);
        }
        // Observe unexpected sentinel death too. WNOWAIT keeps its pid/group
        // reserved until the group has been signalled, preventing pid reuse.
        std::thread::Builder::new()
            .name("goat-group-exit".into())
            .spawn(move || unsafe {
                let mut info = std::mem::zeroed::<libc::siginfo_t>();
                loop {
                    let result = libc::waitid(
                        libc::P_PID,
                        sentinel as libc::id_t,
                        &mut info,
                        libc::WEXITED | libc::WNOWAIT,
                    );
                    if result < 0 && *errno() == libc::EINTR {
                        continue;
                    }
                    // On wait failure the sentinel may still be alive, so
                    // fail closed rather than silently lose containment.
                    libc::kill(-group, libc::SIGKILL);
                    while libc::waitpid(sentinel, std::ptr::null_mut(), 0) < 0
                        && *errno() == libc::EINTR
                    {}
                    break;
                }
            })?;
        Ok(Self { _writer: write })
    }
}

// Darwin cannot create pipes with CLOEXEC atomically. Watching the owner
// directly also makes an accidentally inherited writer unable to delay death.
#[cfg(target_os = "macos")]
unsafe fn arm_owner_watch(
    owner: libc::pid_t,
    reader: libc::c_int,
    group: libc::pid_t,
) -> libc::c_int {
    let queue = libc::kqueue();
    let change = |ident, filter, fflags| libc::kevent {
        ident,
        filter,
        flags: libc::EV_ADD | libc::EV_ONESHOT,
        fflags,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    let changes = [
        change(owner as libc::uintptr_t, libc::EVFILT_PROC, libc::NOTE_EXIT),
        change(reader as libc::uintptr_t, libc::EVFILT_READ, 0),
    ];
    if queue < 0
        || libc::kevent(
            queue,
            changes.as_ptr(),
            2,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        ) < 0
        || libc::getppid() != owner
    {
        libc::kill(-group, libc::SIGKILL);
        libc::_exit(1);
    }
    queue
}

#[cfg(target_os = "macos")]
unsafe fn wait_for_owner(queue: libc::c_int) -> bool {
    let mut event = std::mem::MaybeUninit::<libc::kevent>::uninit();
    loop {
        let result = libc::kevent(
            queue,
            std::ptr::null(),
            0,
            event.as_mut_ptr(),
            1,
            std::ptr::null(),
        );
        if result < 0 && *errno() == libc::EINTR {
            continue;
        }
        return result != 1 || event.assume_init().filter == libc::EVFILT_PROC;
    }
}

#[cfg(target_os = "macos")]
unsafe fn errno() -> *mut libc::c_int {
    libc::__error()
}

#[cfg(target_os = "linux")]
unsafe fn errno() -> *mut libc::c_int {
    libc::__errno_location()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

    #[test]
    fn owned_group_fixture() {
        if std::env::var_os("GOAT_GROUP_FIXTURE").is_none() {
            return;
        }
        // The controller retains stdin's writer, so owner death cannot make
        // cat exit through EOF. Only the group sentinel can release stdout.
        let mut command = Command::new("sh");
        command
            .args(["-c", "exec cat"])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::null())
            .process_group(0);
        let pending = PreparedProcessGroup::prepare(&mut command).unwrap();
        let mut child = command.spawn().unwrap();
        let lifetime = pending.finish().unwrap();
        println!("OWNED={}", child.id());
        if std::env::var_os("GOAT_GROUP_DROP").is_some() {
            drop(lifetime);
            assert!(!child.wait().unwrap().success());
        } else {
            std::io::Read::read_exact(&mut std::io::stdin(), &mut [0u8]).unwrap();
        }
    }

    async fn check_group(owner_dies: bool, before_sentinel: bool) {
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "goat_process_group::tests::owned_group_fixture",
                "--nocapture",
            ])
            .env("GOAT_GROUP_FIXTURE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        if !owner_dies {
            command.env("GOAT_GROUP_DROP", "1");
        }
        if before_sentinel {
            command.env("GOAT_GROUP_PAUSE_BEFORE_SENTINEL", "1");
        }
        let mut owner = command.spawn().unwrap();
        let _keep_stdin_open = owner.stdin.take().unwrap();
        let mut output = BufReader::new(owner.stdout.take().unwrap());
        let group = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let mut line = String::new();
                assert_ne!(output.read_line(&mut line).await.unwrap(), 0);
                if let Some(pid) = line.trim().strip_prefix("OWNED=") {
                    break pid.parse::<libc::pid_t>().unwrap();
                }
            }
        })
        .await
        .unwrap();
        if owner_dies {
            owner.kill().await.unwrap();
        }
        let result = tokio::time::timeout(Duration::from_secs(15), async {
            output.read_to_end(&mut Vec::new()).await.unwrap();
            owner.wait().await.unwrap()
        })
        .await;
        if result.is_err() {
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
        let status = result.expect("owned helper retained stdout after owner/guard exit");
        assert_eq!(status.success(), !owner_dies);
    }

    #[tokio::test]
    async fn owned_helper_exits_after_owner_sigkill_with_stdin_still_open() {
        check_group(true, false).await;
    }

    #[tokio::test]
    async fn owned_helper_exits_when_its_guard_is_dropped() {
        check_group(false, false).await;
    }

    #[tokio::test]
    async fn owned_child_exits_if_owner_dies_before_sentinel_installation() {
        check_group(true, true).await;
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn detached_concurrent_exec_does_not_inherit_lifetime_pipes() {
        use std::sync::{Arc, Barrier};
        let barrier = Arc::new(Barrier::new(2));
        let other = barrier.clone();
        let launches = std::thread::spawn(move || {
            other.wait();
            let mut observations = Vec::new();
            for _ in 0..32 {
                let output = Command::new("sh")
                    .args(["-c", "ls -l /proc/self/fd"])
                    .process_group(0)
                    .output()
                    .unwrap();
                assert!(output.status.success());
                observations.push(String::from_utf8(output.stdout).unwrap());
            }
            observations
        });
        barrier.wait();
        let mut pipes = Vec::new();
        for _ in 0..32 {
            let pair = pipe().unwrap();
            for fd in [&pair.0, &pair.1] {
                assert_ne!(
                    unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                    0
                );
            }
            pipes.push(pair);
        }
        let observations = launches.join().unwrap();
        for pair in &pipes {
            let path = format!("/proc/self/fd/{}", pair.1.as_raw_fd());
            let target = std::fs::read_link(&path).unwrap();
            for output in &observations {
                assert!(
                    !output.contains(target.to_str().unwrap()),
                    "exec retained {target:?}"
                );
            }
        }
    }
}
