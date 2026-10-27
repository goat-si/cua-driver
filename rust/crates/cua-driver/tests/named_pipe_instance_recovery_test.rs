#![cfg(target_os = "windows")]

//! The Windows daemon must keep serving after a later named-pipe instance
//! fails. The accept loop creates a fresh instance for every client, and it
//! used to return from `run_serve` on the first instance it could not create
//! or connect, taking the whole daemon down.
//!
//! Client connect/disconnect races do not reach that path: tokio maps a
//! client that arrives or leaves before `ConnectNamedPipe` (`ERROR_PIPE_CONNECTED`,
//! `ERROR_NO_DATA`) to a successful connect. This test therefore forces a real
//! instance failure through the pipe's own security descriptor, which Windows
//! keeps for the pipe name rather than per instance:
//!
//! 1. Hold a client connection opened with `WRITE_DAC`. The daemon hands that
//!    instance to a connection task and lists a fresh one.
//! 2. Rewrite the pipe DACL to deny `FILE_CREATE_PIPE_INSTANCE`, then consume
//!    the listening instance. The daemon's next `CreateNamedPipeW` fails with
//!    access denied.
//! 3. Restore the DACL. A daemon that survived lists an instance on its next
//!    retry and answers `get_config`; one that bailed is gone.
//!
//! The file also holds the other named-pipe lifetime regression: a stopping
//! daemon must end calls still running on its accepted connections.

use std::ffi::c_void;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use cua_driver_testkit::{CliDriver, Driver, IsolatedStateRoot};

type Handle = *mut c_void;

const FILE_READ_DATA: u32 = 0x0001;
const FILE_WRITE_DATA: u32 = 0x0002;
const READ_CONTROL: u32 = 0x0002_0000;
const WRITE_DAC: u32 = 0x0004_0000;
const SYNCHRONIZE: u32 = 0x0010_0000;
const OPEN_EXISTING: u32 = 3;
const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;
const SDDL_REVISION_1: u32 = 1;
const ERROR_PIPE_BUSY: i32 = 231;

/// Everyone keeps every pipe right except `FILE_CREATE_PIPE_INSTANCE` (0x4),
/// which the daemon needs to list its next instance. Clients in this test open
/// without `FILE_APPEND_DATA`, which shares that bit.
const DENY_NEW_INSTANCES_SDDL: &str = "D:P(D;;0x4;;;WD)(A;;0x1F01FF;;;WD)";

/// Longer than two daemon retry intervals, so a surviving daemon has tried
/// and failed to replace the consumed instance more than once.
const BLOCKED_WINDOW: Duration = Duration::from_millis(1_500);
const RECOVERY_WINDOW: Duration = Duration::from_secs(10);

#[link(name = "kernel32")]
extern "system" {
    fn CreateFileW(
        file_name: *const u16,
        desired_access: u32,
        share_mode: u32,
        security_attributes: *mut c_void,
        creation_disposition: u32,
        flags_and_attributes: u32,
        template_file: Handle,
    ) -> Handle;
    fn CloseHandle(handle: Handle) -> i32;
    fn WaitNamedPipeW(name: *const u16, timeout_ms: u32) -> i32;
    fn LocalFree(memory: *mut c_void) -> *mut c_void;
}

#[link(name = "advapi32")]
extern "system" {
    fn GetKernelObjectSecurity(
        handle: Handle,
        requested_information: u32,
        security_descriptor: *mut c_void,
        length: u32,
        length_needed: *mut u32,
    ) -> i32;
    fn SetKernelObjectSecurity(
        handle: Handle,
        security_information: u32,
        security_descriptor: *const c_void,
    ) -> i32;
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        string_security_descriptor: *const u16,
        string_sd_revision: u32,
        security_descriptor: *mut *mut c_void,
        security_descriptor_size: *mut u32,
    ) -> i32;
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A client end of the daemon's pipe.
struct PipeClient(Handle);

impl PipeClient {
    /// Connect to a listening instance, waiting while every instance is busy.
    fn open(socket: &str, access: u32) -> std::io::Result<Self> {
        let name = wide(socket);
        let deadline = Instant::now() + RECOVERY_WINDOW;
        loop {
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    access,
                    0,
                    std::ptr::null_mut(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if handle as isize != -1 {
                return Ok(Self(handle));
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_PIPE_BUSY) || Instant::now() >= deadline {
                return Err(error);
            }
            wait_for_listening_instance(socket, Duration::from_millis(250));
        }
    }

    fn dacl(&self) -> std::io::Result<Vec<u64>> {
        let mut needed = 0_u32;
        unsafe {
            GetKernelObjectSecurity(
                self.0,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                0,
                &mut needed,
            )
        };
        if needed == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // u64 storage keeps the self-relative descriptor suitably aligned.
        let mut descriptor = vec![0_u64; (needed as usize).div_ceil(8)];
        let ok = unsafe {
            GetKernelObjectSecurity(
                self.0,
                DACL_SECURITY_INFORMATION,
                descriptor.as_mut_ptr().cast(),
                (descriptor.len() * 8) as u32,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(descriptor)
    }

    fn set_dacl(&self, descriptor: *const c_void) -> std::io::Result<()> {
        if unsafe { SetKernelObjectSecurity(self.0, DACL_SECURITY_INFORMATION, descriptor) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for PipeClient {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// A descriptor parsed from SDDL, freed with `LocalFree`.
struct SddlDescriptor(*mut c_void);

impl SddlDescriptor {
    fn parse(sddl: &str) -> std::io::Result<Self> {
        let sddl = wide(sddl);
        let mut descriptor = std::ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self(descriptor))
    }
}

impl Drop for SddlDescriptor {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

/// Whether the daemon lists an instance a client could connect to within
/// `timeout`. `WaitNamedPipeW` does not connect, so it leaves the instance
/// for the next client.
fn wait_for_listening_instance(socket: &str, timeout: Duration) -> bool {
    let name = wide(socket);
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let slice = remaining.min(Duration::from_millis(250)).as_millis() as u32;
        if unsafe { WaitNamedPipeW(name.as_ptr(), slice.max(1)) } != 0 {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        // No instance at all returns immediately; avoid spinning.
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn daemon_keeps_serving_after_a_later_pipe_instance_fails() {
    let mut driver = CliDriver::with_binary(env!("CARGO_BIN_EXE_cua-driver"));
    assert!(driver.available(), "test daemon failed to start");
    let socket = driver
        .daemon_socket()
        .expect("test daemon socket")
        .to_string();

    let before = driver.call("get_config", serde_json::json!({}));
    assert!(
        !before.is_error(),
        "get_config failed before the instance failure: {}",
        before.text()
    );

    // 1. A held connection that can rewrite the pipe DACL.
    let control = PipeClient::open(
        &socket,
        READ_CONTROL | WRITE_DAC | FILE_READ_DATA | FILE_WRITE_DATA | SYNCHRONIZE,
    )
    .expect("open the control connection");
    assert!(
        wait_for_listening_instance(&socket, RECOVERY_WINDOW),
        "daemon did not list a new instance after accepting the control connection"
    );
    let original = control.dacl().expect("read the pipe DACL");

    // 2. Deny new instances, then consume the one that is listening.
    let deny = SddlDescriptor::parse(DENY_NEW_INSTANCES_SDDL).expect("parse the deny SDDL");
    control
        .set_dacl(deny.0)
        .expect("deny FILE_CREATE_PIPE_INSTANCE on the pipe");
    let trigger = PipeClient::open(&socket, FILE_READ_DATA | FILE_WRITE_DATA | SYNCHRONIZE);
    let listed_while_denied = wait_for_listening_instance(&socket, BLOCKED_WINDOW);

    // 3. Lift the denial before asserting, so a failure never leaves it set.
    let restored = control.set_dacl(original.as_ptr().cast());
    let trigger = trigger.expect("consume the listening instance while new instances are denied");
    assert!(
        !listed_while_denied,
        "daemon listed a new instance while FILE_CREATE_PIPE_INSTANCE was denied; \
         the instance failure was not triggered"
    );

    let recovered = wait_for_listening_instance(&socket, RECOVERY_WINDOW);
    let after = driver.call("get_config", serde_json::json!({}));
    assert!(
        recovered && !after.is_error(),
        "daemon stopped serving after one pipe instance failed \
         (listed an instance again: {recovered}, DACL restore: {restored:?}); get_config: {}",
        after.text()
    );

    drop(trigger);
    drop(control);
}

/// A connection's long `run_script` sleep is still running when another
/// connection asks the daemon to shut down. The daemon must abort that call
/// and exit; it used to leave the connection task running, so the SDK
/// shutdown waited for the script to finish.
#[test]
fn shutdown_ends_a_call_still_running_on_another_connection() {
    const EXIT_WINDOW: Duration = Duration::from_secs(15);
    let state = IsolatedStateRoot::new().expect("isolated state root");
    let socket = format!(r"\\.\pipe\cua-driver-shutdown-test-{}", std::process::id());
    let mut command = Command::new(env!("CARGO_BIN_EXE_cua-driver"));
    command
        .args([
            "serve",
            "--socket",
            &socket,
            "--no-permissions-gate",
            "--no-overlay",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .env("CUA_DRIVER_RS_TELEMETRY_ENABLED", "false");
    for (name, value) in state.env() {
        command.env(name, value);
    }
    let mut daemon = command.spawn().expect("spawn the daemon");
    // The pipe reads below block. A daemon that accepts and never answers is
    // terminated after this deadline, which ends those reads with an error.
    // The watchdog holds its own handle to this exact process, so it cannot
    // reach another process that later reuses the pid.
    const TEST_DEADLINE: Duration = Duration::from_secs(120);
    let daemon_pid = daemon.id();
    let watchdog_process = duplicate_process_handle(&daemon);
    let (finished, watchdog_done) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
        use windows::Win32::System::Threading::{TerminateProcess, WaitForSingleObject};
        let process = HANDLE(watchdog_process as *mut c_void);
        if watchdog_done.recv_timeout(TEST_DEADLINE)
            == Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        {
            eprintln!("watchdog: terminating daemon {daemon_pid} after {TEST_DEADLINE:?}");
            unsafe {
                if let Err(error) = TerminateProcess(process, 1) {
                    eprintln!("watchdog: TerminateProcess failed: {error}");
                }
                if WaitForSingleObject(process, 10_000) != WAIT_OBJECT_0 {
                    eprintln!("watchdog: daemon {daemon_pid} did not exit");
                }
            }
        }
        let _ = unsafe { CloseHandle(process) };
    });
    struct KillOnDrop<'a>(&'a mut std::process::Child);
    impl Drop for KillOnDrop<'_> {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let daemon = KillOnDrop(&mut daemon);
    assert!(
        wait_for_listening_instance(&socket, RECOVERY_WINDOW),
        "daemon did not list its pipe"
    );

    let open = |socket: &str| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(socket)
            .expect("open a daemon connection")
    };
    let request = |pipe: &mut std::fs::File, line: &str| {
        pipe.write_all(format!("{line}\n").as_bytes())
            .expect("write a daemon request");
    };
    let reply = |pipe: &std::fs::File| {
        let mut line = String::new();
        BufReader::new(pipe)
            .read_line(&mut line)
            .expect("read a daemon reply");
        line
    };

    // A round trip first proves this connection's task is reading, so the
    // script below is in flight before the shutdown connection is accepted.
    let mut call = open(&socket);
    request(&mut call, r#"{"method":"metadata"}"#);
    assert!(reply(&call).contains(r#""ok":true"#));
    request(
        &mut call,
        r#"{"method":"call","name":"run_script","args":{"script":"await cua.sleep(60000); return 'late';","timeout_ms":90000}}"#,
    );
    // The script engine runs on a thread named `cua-run-script`, started
    // only after the SDK admitted the call. Wait for it, so shutdown cannot
    // win the race and refuse the call at admission instead.
    let admission = Instant::now();
    while !process_has_thread_named(daemon_pid, "cua-run-script") {
        assert!(
            admission.elapsed() < RECOVERY_WINDOW,
            "run_script did not start within {RECOVERY_WINDOW:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    assert!(wait_for_listening_instance(&socket, RECOVERY_WINDOW));
    let mut stop = open(&socket);
    request(&mut stop, r#"{"method":"shutdown"}"#);
    assert!(reply(&stop).contains(r#""shutdown":true"#));

    let stopped = Instant::now();
    let status = loop {
        if let Some(status) = daemon.0.try_wait().expect("poll the daemon") {
            break Some(status);
        }
        if stopped.elapsed() >= EXIT_WINDOW {
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        status.is_some_and(|status| status.success()),
        "daemon still running {EXIT_WINDOW:?} after shutdown, or failed: {status:?}; \
         the in-flight run_script call kept it alive"
    );
    drop(call);
    let _ = finished.send(());
    let _ = watchdog.join();
}

/// A handle to `child` the caller owns, as an integer so it can move to
/// another thread.
fn duplicate_process_handle(child: &std::process::Child) -> usize {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE};
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut duplicate = HANDLE::default();
    unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            HANDLE(child.as_raw_handle()),
            GetCurrentProcess(),
            &mut duplicate,
            0,
            false,
            DUPLICATE_SAME_ACCESS,
        )
    }
    .expect("duplicate the daemon's process handle");
    duplicate.0 as usize
}

/// Whether a thread of process `pid` carries the description `name`.
/// Rust publishes thread names on Windows through `SetThreadDescription`.
fn process_has_thread_named(pid: u32, name: &str) -> bool {
    use windows::Win32::Foundation::{CloseHandle, LocalFree, HLOCAL};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows::Win32::System::Threading::{
        GetThreadDescription, OpenThread, THREAD_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else {
            return false;
        };
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut found = false;
        let mut more = Thread32First(snapshot, &mut entry).is_ok();
        while more && !found {
            if entry.th32OwnerProcessID == pid {
                if let Ok(thread) =
                    OpenThread(THREAD_QUERY_LIMITED_INFORMATION, false, entry.th32ThreadID)
                {
                    if let Ok(description) = GetThreadDescription(thread) {
                        found = !description.is_null()
                            && description.to_string().is_ok_and(|text| text == name);
                        let _ = LocalFree(Some(HLOCAL(description.0 as *mut c_void)));
                    }
                    let _ = CloseHandle(thread);
                }
            }
            more = Thread32Next(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
        found
    }
}
