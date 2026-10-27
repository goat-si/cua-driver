use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;

use sha2::{Digest, Sha256};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ExecutableIdentity {
    volume: u32,
    file_id: u64,
    size: u64,
    last_write: u64,
    sha256: [u8; 32],
}

impl ExecutableIdentity {
    pub(super) fn read(path: &str) -> io::Result<Self> {
        // Deny concurrent writers while hashing; do not pin applications against updates.
        let mut file = OpenOptions::new()
            .read(true)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_DELETE).0)
            .open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("application identity requires a file"));
        }
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        unsafe {
            GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info)
                .map_err(io::Error::other)?;
        }
        let mut digest = Sha256::new();
        let mut buffer = [0; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
        Ok(Self {
            volume: info.dwVolumeSerialNumber,
            file_id: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            size: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
            last_write: (u64::from(info.ftLastWriteTime.dwHighDateTime) << 32)
                | u64::from(info.ftLastWriteTime.dwLowDateTime),
            sha256: digest.finalize().into(),
        })
    }
}
