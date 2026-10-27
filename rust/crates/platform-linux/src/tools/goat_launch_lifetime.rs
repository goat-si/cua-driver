//! User applications survive their embedded automation host. Owned helpers use
//! the inherited group instead and are reaped with the driver.

use std::os::unix::process::CommandExt;
use std::process::Command;

pub(super) fn detach_user_app(command: &mut Command) -> &mut Command {
    configure(command, cua_driver_core::host_owned_runtime())
}

fn configure(command: &mut Command, embedded: bool) -> &mut Command {
    if embedded {
        command.process_group(0);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    #[test]
    fn embedded_user_apps_get_a_separate_group_and_standalone_launches_keep_their_group() {
        for embedded in [false, true] {
            let mut command = Command::new("sh");
            command
                .args(["-c", "exec cat"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null());
            let mut child = configure(&mut command, embedded).spawn().unwrap();
            let group = unsafe { libc::getpgid(child.id() as libc::pid_t) };
            let expected = if embedded {
                child.id() as libc::pid_t
            } else {
                unsafe { libc::getpgrp() }
            };
            // Close stdin and reap before asserting so a failure cannot leave
            // this deliberately detached user application behind.
            drop(child.stdin.take());
            assert!(child.wait().unwrap().success());
            assert_eq!(group, expected);
        }
    }
}
