//! Kernel-backed process instance identity for protected snapshots.

use cua_driver_core::browser::ProcessFingerprint;

use crate::ax::enablement::{process_start_stamp_with, read_process_info};

trait ProcessReader {
    fn info(&self, pid: i32, info: &mut libc::proc_bsdinfo) -> libc::c_int;
    fn executable(&self, pid: i32) -> Option<String>;
}

struct KernelReader;

impl ProcessReader for KernelReader {
    fn info(&self, pid: i32, info: &mut libc::proc_bsdinfo) -> libc::c_int {
        read_process_info(pid, info)
    }

    fn executable(&self, pid: i32) -> Option<String> {
        crate::apps::path_of_pid(pid)
    }
}

// U33B will connect this supplier to snapshot publication and resolution.
#[allow(dead_code)]
pub(crate) fn process_fingerprint(pid: i64) -> Option<ProcessFingerprint> {
    fingerprint_with(pid, &KernelReader)
}

fn fingerprint_with(pid: i64, reader: &impl ProcessReader) -> Option<ProcessFingerprint> {
    let native_pid = i32::try_from(pid).ok().filter(|pid| *pid > 0)?;
    let (seconds, micros) = process_start_stamp_with(native_pid, |pid, info| reader.info(pid, info))?;
    if micros >= 1_000_000 {
        return None;
    }
    let start_time = seconds.checked_mul(1_000_000)?;
    let executable = reader.executable(native_pid);
    Some(ProcessFingerprint { pid, start_time: Some(start_time), executable })
}

#[cfg(test)]
mod u33_identity_tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    type InfoResult = Option<(i32, u64, u64)>;

    struct FakeReader {
        info: RefCell<VecDeque<InfoResult>>,
        executable: Option<String>,
        calls: Cell<usize>,
    }

    impl FakeReader {
        fn new(info: &[InfoResult], executable: Option<&str>) -> Self {
            Self { info: RefCell::new(info.iter().copied().collect()), executable: executable.map(str::to_owned), calls: Cell::new(0) }
        }
    }

    impl ProcessReader for FakeReader {
        fn info(&self, _pid: i32, info: &mut libc::proc_bsdinfo) -> libc::c_int {
            self.calls.set(self.calls.get() + 1);
            match self.info.borrow_mut().pop_front().expect("unexpected info read") {
                Some((length, sec, usec)) => { info.pbi_start_tvsec = sec; info.pbi_start_tvusec = usec; length }
                None => 0,
            }
        }
        fn executable(&self, _pid: i32) -> Option<String> { self.executable.clone() }
    }

    fn full(sec: u64, usec: u64) -> InfoResult {
        Some((std::mem::size_of::<libc::proc_bsdinfo>() as i32, sec, usec))
    }

    fn stable(sec: u64, usec: u64, exe: Option<&str>) -> FakeReader {
        FakeReader::new(&[full(sec, usec), full(sec, usec)], exe)
    }

    #[test]
    fn packs_seconds_and_microseconds() {
        assert_eq!(fingerprint_with(42, &stable(12, 345_678, None)).unwrap().start_time, Some(12_345_678));
    }

    #[test]
    fn distinguishes_starts_within_one_second() {
        let a = fingerprint_with(42, &stable(12, 1, Some("/a"))).unwrap();
        let b = fingerprint_with(42, &stable(12, 2, Some("/a"))).unwrap();
        assert_ne!(a.start_time, b.start_time);
    }

    #[test]
    fn rejects_out_of_range_microseconds() {
        assert_eq!(fingerprint_with(42, &stable(12, 1_000_000, None)), None);
    }

    #[test]
    fn rejects_start_time_overflow() {
        assert_eq!(fingerprint_with(42, &stable(u64::MAX, 1, None)), None);
        assert_eq!(fingerprint_with(42, &stable(u64::MAX / 1_000_000, u64::MAX % 1_000_000 + 1, None)), None);
    }

    #[test]
    fn rejects_short_proc_info() {
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        let reader = FakeReader::new(&[Some((size - 1, 12, 345))], None);
        assert_eq!(fingerprint_with(42, &reader), None);
    }

    #[test]
    fn invalid_pid_is_not_coerced() {
        for pid in [0, -1, i64::MAX] {
            let reader = FakeReader::new(&[], None);
            assert_eq!(fingerprint_with(pid, &reader), None);
            assert_eq!(reader.calls.get(), 0);
        }
    }

    #[test]
    fn unreadable_start_has_no_identity() {
        assert_eq!(fingerprint_with(42, &FakeReader::new(&[None], Some("/a"))), None);
        assert_eq!(fingerprint_with(42, &FakeReader::new(&[full(12, 1), None], Some("/a"))), None);
    }

    #[test]
    fn executable_is_optional_and_exact() {
        assert_eq!(fingerprint_with(42, &stable(12, 1, None)).unwrap().executable, None);
        let path = "/Applications/Example.app/Contents/MacOS/Example";
        assert_eq!(fingerprint_with(42, &stable(12, 1, Some(path))).unwrap().executable.as_deref(), Some(path));
    }

    #[test]
    fn stamp_change_during_executable_read_is_unavailable() {
        let reader = FakeReader::new(&[full(12, 1), full(12, 2)], Some("/a"));
        assert_eq!(fingerprint_with(42, &reader), None);
    }
}
