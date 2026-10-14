//! Where the service keeps its state and socket, and the lock that makes
//! one service their owner. Both folders must be real, owned by this user,
//! and closed to others. One that is not is refused rather than repaired,
//! because its contents may already be known to someone else.

use std::fs::DirBuilder;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation, Mode, OFlags};

/// Environment variable that overrides the state directory.
pub const STATE_VAR: &str = "HYAKUME_STATE_DIR";
/// Environment variable that overrides the runtime directory.
pub const RUNTIME_VAR: &str = "HYAKUME_RUNTIME_DIR";

#[derive(Debug, thiserror::Error)]
pub enum LocationError {
    #[error("no runtime directory: XDG_RUNTIME_DIR is not set; pass --runtime-dir")]
    NoRuntimeDir,
    #[error("no state directory: neither XDG_STATE_HOME nor HOME is set; pass --state-dir")]
    NoStateDir,
    #[error("{0} is a symbolic link or not a directory")]
    NotDirectory(PathBuf),
    #[error("{0} belongs to another user")]
    WrongOwner(PathBuf),
    #[error("{0} is open to other users; run chmod 700 on it")]
    OpenToOthers(PathBuf),
    #[error("another Hyakume service already uses {0}")]
    AlreadyRunning(PathBuf),
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
}

impl LocationError {
    fn io(path: &Path, source: impl Into<io::Error>) -> Self {
        Self::Io {
            path: path.to_owned(),
            source: source.into(),
        }
    }
}

/// The two directories one service owns.
#[derive(Clone, Debug)]
pub struct Locations {
    pub state: PathBuf,
    pub runtime: PathBuf,
}

impl Locations {
    /// Explicit directories win; otherwise the XDG defaults:
    /// `$XDG_STATE_HOME/hyakume` (or `~/.local/state/hyakume`) and
    /// `$XDG_RUNTIME_DIR/hyakume`. Relative paths are taken from the
    /// current directory.
    pub fn resolve(
        state: Option<PathBuf>,
        runtime: Option<PathBuf>,
    ) -> Result<Self, LocationError> {
        let state = match state {
            Some(p) => p,
            None => default_state()?,
        };
        let runtime = match runtime {
            Some(p) => p,
            None => absolute_var("XDG_RUNTIME_DIR")
                .ok_or(LocationError::NoRuntimeDir)?
                .join("hyakume"),
        };
        Ok(Self {
            state: std::path::absolute(&state).map_err(|e| LocationError::io(&state, e))?,
            runtime: std::path::absolute(&runtime).map_err(|e| LocationError::io(&runtime, e))?,
        })
    }

    pub fn socket(&self) -> PathBuf {
        self.runtime.join("hyakume.sock")
    }

    pub fn database(&self) -> PathBuf {
        self.state.join("hyakume.db")
    }

    pub fn key_file(&self) -> PathBuf {
        self.state.join("identity.age")
    }

    pub fn spool(&self) -> PathBuf {
        self.state.join("spool")
    }

    pub fn app_storage(&self) -> PathBuf {
        self.state.join("received")
    }

    /// Creates or checks the state and runtime directories and the
    /// directories inside the state directory, and returns these locations
    /// by their real paths. Files inside are recorded by path and reopened
    /// without following any link, so a linked ancestor, such as a home
    /// folder that is a link, is resolved once here.
    pub fn prepare(&self) -> Result<Self, LocationError> {
        private_dir(&self.state)?;
        private_dir(&self.runtime)?;
        let real = Self {
            state: real_path(&self.state)?,
            runtime: real_path(&self.runtime)?,
        };

        // Checked again by the real path, which is the one kept.
        private_dir(&real.state)?;
        private_dir(&real.runtime)?;
        private_dir(&real.spool())?;
        private_dir(&real.app_storage())?;
        Ok(real)
    }
}

fn absolute_var(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

fn default_state() -> Result<PathBuf, LocationError> {
    if let Some(base) = absolute_var("XDG_STATE_HOME") {
        return Ok(base.join("hyakume"));
    }
    absolute_var("HOME")
        .map(|home| home.join(".local/state/hyakume"))
        .ok_or(LocationError::NoStateDir)
}

fn real_path(path: &Path) -> Result<PathBuf, LocationError> {
    std::fs::canonicalize(path).map_err(|e| LocationError::io(path, e))
}

/// Creates `path` with mode 0700 if it is missing, then checks that it is a
/// directory owned by this user and closed to everyone else.
pub fn private_dir(path: &Path) -> Result<(), LocationError> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| LocationError::io(parent, e))?;
            }
            match DirBuilder::new().mode(0o700).create(path) {
                Ok(()) => {}
                // Someone made it in between; check what is there.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(LocationError::io(path, e)),
            }
            std::fs::symlink_metadata(path).map_err(|e| LocationError::io(path, e))?
        }
        Err(e) => return Err(LocationError::io(path, e)),
    };
    if !meta.file_type().is_dir() {
        return Err(LocationError::NotDirectory(path.to_owned()));
    }
    if meta.uid() != rustix::process::geteuid().as_raw() {
        return Err(LocationError::WrongOwner(path.to_owned()));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(LocationError::OpenToOthers(path.to_owned()));
    }
    Ok(())
}

/// Held for the life of the service. The operating system releases it when
/// the process ends, however it ends.
#[derive(Debug)]
pub struct InstanceLock(#[allow(dead_code)] std::os::fd::OwnedFd);

/// Takes the lock that makes this process the only service for `state`.
pub fn lock_instance(state: &Path) -> Result<InstanceLock, LocationError> {
    let fd = open_lock(state)?;
    take_lock(fd, state)
}

/// Takes the locks that make this process the only service for its state
/// directory and the only one for its socket. When both are one directory,
/// under one path or two, its one lock covers both: a second lock on
/// another open of the same file would refuse this very process.
pub fn lock_service(locations: &Locations) -> Result<Vec<InstanceLock>, LocationError> {
    let state = lock_instance(&locations.state)?;
    let runtime = open_lock(&locations.runtime)?;
    let stat = |fd: &std::os::fd::OwnedFd, dir: &Path| {
        rustix::fs::fstat(fd)
            .map(|s| (s.st_dev, s.st_ino))
            .map_err(|e| LocationError::io(&dir.join("service.lock"), e))
    };
    if stat(&state.0, &locations.state)? == stat(&runtime, &locations.runtime)? {
        return Ok(vec![state]);
    }
    Ok(vec![state, take_lock(runtime, &locations.runtime)?])
}

fn open_lock(dir: &Path) -> Result<std::os::fd::OwnedFd, LocationError> {
    let path = dir.join("service.lock");
    rustix::fs::open(
        &path,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(|e| LocationError::io(&path, e))
}

fn take_lock(fd: std::os::fd::OwnedFd, dir: &Path) -> Result<InstanceLock, LocationError> {
    match rustix::fs::flock(&fd, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(InstanceLock(fd)),
        Err(rustix::io::Errno::WOULDBLOCK) => Err(LocationError::AlreadyRunning(dir.to_owned())),
        Err(e) => Err(LocationError::io(&dir.join("service.lock"), e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hyakume-paths-{name}-{}-{:x}",
            std::process::id(),
            rand_core::RngCore::next_u64(&mut rand_core::OsRng)
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[test]
    fn a_missing_directory_is_created_private() {
        let base = temp("create");
        let dir = base.join("a/b");
        private_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o700);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_symlink_or_an_open_directory_is_refused() {
        let base = temp("refuse");
        let real = base.join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            private_dir(&real),
            Err(LocationError::OpenToOthers(_))
        ));
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(matches!(
            private_dir(&link),
            Err(LocationError::NotDirectory(_))
        ));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn folders_under_a_linked_ancestor_are_kept_by_their_real_path() {
        let base = temp("ancestor");
        let real = base.join("real");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, base.join("link")).unwrap();
        let through_link = Locations {
            state: base.join("link/state"),
            runtime: base.join("link/run"),
        };
        let prepared = through_link.prepare().unwrap();
        assert_eq!(prepared.state, real.join("state"));
        // Spools and received files are reopened without following links,
        // as cleanup and receiving do.
        assert!(crate::files::destination::Destination::qualify(&prepared.spool()).is_ok());
        assert!(crate::files::destination::Destination::qualify(&prepared.app_storage()).is_ok());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_second_lock_on_the_same_state_is_refused_until_the_first_ends() {
        let base = temp("lock");
        let first = lock_instance(&base).unwrap();
        assert!(matches!(
            lock_instance(&base),
            Err(LocationError::AlreadyRunning(_))
        ));
        drop(first);
        assert!(lock_instance(&base).is_ok());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn one_folder_for_state_and_socket_is_locked_once() {
        let base = temp("one");
        // The same folder by a second path, as a bind mount would give.
        let link = base.with_extension("link");
        std::os::unix::fs::symlink(&base, &link).unwrap();
        let one = Locations {
            state: base.clone(),
            runtime: link.clone(),
        };
        let locks = lock_service(&one).unwrap();
        // It still keeps out a second service.
        assert!(matches!(
            lock_service(&one),
            Err(LocationError::AlreadyRunning(_))
        ));
        drop(locks);
        assert!(lock_service(&one).is_ok());
        std::fs::remove_file(link).unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }
}
