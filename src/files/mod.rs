//! Files on this device: sources to send, private partial files, and
//! publication under a final name. A received item appears only through a
//! no-clobber rename of its verified partial file, so nobody sees half a
//! file and nothing already there is overwritten.

pub mod destination;
pub mod names;
pub mod source;

use rustix::fs::Stat;

/// What identifies one file: its device and inode, and the size and times
/// that change when its content changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub modified: (i64, u32),
    pub changed: (i64, u32),
}

impl FileIdentity {
    // The integer types of `stat` fields differ between targets.
    #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
    pub fn of(st: &Stat) -> Self {
        Self {
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
            size: u64::try_from(st.st_size).unwrap_or(0),
            modified: (st.st_mtime as i64, st.st_mtime_nsec as u32),
            changed: (st.st_ctime as i64, st.st_ctime_nsec as u32),
        }
    }

    /// Device and inode only, as the file ledger records them.
    pub fn key(&self) -> (u64, u64) {
        (self.dev, self.ino)
    }
}

/// Whether `st` is a regular file owned by this user.
#[allow(clippy::unnecessary_cast)]
pub(crate) fn owned_regular(st: &Stat) -> bool {
    rustix::fs::FileType::from_raw_mode(st.st_mode as _) == rustix::fs::FileType::RegularFile
        && st.st_uid as u32 == rustix::process::geteuid().as_raw()
}

/// A hex form of a transfer ID for file names.
fn hex(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(bytes)
}
