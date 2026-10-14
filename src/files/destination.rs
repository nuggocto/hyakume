//! Receive destinations. A folder qualifies only if it resolves without
//! links, belongs to this user alone, is on ext4, btrfs, XFS, or tmpfs, and
//! passes a live probe of no-clobber rename and sync, since a filesystem's
//! name alone promises nothing. Every later operation uses the opened
//! handle, so a folder swapped in afterwards is never written to.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rand_core::{OsRng, RngCore};
use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags, ResolveFlags};
use rustix::io::Errno;

use super::{FileIdentity, owned_regular};
use crate::state::Place;

/// `statfs` magic numbers of the declared filesystems.
const EXT4: i64 = 0xEF53;
const BTRFS: i64 = 0x9123_683E;
const XFS: i64 = 0x5846_5342;
const TMPFS: i64 = 0x0102_1994;

#[derive(Debug, thiserror::Error)]
pub enum DestinationError {
    #[error("the path must be absolute")]
    Relative,
    #[error("a part of the path is a symbolic link; give the real path")]
    Symlink,
    #[error("it is not a directory")]
    NotDirectory,
    #[error("it does not exist")]
    Missing,
    #[error("it belongs to another user")]
    NotOwned,
    #[error("other users can write to it")]
    OpenToOthers,
    #[error("its filesystem (type {0:#x}) is not one Hyakume can publish into safely")]
    Filesystem(i64),
    #[error("its filesystem does not support renaming without replacing")]
    NoAtomicRename,
    #[error("it is no longer the directory that was chosen")]
    Replaced,
    #[error("the qualification check failed: {0}")]
    Probe(io::Error),
    #[error("{0}")]
    Io(io::Error),
}

/// Why publishing one item failed.
#[derive(Debug, thiserror::Error)]
pub enum PublishFailure {
    #[error("a file with that name already exists")]
    Exists,
    #[error("{0}")]
    Io(io::Error),
    /// The rename happened, so the file is under its final name, but the
    /// folder could not be synced to make the new name durable.
    #[error("the file is in place but its folder could not be synced: {0}")]
    Unsynced(io::Error),
}

/// A qualified destination directory.
#[derive(Debug)]
pub struct Destination {
    dir: OwnedFd,
    place: Place,
}

impl Destination {
    /// Opens `path` and qualifies it for publication.
    pub fn qualify(path: &Path) -> Result<Self, DestinationError> {
        let dir = open_dir(path)?;
        let st = rustix::fs::fstat(&dir).map_err(|e| DestinationError::Io(e.into()))?;
        #[allow(clippy::unnecessary_cast)]
        if st.st_uid as u32 != rustix::process::geteuid().as_raw() {
            return Err(DestinationError::NotOwned);
        }
        #[allow(clippy::unnecessary_cast)]
        if st.st_mode as u32 & 0o022 != 0 {
            return Err(DestinationError::OpenToOthers);
        }

        let fs = rustix::fs::fstatfs(&dir).map_err(|e| DestinationError::Io(e.into()))?;
        #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
        let kind = fs.f_type as i64;
        if ![EXT4, BTRFS, XFS, TMPFS].contains(&kind) {
            return Err(DestinationError::Filesystem(kind));
        }

        probe(&dir)?;
        let id = FileIdentity::of(&st);
        Ok(Self {
            dir,
            place: Place {
                dir: path.as_os_str().as_bytes().to_vec(),
                dev: id.dev,
                ino: id.ino,
            },
        })
    }

    /// Opens a destination recorded earlier, for recovery and cleanup. The
    /// directory must still be the one recorded.
    pub fn reopen(place: &Place) -> Result<Self, DestinationError> {
        let path = Path::new(std::ffi::OsStr::from_bytes(&place.dir));
        let dir = open_dir(path)?;
        let st = rustix::fs::fstat(&dir).map_err(|e| DestinationError::Io(e.into()))?;
        let id = FileIdentity::of(&st);
        if (id.dev, id.ino) != (place.dev, place.ino) {
            return Err(DestinationError::Replaced);
        }
        Ok(Self {
            dir,
            place: place.clone(),
        })
    }

    pub fn place(&self) -> &Place {
        &self.place
    }

    /// Bytes available to this user on the destination's filesystem.
    pub fn free_bytes(&self) -> io::Result<u64> {
        free_bytes(&self.dir)
    }

    /// Creates a new private partial file. It must not exist yet.
    pub fn create_partial(&self, name: &str) -> io::Result<(File, FileIdentity)> {
        let fd = rustix::fs::openat(
            &self.dir,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?;
        let st = rustix::fs::fstat(&fd)?;
        Ok((File::from(fd), FileIdentity::of(&st)))
    }

    /// The device and inode of `name` in this directory, without following
    /// a link, or `None` if nothing has that name.
    pub fn identity_of(&self, name: &[u8]) -> io::Result<Option<(u64, u64)>> {
        match rustix::fs::statat(&self.dir, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => Ok(Some(FileIdentity::of(&st).key())),
            Err(Errno::NOENT) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Publishes a partial file under its final name, atomically and only
    /// if the name is free, then makes the rename durable. A preliminary
    /// check for the name would only be advice; the rename decides.
    pub fn publish(&self, partial: &str, output: &str) -> Result<(), PublishFailure> {
        match rustix::fs::renameat_with(
            &self.dir,
            partial,
            &self.dir,
            output,
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => {}
            Err(Errno::EXIST) => return Err(PublishFailure::Exists),
            Err(e) => return Err(PublishFailure::Io(e.into())),
        }
        self.sync().map_err(PublishFailure::Unsynced)
    }

    /// Syncs the folder, making the names in it durable.
    pub fn sync(&self) -> io::Result<()> {
        Ok(rustix::fs::fsync(&self.dir)?)
    }

    /// Removes a file Hyakume owns. It must be a regular file of this user,
    /// and when `expected` is known, the same device and inode. A name with
    /// nothing behind it counts as removed.
    pub fn remove(&self, name: &[u8], expected: Option<(u64, u64)>) -> io::Result<()> {
        remove_owned(&self.dir, name, expected)
    }
}

/// Opens a directory without following symbolic links in any component.
fn open_dir(path: &Path) -> Result<OwnedFd, DestinationError> {
    if !path.is_absolute() {
        return Err(DestinationError::Relative);
    }
    rustix::fs::openat2(
        rustix::fs::CWD,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
    )
    .map_err(|e| match e {
        Errno::LOOP => DestinationError::Symlink,
        Errno::NOTDIR => DestinationError::NotDirectory,
        Errno::NOENT => DestinationError::Missing,
        e => DestinationError::Io(e.into()),
    })
}

pub(crate) fn free_bytes(fd: impl AsFd) -> io::Result<u64> {
    let st = rustix::fs::fstatvfs(fd)?;
    Ok(st.f_bavail.saturating_mul(st.f_frsize))
}

/// Removes `name` from `dir` if it is a regular file of this user and, when
/// `expected` is known, has that device and inode. Only a single name in
/// `dir` is ever removed: a path that could reach outside it is refused.
pub(crate) fn remove_owned(
    dir: impl AsFd,
    name: &[u8],
    expected: Option<(u64, u64)>,
) -> io::Result<()> {
    let single = !name.is_empty() && !name.contains(&b'/') && !name.contains(&0);
    if !single || name == b"." || name == b".." {
        return Err(io::Error::other("not a file name in its folder"));
    }

    let st = match rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => st,
        Err(Errno::NOENT) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let ours = owned_regular(&st) && expected.is_none_or(|key| FileIdentity::of(&st).key() == key);
    if !ours {
        return Err(io::Error::other(
            "the file in its place is not the one Hyakume created",
        ));
    }
    match rustix::fs::unlinkat(&dir, name, AtFlags::empty()) {
        Ok(()) | Err(Errno::NOENT) => {}
        Err(e) => return Err(e.into()),
    }
    rustix::fs::fsync(&dir)?;
    Ok(())
}

/// Shows that the directory gives the guarantees publication needs, using
/// two scratch files that are removed afterwards whatever happens.
fn probe(dir: &OwnedFd) -> Result<(), DestinationError> {
    let tag = OsRng.next_u64();
    let a = format!("{}probe-{tag:016x}-a", super::names::RESERVED_PREFIX);
    let b = format!("{}probe-{tag:016x}-b", super::names::RESERVED_PREFIX);
    let c = format!("{}probe-{tag:016x}-c", super::names::RESERVED_PREFIX);
    let result = probe_steps(dir, &a, &b, &c);
    for name in [&a, &b, &c] {
        let _ = rustix::fs::unlinkat(dir, name.as_str(), AtFlags::empty());
    }
    let _ = rustix::fs::fsync(dir);
    result
}

fn probe_steps(dir: &OwnedFd, a: &str, b: &str, c: &str) -> Result<(), DestinationError> {
    let create = |name: &str, byte: u8| -> io::Result<()> {
        let fd = rustix::fs::openat(
            dir,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?;
        let mut file = File::from(fd);
        file.write_all(&[byte])?;
        file.sync_all()
    };
    create(a, b'a').map_err(DestinationError::Probe)?;
    // Exclusive creation refuses a name in use.
    match create(a, b'x') {
        Err(e) if e.raw_os_error() == Some(Errno::EXIST.raw_os_error()) => {}
        Ok(()) => return Err(DestinationError::NoAtomicRename),
        Err(e) => return Err(DestinationError::Probe(e)),
    }
    create(b, b'b').map_err(DestinationError::Probe)?;
    match rustix::fs::renameat_with(dir, a, dir, b, RenameFlags::NOREPLACE) {
        Err(Errno::EXIST) => {}
        Err(Errno::INVAL | Errno::NOSYS | Errno::NOTSUP) => {
            return Err(DestinationError::NoAtomicRename);
        }
        Ok(()) => return Err(DestinationError::NoAtomicRename),
        Err(e) => return Err(DestinationError::Probe(e.into())),
    }
    // The refused rename left the existing file as it was.
    let kept = rustix::fs::openat(
        dir,
        b,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| DestinationError::Probe(e.into()))?;
    let mut byte = [0u8; 1];
    let read = rustix::io::read(&kept, &mut byte).map_err(|e| DestinationError::Probe(e.into()))?;
    if read != 1 || byte[0] != b'b' {
        return Err(DestinationError::NoAtomicRename);
    }
    match rustix::fs::renameat_with(dir, a, dir, c, RenameFlags::NOREPLACE) {
        Ok(()) => {}
        Err(Errno::INVAL | Errno::NOSYS | Errno::NOTSUP) => {
            return Err(DestinationError::NoAtomicRename);
        }
        Err(e) => return Err(DestinationError::Probe(e.into())),
    }
    rustix::fs::fsync(dir).map_err(|e| DestinationError::Probe(e.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    /// A private temporary directory on the filesystem the tests run on.
    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hyakume-dest-{name}-{}-{:x}",
            std::process::id(),
            OsRng.next_u64()
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[test]
    fn publication_never_replaces_an_existing_file() {
        let dir = temp("noclobber");
        let dest = Destination::qualify(&dir).unwrap();
        std::fs::write(dir.join("report.txt"), b"theirs").unwrap();
        let (mut file, _) = dest.create_partial(".hyakume-x-0.part").unwrap();
        file.write_all(b"ours").unwrap();
        assert!(matches!(
            dest.publish(".hyakume-x-0.part", "report.txt"),
            Err(PublishFailure::Exists)
        ));
        assert_eq!(std::fs::read(dir.join("report.txt")).unwrap(), b"theirs");
        dest.publish(".hyakume-x-0.part", "report (2).txt").unwrap();
        assert_eq!(std::fs::read(dir.join("report (2).txt")).unwrap(), b"ours");
        // The probe left nothing behind.
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["report (2).txt", "report.txt"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_symlinked_or_shared_destination_is_refused_before_use() {
        let base = temp("refuse");
        let real = base.join("real");
        std::fs::create_dir(&real).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(matches!(
            Destination::qualify(&link),
            Err(DestinationError::Symlink)
        ));
        assert!(matches!(
            Destination::qualify(&link.join("inside")),
            Err(DestinationError::Symlink)
        ));
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(matches!(
            Destination::qualify(&real),
            Err(DestinationError::OpenToOthers)
        ));
        assert!(matches!(
            Destination::qualify(Path::new("relative")),
            Err(DestinationError::Relative)
        ));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_folder_this_user_cannot_write_to_is_refused() {
        let dir = temp("readonly");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let refused = Destination::qualify(&dir);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            matches!(refused, Err(DestinationError::Probe(_))),
            "{refused:?}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_unsupported_filesystem_is_refused() {
        // /proc is a real directory of this user's processes on a
        // filesystem no transfer may publish into.
        let own = PathBuf::from(format!("/proc/{}", std::process::id()));
        assert!(matches!(
            Destination::qualify(&own),
            Err(DestinationError::Filesystem(_))
        ));
    }

    #[test]
    fn a_directory_put_in_place_of_the_chosen_one_is_noticed() {
        let base = temp("replaced");
        let chosen = base.join("in");
        std::fs::create_dir(&chosen).unwrap();
        let place = Destination::qualify(&chosen).unwrap().place().clone();
        std::fs::rename(&chosen, base.join("moved")).unwrap();
        std::fs::create_dir(&chosen).unwrap();
        assert!(matches!(
            Destination::reopen(&place),
            Err(DestinationError::Replaced)
        ));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn removal_refuses_a_file_that_is_not_the_one_recorded() {
        let dir = temp("remove");
        let dest = Destination::qualify(&dir).unwrap();
        let (_, id) = dest.create_partial(".hyakume-y-0.part").unwrap();
        // Moved aside, not deleted, so the new file cannot reuse its inode.
        std::fs::rename(dir.join(".hyakume-y-0.part"), dir.join("moved")).unwrap();
        std::fs::write(dir.join(".hyakume-y-0.part"), b"someone else's").unwrap();
        assert!(dest.remove(b".hyakume-y-0.part", Some(id.key())).is_err());
        assert!(dir.join(".hyakume-y-0.part").exists());
        assert!(dest.remove(b"never-existed", Some(id.key())).is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn removal_never_reaches_outside_its_folder() {
        let base = temp("outside");
        let inner = base.join("in");
        std::fs::create_dir(&inner).unwrap();
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o700)).unwrap();
        let canary = base.join("canary");
        std::fs::write(&canary, b"keep").unwrap();
        let dest = Destination::qualify(&inner).unwrap();
        // The right inode, reached through a name that leaves the folder.
        let key = dest.identity_of(b"../canary").unwrap();
        assert!(dest.remove(b"../canary", key).is_err());
        assert!(dest.remove(b"..", None).is_err());
        assert!(canary.exists());
        std::fs::remove_dir_all(base).unwrap();
    }
}
