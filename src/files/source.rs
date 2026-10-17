//! What a sender reads. Nothing is offered before its exact length and
//! digest are known: files are hashed with their identity recorded, and
//! standard input, text, and links go to a sealed, read-only spool. Bytes
//! are hashed again as they are sent, so a file that changed fails instead
//! of sending different bytes under the same offer.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustix::fs::{AtFlags, Mode, OFlags};
use rustix::io::Errno;

use super::FileIdentity;
use super::destination::free_bytes;
use super::names::{NameError, OutputName, spool_name};
use crate::ids::TransferId;
use crate::limits::{MAX_FILE_BYTES, MIN_FREE_BYTES};

/// Bytes read at once while hashing a file.
const HASH_BUFFER: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum PrepareError {
    #[error("{0}: is a symbolic link; give the file itself")]
    Symlink(PathBuf),
    #[error("{0}: is not a regular file")]
    NotRegular(PathBuf),
    #[error("{0}: is larger than 16 GiB")]
    TooLarge(PathBuf),
    #[error("{0}: changed while it was being read")]
    Changed(PathBuf),
    #[error(
        "{0}: holds more bytes than its size says, as files under /proc do; send its content as standard input instead"
    )]
    SizeUnknown(PathBuf),
    #[error("{0}: its name cannot be offered: {1}")]
    Name(PathBuf, NameError),
    #[error("{0}: {1}")]
    Io(PathBuf, io::Error),
}

/// Bytes an item is read from.
#[derive(Clone, Debug)]
pub enum Source {
    /// Bytes already in memory.
    Memory(Arc<Vec<u8>>),
    /// An open file and the identity it had when it was prepared.
    File {
        file: Arc<File>,
        identity: FileIdentity,
    },
}

impl Source {
    /// Reads exactly `len` bytes at `offset`. Blocking.
    pub fn read_at(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        match self {
            Self::Memory(bytes) => {
                let start = usize::try_from(offset).map_err(io::Error::other)?;
                let end = start
                    .checked_add(len)
                    .filter(|end| *end <= bytes.len())
                    .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
                Ok(bytes[start..end].to_vec())
            }
            Self::File { file, .. } => {
                let mut buf = vec![0u8; len];
                file.read_exact_at(&mut buf, offset)?;
                Ok(buf)
            }
        }
    }

    /// Whether the file still has the identity it had when prepared and
    /// still ends where its size says. Blocking.
    pub fn unchanged(&self) -> io::Result<bool> {
        match self {
            Self::Memory(_) => Ok(true),
            Self::File { file, identity } => {
                let st = rustix::fs::fstat(file.as_ref())?;
                Ok(FileIdentity::of(&st) == *identity && ends_at(file, identity.size)?)
            }
        }
    }
}

/// One item ready to offer.
#[derive(Clone, Debug)]
pub struct PreparedItem {
    pub name: Vec<u8>,
    pub size: u64,
    pub digest: [u8; 32],
    pub source: Source,
}

/// Opens and hashes one regular file, without following a symbolic link
/// and without blocking on a pipe or device. Blocking.
pub fn prepare_file(path: &Path) -> Result<PreparedItem, PrepareError> {
    let io = |e: io::Error| PrepareError::Io(path.to_owned(), e);
    let name = path.file_name().map(|n| n.as_bytes()).unwrap_or_default();
    OutputName::new(name).map_err(|e| PrepareError::Name(path.to_owned(), e))?;

    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| match e {
        Errno::LOOP => PrepareError::Symlink(path.to_owned()),
        e => io(e.into()),
    })?;
    let st = rustix::fs::fstat(&fd).map_err(|e| io(e.into()))?;
    #[allow(clippy::unnecessary_cast)]
    if rustix::fs::FileType::from_raw_mode(st.st_mode as _) != rustix::fs::FileType::RegularFile {
        return Err(PrepareError::NotRegular(path.to_owned()));
    }

    let identity = FileIdentity::of(&st);
    if identity.size > MAX_FILE_BYTES {
        return Err(PrepareError::TooLarge(path.to_owned()));
    }

    let file = File::from(fd);
    let digest = hash_file(&file, identity.size)
        .map_err(io)?
        .ok_or_else(|| PrepareError::Changed(path.to_owned()))?;
    let ended = ends_at(&file, identity.size).map_err(io)?;
    // A file written to meanwhile has a new identity. One that kept it and
    // still has bytes past its size reports a wrong size, as files under
    // /proc do; offered at that size, it would arrive cut short.
    let after = rustix::fs::fstat(&file).map_err(|e| io(e.into()))?;
    if FileIdentity::of(&after) != identity {
        return Err(PrepareError::Changed(path.to_owned()));
    }
    if !ended {
        return Err(PrepareError::SizeUnknown(path.to_owned()));
    }
    Ok(PreparedItem {
        name: name.to_vec(),
        size: identity.size,
        digest,
        source: Source::File {
            file: Arc::new(file),
            identity,
        },
    })
}

/// The digest of the first `size` bytes, or `None` if the file is shorter.
fn hash_file(file: &File, size: u64) -> io::Result<Option<[u8; 32]>> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; HASH_BUFFER];
    let mut offset = 0u64;
    while offset < size {
        let want =
            usize::try_from((size - offset).min(HASH_BUFFER as u64)).map_err(io::Error::other)?;
        let read = file.read_at(&mut buf[..want], offset)?;
        if read == 0 {
            return Ok(None);
        }
        hasher.update(&buf[..read]);
        offset += read as u64;
    }
    Ok(Some(*hasher.finalize().as_bytes()))
}

/// Whether nothing can be read at `size`. Blocking.
fn ends_at(file: &File, size: u64) -> io::Result<bool> {
    Ok(file.read_at(&mut [0u8; 1], size)? == 0)
}

#[derive(Debug, thiserror::Error)]
pub enum SpoolError {
    #[error("the input is larger than the {0} byte limit")]
    TooLarge(u64),
    #[error("not enough free disk space for the input")]
    NoSpace,
    #[error("{0}")]
    Io(#[from] io::Error),
}

impl From<Errno> for SpoolError {
    fn from(e: Errno) -> Self {
        Self::Io(e.into())
    }
}

/// Input being written to a private spool before anything is offered. The
/// file is the spool's to delete until [`Spool::keep`] hands it to the
/// ledger; dropping the spool first deletes it.
pub struct Spool {
    dir: OwnedFd,
    name: String,
    file: Option<File>,
    hasher: blake3::Hasher,
    len: u64,
    /// The most bytes the file can hold: `len`, or more after a write
    /// that failed partway.
    most: u64,
    limit: u64,
    /// The first bytes, kept for previews and checks.
    head: Vec<u8>,
    keep: usize,
}

/// A spool sealed at end of input: immutable and readable only.
pub struct Sealed {
    pub name: String,
    pub size: u64,
    pub digest: [u8; 32],
    pub head: Vec<u8>,
    pub source: Source,
}

/// A spool file that could not be deleted: what the ledger needs to
/// delete it later.
pub struct Leftover {
    pub name: String,
    pub identity: Option<(u64, u64)>,
    pub bytes: u64,
}

impl Spool {
    /// Creates the transfer's spool in `dir`; it must not exist yet. At
    /// most `limit` bytes may be written; the first `keep` bytes are kept
    /// in memory as well.
    pub fn create(dir: &Path, transfer: TransferId, limit: u64, keep: usize) -> io::Result<Self> {
        let dir = rustix::fs::open(
            dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let name = spool_name(transfer);
        let fd = rustix::fs::openat(
            &dir,
            name.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?;
        Ok(Self {
            dir,
            name,
            file: Some(File::from(fd)),
            hasher: blake3::Hasher::new(),
            len: 0,
            most: 0,
            limit,
            head: Vec::new(),
            keep,
        })
    }

    /// Appends `data`, refusing to pass the limit or to fill the disk.
    /// Blocking.
    pub fn write(&mut self, data: &[u8]) -> Result<(), SpoolError> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("sealed"))?;
        let end = self
            .len
            .checked_add(data.len() as u64)
            .filter(|end| *end <= self.limit)
            .ok_or(SpoolError::TooLarge(self.limit))?;
        if free_bytes(&*file)? < (data.len() as u64).saturating_add(MIN_FREE_BYTES) {
            return Err(SpoolError::NoSpace);
        }
        self.most = end;
        file.write_all(data)?;
        self.hasher.update(data);
        let room = self.keep.saturating_sub(self.head.len());
        self.head.extend_from_slice(&data[..data.len().min(room)]);
        self.len = end;
        Ok(())
    }

    /// The first bytes written, up to the `keep` given at creation.
    pub fn head(&self) -> &[u8] {
        &self.head
    }

    /// Seals the spool at end of input: synced, read-only, and reopened for
    /// reading only, so the offered bytes cannot change. The file is still
    /// the spool's to delete. Blocking.
    pub fn seal(&mut self) -> Result<Sealed, SpoolError> {
        let file = self.file.take().ok_or_else(|| io::Error::other("sealed"))?;
        // Read-only first, so the sync makes the mode durable with the bytes:
        // recovery takes a spool that is not read-only as lost.
        rustix::fs::fchmod(&file, Mode::from_raw_mode(0o400))?;
        file.sync_all()?;
        drop(file);
        rustix::fs::fsync(&self.dir)?;

        let fd = rustix::fs::openat(
            &self.dir,
            self.name.as_str(),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let identity = FileIdentity::of(&rustix::fs::fstat(&fd)?);
        if identity.size != self.len {
            return Err(io::Error::other("the spool changed while it was sealed").into());
        }
        Ok(Sealed {
            name: self.name.clone(),
            size: self.len,
            digest: *self.hasher.finalize().as_bytes(),
            head: std::mem::take(&mut self.head),
            source: Source::File {
                file: Arc::new(File::from(fd)),
                identity,
            },
        })
    }

    /// The ledger owns the file from now on; the spool no longer deletes it.
    pub fn keep(&mut self) {
        self.name.clear();
    }

    /// Deletes the file now, unless it was kept. If that fails, returns
    /// what the ledger needs to delete it later. Blocking.
    pub fn discard(&mut self) -> Result<(), (io::Error, Leftover)> {
        let name = std::mem::take(&mut self.name);
        if name.is_empty() {
            return Ok(());
        }
        match rustix::fs::unlinkat(&self.dir, name.as_str(), AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => Ok(()),
            Err(e) => {
                // A write that failed partway leaves bytes `len` does not
                // count, so the file is charged by its real size, or by the
                // most it can hold when that cannot be read.
                let found = rustix::fs::statat(&self.dir, name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
                    .ok()
                    .map(|st| FileIdentity::of(&st));
                Err((
                    e.into(),
                    Leftover {
                        name,
                        identity: found.as_ref().map(FileIdentity::key),
                        bytes: found.map_or(self.most, |f| f.size),
                    },
                ))
            }
        }
    }
}

impl Drop for Spool {
    /// A spool nobody kept never becomes an offer, so its file goes. Code
    /// that must know whether that worked calls [`Spool::discard`].
    fn drop(&mut self) {
        if !self.name.is_empty() {
            let _ = rustix::fs::unlinkat(&self.dir, self.name.as_str(), AtFlags::empty());
        }
    }
}

/// Opens a sealed spool again after a restart, if it is still the file
/// that was sealed: same device and inode, same size, read-only.
pub fn reopen_spool(
    dir: &Path,
    name: &[u8],
    expected: (u64, u64),
    size: u64,
) -> io::Result<Option<Source>> {
    let dir = rustix::fs::open(
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let fd = match rustix::fs::openat(
        &dir,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::NOENT | Errno::LOOP) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let st = rustix::fs::fstat(&fd)?;
    let identity = FileIdentity::of(&st);
    #[allow(clippy::unnecessary_cast)]
    let read_only = st.st_mode as u32 & 0o222 == 0;
    if identity.key() != expected || identity.size != size || !read_only {
        return Ok(None);
    }
    Ok(Some(Source::File {
        file: Arc::new(File::from(fd)),
        identity,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::{OsRng, RngCore};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hyakume-source-{name}-{}-{:x}",
            std::process::id(),
            OsRng.next_u64()
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[test]
    fn a_prepared_file_has_its_exact_length_and_digest() {
        let dir = temp("prepare");
        let path = dir.join("notes.txt");
        std::fs::write(&path, b"hello there").unwrap();
        let item = prepare_file(&path).unwrap();
        assert_eq!(item.name, b"notes.txt");
        assert_eq!(item.size, 11);
        assert_eq!(item.digest, *blake3::hash(b"hello there").as_bytes());
        assert_eq!(item.source.read_at(6, 5).unwrap(), b"there");
        assert!(item.source.unchanged().unwrap());
        std::fs::write(&path, b"hello world").unwrap();
        assert!(!item.source.unchanged().unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_file_with_more_bytes_than_its_size_is_refused() {
        // Linux reports a size of zero for this file, which is not empty.
        let path = Path::new("/proc/self/status");
        assert_eq!(std::fs::metadata(path).unwrap().len(), 0);
        assert!(matches!(
            prepare_file(path),
            Err(PrepareError::SizeUnknown(_))
        ));
        // A source like it, opened some other way, fails the sender's
        // check before its item is reported sent.
        let file = File::open(path).unwrap();
        let identity = FileIdentity::of(&rustix::fs::fstat(&file).unwrap());
        let source = Source::File {
            file: Arc::new(file),
            identity,
        };
        assert!(!source.unchanged().unwrap());
    }

    #[test]
    fn links_pipes_and_directories_are_refused() {
        let dir = temp("refuse");
        std::fs::write(dir.join("real"), b"x").unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        assert!(matches!(
            prepare_file(&dir.join("link")),
            Err(PrepareError::Symlink(_))
        ));
        assert!(matches!(
            prepare_file(&dir),
            Err(PrepareError::NotRegular(_))
        ));
        let fifo = dir.join("fifo");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();
        // Opening must not wait for a writer.
        assert!(matches!(
            prepare_file(&fifo),
            Err(PrepareError::NotRegular(_))
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_spool_is_sealed_read_only_and_an_unsealed_one_disappears() {
        let dir = temp("spool");
        let id = TransferId::from_bytes([7; 16]);
        let mut spool = Spool::create(&dir, id, 10, 4).unwrap();
        spool.write(b"hello").unwrap();
        assert!(matches!(
            spool.write(b"world!"),
            Err(SpoolError::TooLarge(10))
        ));
        spool.write(b"world").unwrap();
        let sealed = spool.seal().unwrap();
        spool.keep();
        assert_eq!((sealed.size, sealed.head.as_slice()), (10, &b"hell"[..]));
        assert_eq!(sealed.digest, *blake3::hash(b"helloworld").as_bytes());
        let mode = std::fs::metadata(dir.join(&sealed.name)).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o400);
        let Source::File { identity, .. } = &sealed.source else {
            panic!("a spool is a file");
        };
        assert!(
            reopen_spool(&dir, sealed.name.as_bytes(), identity.key(), 10)
                .unwrap()
                .is_some()
        );
        assert!(
            reopen_spool(&dir, sealed.name.as_bytes(), identity.key(), 11)
                .unwrap()
                .is_none()
        );

        let other = TransferId::from_bytes([8; 16]);
        let mut abandoned = Spool::create(&dir, other, 10, 0).unwrap();
        abandoned.write(b"partial").unwrap();
        drop(abandoned);
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(names.len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_spool_that_cannot_be_deleted_says_what_is_left() {
        let dir = temp("leftover");
        let id = TransferId::from_bytes([9; 16]);
        let mut spool = Spool::create(&dir, id, 10, 0).unwrap();
        spool.write(b"abc").unwrap();
        // A write that failed partway left bytes the spool never counted.
        let name = spool_name(id);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join(&name))
            .unwrap();
        file.write_all(b"de").unwrap();
        // Nobody may remove names from the folder now.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let discarded = spool.discard();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let Err((_, left)) = discarded else {
            panic!("the file was deleted from a read-only folder");
        };
        let path = dir.join(&left.name);
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(left.identity, Some((meta.dev(), meta.ino())));
        assert_eq!(left.name, name);
        assert_eq!(left.bytes, 5);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
