//! Durable state in one SQLite database, written only by [`crate::state::State`]
//! inside its lock, so writes follow the decisions they record. `synchronous
//! = FULL` makes a committed write durable when the call returns. Every row
//! is checked on load and a bad one is an error, never repaired; nothing
//! secret is stored.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::clock::ClockReading;
use crate::display::PeerText;
use crate::files::names::{OutputName, partial_name, spool_name};
use crate::identity::DeviceId;
use crate::ids::TransferId;
use crate::limits::{MAX_PEER_NAME_BYTES, MAX_TRUST_RECORDS};
use crate::offer::Offer;
use crate::protocol::{self, Message};
use crate::state::{MAX_KEPT_TRANSFERS, MAX_REMEMBERED_OFFERS, PartialFile, Place, TransferInfo};
use crate::transfer::{ItemState, OfferState, Transfer};
use crate::trust::{Authority, Epoch, LocalAlias, TrustRecord, TrustState};

/// The schema this build writes.
pub const SCHEMA_VERSION: u32 = 1;

const SCHEMA_V1: &str = "
CREATE TABLE installation (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    installation BLOB NOT NULL CHECK (length(installation) = 16),
    device BLOB NOT NULL CHECK (length(device) = 32),
    store TEXT NOT NULL,
    name BLOB NOT NULL,
    ready INTEGER NOT NULL CHECK (ready IN (0, 1))
) STRICT;
CREATE TABLE counters (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    next_epoch INTEGER NOT NULL CHECK (next_epoch >= 0),
    clock_ms INTEGER NOT NULL CHECK (clock_ms >= 0),
    clock_wall_ms INTEGER NOT NULL CHECK (clock_wall_ms >= 0)
) STRICT;
INSERT INTO counters (id, next_epoch, clock_ms, clock_wall_ms) VALUES (1, 0, 0, 0);
CREATE TABLE trust (
    device BLOB PRIMARY KEY CHECK (length(device) = 32),
    alias TEXT NOT NULL,
    declared_name BLOB NOT NULL,
    epoch INTEGER NOT NULL CHECK (epoch >= 0),
    revoked INTEGER NOT NULL CHECK (revoked IN (0, 1))
) STRICT;
CREATE TABLE offered (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    transfer BLOB NOT NULL UNIQUE CHECK (length(transfer) = 16)
) STRICT;
CREATE TABLE transfers (
    id BLOB PRIMARY KEY CHECK (length(id) = 16),
    outgoing INTEGER NOT NULL CHECK (outgoing IN (0, 1)),
    peer BLOB NOT NULL CHECK (length(peer) = 32),
    epoch INTEGER NOT NULL CHECK (epoch >= 0),
    offer BLOB NOT NULL,
    offer_state TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    expires_ms INTEGER NOT NULL,
    place_dir BLOB,
    place_dev INTEGER,
    place_ino INTEGER,
    finished INTEGER
) STRICT;
CREATE TABLE items (
    transfer BLOB NOT NULL REFERENCES transfers (id) ON DELETE CASCADE,
    item INTEGER NOT NULL CHECK (item >= 0),
    state TEXT NOT NULL,
    output BLOB,
    source BLOB,
    PRIMARY KEY (transfer, item)
) STRICT;
CREATE TABLE files (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL CHECK (kind IN ('spool', 'partial')),
    transfer BLOB NOT NULL CHECK (length(transfer) = 16),
    item INTEGER NOT NULL CHECK (item >= 0),
    dir BLOB NOT NULL,
    dir_dev INTEGER NOT NULL,
    dir_ino INTEGER NOT NULL,
    name BLOB NOT NULL,
    dev INTEGER,
    ino INTEGER,
    bytes INTEGER NOT NULL CHECK (bytes >= 0),
    state TEXT NOT NULL CHECK (state IN ('reserved', 'live', 'delete', 'retained')),
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    delete_since INTEGER,
    next_attempt INTEGER,
    last_error TEXT,
    UNIQUE (kind, transfer, item)
) STRICT;
";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("the state database failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("the state database was written by a newer Hyakume (schema {0})")]
    NewerSchema(u32),
    #[error("the state database is damaged: {0}")]
    Corrupt(String),
    #[error("cannot restrict the state database to its owner: {0}")]
    Permissions(std::io::Error),
}

fn corrupt(what: impl Into<String>) -> StoreError {
    StoreError::Corrupt(what.into())
}

/// Which protected store holds the device key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StoreKind {
    SecretService,
    KeyFile,
}

impl StoreKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::SecretService => "secret-service",
            Self::KeyFile => "key-file",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "secret-service" => Some(Self::SecretService),
            "key-file" => Some(Self::KeyFile),
            _ => None,
        }
    }
}

/// This device's installation: which identity and which store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installation {
    /// Random; it binds the protected key bundle to this database.
    pub id: [u8; 16],
    pub device: DeviceId,
    pub store: StoreKind,
    /// The name this device declares to peers.
    pub name: String,
    /// False while `init` has not finished writing the key.
    pub ready: bool,
}

/// What a file row stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    /// A sealed sender spool.
    Spool,
    /// A private partial file of a received item.
    Partial,
}

impl FileKind {
    fn name(self) -> &'static str {
        match self {
            Self::Spool => "spool",
            Self::Partial => "partial",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "spool" => Some(Self::Spool),
            "partial" => Some(Self::Partial),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileState {
    /// Bytes are charged; the file may not exist yet.
    Reserved,
    /// The file exists with the recorded identity.
    Live,
    /// The file must be deleted; still charged until it is.
    Delete,
    /// Deleting kept failing for the whole retry window. Still charged.
    Retained,
}

impl FileState {
    fn name(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Live => "live",
            Self::Delete => "delete",
            Self::Retained => "retained",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "reserved" => Some(Self::Reserved),
            "live" => Some(Self::Live),
            "delete" => Some(Self::Delete),
            "retained" => Some(Self::Retained),
            _ => None,
        }
    }
}

/// One file Hyakume owns, as the ledger records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedFile {
    pub id: i64,
    pub kind: FileKind,
    pub transfer: TransferId,
    pub item: usize,
    /// The directory holding the file, by path and identity.
    pub dir: Place,
    pub name: Vec<u8>,
    /// Device and inode, once the file exists.
    pub identity: Option<(u64, u64)>,
    pub bytes: u64,
    pub state: FileState,
    pub attempts: u32,
    pub delete_since_ms: Option<u64>,
    pub next_attempt_ms: Option<u64>,
    pub last_error: Option<String>,
}

/// A file to add to the ledger.
pub struct NewFile<'a> {
    pub kind: FileKind,
    pub transfer: TransferId,
    pub item: usize,
    pub dir: &'a Place,
    pub name: &'a [u8],
    pub identity: Option<(u64, u64)>,
    pub bytes: u64,
}

/// One saved transfer.
pub struct SavedTransfer {
    pub transfer: Transfer,
    pub info: TransferInfo,
    /// Order in which it finished; `None` while unfinished.
    pub finished: Option<u64>,
}

/// Everything loaded at startup.
pub struct Loaded {
    pub trust: Vec<TrustRecord>,
    pub next_epoch: u64,
    /// The latest lifetime-clock reading saved.
    pub clock: ClockReading,
    /// Oldest first.
    pub offered: Vec<TransferId>,
    /// Unfinished first, then finished in the order they finished.
    pub transfers: Vec<SavedTransfer>,
}

pub struct Store {
    conn: Connection,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Store")
    }
}

// SQLite stores 64-bit signed integers. Device numbers, inodes, and byte
// counts are unsigned; they are stored by reinterpreting the bits, which
// round-trips every value.
fn to_sql(v: u64) -> i64 {
    v as i64
}

fn from_sql(v: i64) -> u64 {
    v as u64
}

impl Store {
    /// Opens or creates the database at `path` and brings its schema up to
    /// date. The file is readable only by its owner.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        use std::os::unix::fs::PermissionsExt;
        let conn = Connection::open(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(StoreError::Permissions)?;
        Self::setup(conn)
    }

    /// A database that lives in memory, for tests and short-lived nodes.
    pub fn memory() -> Result<Self, StoreError> {
        Self::setup(Connection::open_in_memory()?)
    }

    fn setup(conn: Connection) -> Result<Self, StoreError> {
        conn.pragma_update(None, "journal_mode", "DELETE")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if check != "ok" {
            return Err(corrupt("SQLite's integrity check failed"));
        }
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&mut self) -> Result<(), StoreError> {
        let version: u32 = self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(StoreError::NewerSchema(version));
        }
        if version == 0 {
            let tx = self.conn.transaction()?;
            tx.execute_batch(SCHEMA_V1)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
        }
        Ok(())
    }

    // ---- Installation ----

    pub fn installation(&self) -> Result<Option<Installation>, StoreError> {
        let row = self
            .conn
            .query_row(
                "SELECT installation, device, store, name, ready FROM installation",
                [],
                |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Vec<u8>>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((id, device, store, name, ready)) = row else {
            return Ok(None);
        };
        Ok(Some(Installation {
            id: id.try_into().map_err(|_| corrupt("installation ID"))?,
            device: device_id(&device)?,
            store: StoreKind::from_name(&store).ok_or_else(|| corrupt("store kind"))?,
            // The rules `init` applied, so a name changed on disk can
            // carry no terminal controls.
            name: String::from_utf8(name)
                .ok()
                .and_then(|n| LocalAlias::new(&n).ok())
                .map(|n| n.as_str().to_owned())
                .ok_or_else(|| corrupt("device name"))?,
            ready: ready == 1,
        }))
    }

    /// Records a new installation before its key is written.
    pub fn begin_installation(&mut self, inst: &Installation) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO installation (id, installation, device, store, name, ready)
             VALUES (1, ?1, ?2, ?3, ?4, 0)",
            params![
                &inst.id[..],
                &inst.device.as_bytes()[..],
                inst.store.name(),
                inst.name.as_bytes()
            ],
        )?;
        Ok(())
    }

    /// The key is written: the installation is complete.
    pub fn finish_installation(&mut self) -> Result<(), StoreError> {
        self.conn
            .execute("UPDATE installation SET ready = 1 WHERE id = 1", [])?;
        Ok(())
    }

    /// Forgets the installation and everything authorized under it: trust,
    /// transfers, and the offer window. Files Hyakume owns stay in the
    /// ledger, marked for deletion, so their bytes stay charged until they
    /// are gone. The epoch counter stays, so no epoch is ever reused.
    pub fn forget_installation(&mut self, now_ms: u64) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM installation", [])?;
        tx.execute("DELETE FROM trust", [])?;
        tx.execute("DELETE FROM offered", [])?;
        tx.execute("DELETE FROM transfers", [])?;
        tx.execute(
            "UPDATE files SET state = 'delete', delete_since = ?1, next_attempt = ?1, attempts = 0
             WHERE state IN ('reserved', 'live')",
            params![to_sql(now_ms)],
        )?;
        tx.commit()?;
        Ok(())
    }

    // ---- Trust ----

    /// Saves one trust record and the epoch counter together, so a counter
    /// lower than a saved epoch can never be read back.
    pub fn save_trust(&mut self, record: &TrustRecord, next_epoch: u64) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        let (epoch, revoked) = match record.state {
            TrustState::Trusted { epoch } => (epoch.value(), 0),
            // A revoked record keeps no usable epoch; the counter already
            // moved past every epoch it ever had.
            TrustState::Revoked => (0, 1),
        };
        tx.execute(
            "INSERT OR REPLACE INTO trust (device, alias, declared_name, epoch, revoked)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                &record.device.as_bytes()[..],
                record.alias.as_str(),
                record.declared_name.as_bytes(),
                to_sql(epoch),
                revoked
            ],
        )?;
        set_next_epoch(&tx, next_epoch)?;
        tx.commit()?;
        Ok(())
    }

    // ---- Offered window ----

    /// Remembers an offered transfer ID, forgetting the oldest beyond
    /// [`MAX_REMEMBERED_OFFERS`].
    pub fn push_offered(&mut self, id: TransferId) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO offered (transfer) VALUES (?1)",
            params![&id.as_bytes()[..]],
        )?;
        tx.execute(
            "DELETE FROM offered WHERE seq <= (SELECT max(seq) FROM offered) - ?1",
            params![MAX_REMEMBERED_OFFERS as i64],
        )?;
        tx.commit()?;
        Ok(())
    }

    // ---- Transfers ----

    pub fn insert_transfer(&mut self, t: &Transfer, info: &TransferInfo) -> Result<(), StoreError> {
        let offer = protocol::encode_payload(&Message::Offer(info.offer.fields()));
        let tx = self.conn.transaction()?;
        let place = info.place.as_ref();
        tx.execute(
            "INSERT INTO transfers (id, outgoing, peer, epoch, offer, offer_state, created_ms,
                                    expires_ms, place_dir, place_dev, place_ino)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                &t.id().as_bytes()[..],
                info.outgoing,
                &t.peer().as_bytes()[..],
                to_sql(t.authority().epoch().value()),
                offer,
                t.offer_state().name(),
                to_sql(info.created_ms),
                to_sql(info.expires_ms),
                place.map(|p| p.dir.clone()),
                place.map(|p| to_sql(p.dev)),
                place.map(|p| to_sql(p.ino)),
            ],
        )?;
        for (n, state) in t.items().iter().enumerate() {
            tx.execute(
                "INSERT INTO items (transfer, item, state, output, source)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    &t.id().as_bytes()[..],
                    n as i64,
                    stored(*state).name(),
                    info.outputs.get(n).cloned().flatten(),
                    info.sources.get(n).cloned().flatten(),
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Saves the offer state and each item's state, and marks the files of
    /// items that ended for deletion. Items that are only moving bytes are
    /// saved as queued: after a restart they are interrupted, whatever step
    /// they had reached.
    pub fn save_transfer(&mut self, t: &Transfer, now_ms: u64) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        save_states(&tx, t, now_ms)?;
        tx.commit()?;
        Ok(())
    }

    /// Accepts a received offer into `place`: the offer state, the output
    /// names, and a reserved partial file per item, in one write.
    pub fn accept_into(
        &mut self,
        t: &Transfer,
        place: &Place,
        partials: &[PartialFile],
        now_ms: u64,
    ) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE transfers SET place_dir = ?2, place_dev = ?3, place_ino = ?4 WHERE id = ?1",
            params![
                &t.id().as_bytes()[..],
                place.dir,
                to_sql(place.dev),
                to_sql(place.ino)
            ],
        )?;
        for p in partials {
            tx.execute(
                "UPDATE items SET output = ?3 WHERE transfer = ?1 AND item = ?2",
                params![&t.id().as_bytes()[..], p.item as i64, p.output],
            )?;
            add_file(
                &tx,
                &NewFile {
                    kind: FileKind::Partial,
                    transfer: t.id(),
                    item: p.item,
                    dir: place,
                    name: &p.partial,
                    identity: None,
                    bytes: p.bytes,
                },
            )?;
        }
        save_states(&tx, t, now_ms)?;
        tx.commit()?;
        Ok(())
    }

    /// The commit record for one published item: its state, and its
    /// partial file leaves the ledger because it became the published file.
    pub fn publish(&mut self, t: &Transfer, item: usize, now_ms: u64) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM files WHERE kind = 'partial' AND transfer = ?1 AND item = ?2",
            params![&t.id().as_bytes()[..], item as i64],
        )?;
        save_states(&tx, t, now_ms)?;
        tx.commit()?;
        Ok(())
    }

    pub fn finish_transfer(&mut self, id: TransferId, order: u64) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE transfers SET finished = ?2 WHERE id = ?1",
            params![&id.as_bytes()[..], to_sql(order)],
        )?;
        Ok(())
    }

    pub fn delete_transfer(&mut self, id: TransferId) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM transfers WHERE id = ?1",
            params![&id.as_bytes()[..]],
        )?;
        Ok(())
    }

    // ---- Files ----

    pub fn add_file(&mut self, file: &NewFile<'_>) -> Result<i64, StoreError> {
        let tx = self.conn.transaction()?;
        let id = add_file(&tx, file)?;
        tx.commit()?;
        Ok(id)
    }

    /// Records a file that must go, such as input whose deletion just
    /// failed. It stays charged until cleanup deletes it.
    pub fn add_file_to_delete(
        &mut self,
        file: &NewFile<'_>,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        let id = add_file(&tx, file)?;
        tx.execute(
            "UPDATE files SET state = 'delete', delete_since = ?2, next_attempt = ?2 WHERE id = ?1",
            params![id, to_sql(now_ms)],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// A reserved file now exists with this device and inode. False when
    /// the reservation is gone, as after a cancel: nothing then records the
    /// file.
    pub fn file_created(
        &mut self,
        kind: FileKind,
        transfer: TransferId,
        item: usize,
        identity: (u64, u64),
    ) -> Result<bool, StoreError> {
        let updated = self.conn.execute(
            "UPDATE files SET state = 'live', dev = ?4, ino = ?5
             WHERE kind = ?1 AND transfer = ?2 AND item = ?3 AND state = 'reserved'",
            params![
                kind.name(),
                &transfer.as_bytes()[..],
                item as i64,
                to_sql(identity.0),
                to_sql(identity.1)
            ],
        )?;
        Ok(updated == 1)
    }

    /// Marks every file of `transfer` for deletion.
    pub fn delete_files_of(&mut self, transfer: TransferId, now_ms: u64) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE files SET state = 'delete', delete_since = ?2, next_attempt = ?2
             WHERE transfer = ?1 AND state IN ('reserved', 'live')",
            params![&transfer.as_bytes()[..], to_sql(now_ms)],
        )?;
        Ok(())
    }

    /// Marks one file for deletion.
    pub fn delete_file(&mut self, id: i64, now_ms: u64) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE files SET state = 'delete', delete_since = ?2, next_attempt = ?2
             WHERE id = ?1 AND state IN ('reserved', 'live')",
            params![id, to_sql(now_ms)],
        )?;
        Ok(())
    }

    pub fn files(&self) -> Result<Vec<OwnedFile>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, transfer, item, dir, dir_dev, dir_ino, name, dev, ino, bytes,
                    state, attempts, delete_since, next_attempt, last_error
             FROM files ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(RawFile {
                id: r.get(0)?,
                kind: r.get(1)?,
                transfer: r.get(2)?,
                item: r.get(3)?,
                dir: r.get(4)?,
                dir_dev: r.get(5)?,
                dir_ino: r.get(6)?,
                name: r.get(7)?,
                dev: r.get(8)?,
                ino: r.get(9)?,
                bytes: r.get(10)?,
                state: r.get(11)?,
                attempts: r.get(12)?,
                delete_since: r.get(13)?,
                next_attempt: r.get(14)?,
                last_error: r.get(15)?,
            })
        })?;
        let mut files = Vec::new();
        for row in rows {
            files.push(row?.validate()?);
        }
        Ok(files)
    }

    /// Files marked for deletion whose next attempt is due at `now_ms`.
    pub fn files_due(&self, now_ms: u64) -> Result<Vec<OwnedFile>, StoreError> {
        Ok(self
            .files()?
            .into_iter()
            .filter(|f| {
                f.state == FileState::Delete && f.next_attempt_ms.is_none_or(|t| t <= now_ms)
            })
            .collect())
    }

    /// Bytes charged to each kind of file: spools, then partials.
    pub fn charged(&self) -> Result<(u64, u64), StoreError> {
        let sum = |kind: &str| -> Result<u64, StoreError> {
            let total: i64 = self.conn.query_row(
                "SELECT coalesce(sum(bytes), 0) FROM files WHERE kind = ?1",
                params![kind],
                |r| r.get(0),
            )?;
            u64::try_from(total).map_err(|_| corrupt("charged bytes"))
        };
        Ok((sum("spool")?, sum("partial")?))
    }

    /// The file is gone: its row leaves the ledger and its bytes are free.
    /// Row IDs are never reused, and only a row still marked for deletion
    /// changes, so a late or repeated result cannot touch another file.
    pub fn file_deleted(&mut self, id: i64) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM files WHERE id = ?1 AND state = 'delete'",
            params![id],
        )?;
        Ok(())
    }

    /// Deleting failed again. After the retry window the file is retained
    /// and automatic retries stop.
    pub fn deletion_failed(
        &mut self,
        id: i64,
        error: &str,
        next_attempt_ms: Option<u64>,
    ) -> Result<(), StoreError> {
        let state = if next_attempt_ms.is_some() {
            FileState::Delete
        } else {
            FileState::Retained
        };
        self.conn.execute(
            "UPDATE files SET attempts = attempts + 1, last_error = ?2, next_attempt = ?3,
                              state = ?4
             WHERE id = ?1 AND state = 'delete'",
            params![id, error, next_attempt_ms.map(to_sql), state.name()],
        )?;
        Ok(())
    }

    /// Starts a new retry window for every retained file.
    pub fn retry_retained(&mut self, now_ms: u64) -> Result<usize, StoreError> {
        Ok(self.conn.execute(
            "UPDATE files SET state = 'delete', attempts = 0, delete_since = ?1, next_attempt = ?1
             WHERE state = 'retained'",
            params![to_sql(now_ms)],
        )?)
    }

    /// Makes every later write fail, as a failing disk would.
    #[cfg(test)]
    pub(crate) fn refuse_writes(&mut self) {
        let _ = self.conn.pragma_update(None, "query_only", true);
    }

    /// Ends [`Store::refuse_writes`], as a disk that recovered would.
    #[cfg(test)]
    pub(crate) fn allow_writes(&mut self) {
        let _ = self.conn.pragma_update(None, "query_only", false);
    }

    /// Saves a lifetime-clock reading. Its time never moves back.
    pub fn save_clock(&mut self, reading: ClockReading) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE counters SET clock_ms = ?1, clock_wall_ms = ?2 WHERE id = 1 AND clock_ms <= ?1",
            params![to_sql(reading.lifetime_ms), to_sql(reading.wall_ms)],
        )?;
        Ok(())
    }

    // ---- Loading ----

    /// Loads trust, the offer window, and transfers, checking every row.
    pub fn load(&self) -> Result<Loaded, StoreError> {
        let (next_epoch, clock_ms, clock_wall_ms): (i64, i64, i64) = self.conn.query_row(
            "SELECT next_epoch, clock_ms, clock_wall_ms FROM counters WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Ok(Loaded {
            trust: self.load_trust()?,
            next_epoch: u64::try_from(next_epoch).map_err(|_| corrupt("epoch counter"))?,
            clock: ClockReading {
                lifetime_ms: u64::try_from(clock_ms).map_err(|_| corrupt("clock"))?,
                wall_ms: u64::try_from(clock_wall_ms).map_err(|_| corrupt("clock"))?,
            },
            offered: self.load_offered()?,
            transfers: self.load_transfers()?,
        })
    }

    fn load_trust(&self) -> Result<Vec<TrustRecord>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT device, alias, declared_name, epoch, revoked FROM trust")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (device, alias, name, epoch, revoked) = row?;
            if out.len() == MAX_TRUST_RECORDS {
                return Err(corrupt("too many trust records"));
            }
            out.push(TrustRecord {
                device: device_id(&device)?,
                alias: LocalAlias::new(&alias).map_err(|_| corrupt("alias"))?,
                declared_name: PeerText::new(name, MAX_PEER_NAME_BYTES)
                    .map_err(|_| corrupt("declared name"))?,
                state: match revoked {
                    0 => TrustState::Trusted {
                        epoch: Epoch::restore(from_sql(epoch)),
                    },
                    _ => TrustState::Revoked,
                },
            });
        }
        Ok(out)
    }

    fn load_offered(&self) -> Result<Vec<TransferId>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT transfer FROM offered ORDER BY seq")?;
        let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(transfer_id(&row?)?);
        }
        if out.len() > MAX_REMEMBERED_OFFERS {
            return Err(corrupt("too many remembered offers"));
        }
        Ok(out)
    }

    fn load_transfers(&self) -> Result<Vec<SavedTransfer>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, outgoing, peer, epoch, offer, offer_state, created_ms, expires_ms,
                    place_dir, place_dev, place_ino, finished
             FROM transfers ORDER BY finished IS NOT NULL, finished",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(RawTransfer {
                id: r.get(0)?,
                outgoing: r.get(1)?,
                peer: r.get(2)?,
                epoch: r.get(3)?,
                offer: r.get(4)?,
                offer_state: r.get(5)?,
                created_ms: r.get(6)?,
                expires_ms: r.get(7)?,
                place_dir: r.get(8)?,
                place_dev: r.get(9)?,
                place_ino: r.get(10)?,
                finished: r.get(11)?,
            })
        })?;
        let mut raws = Vec::new();
        for row in rows {
            raws.push(row?);
            if raws.len() > 2 * MAX_KEPT_TRANSFERS {
                return Err(corrupt("too many transfers"));
            }
        }

        let mut items = self.conn.prepare(
            "SELECT item, state, output, source FROM items WHERE transfer = ?1 ORDER BY item",
        )?;
        let mut out = Vec::new();
        for raw in raws {
            let rows = items.query_map(params![raw.id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<Vec<u8>>>(2)?,
                    r.get::<_, Option<Vec<u8>>>(3)?,
                ))
            })?;
            let mut states = Vec::new();
            let mut outputs = Vec::new();
            let mut sources = Vec::new();
            for (n, row) in rows.enumerate() {
                let (item, state, output, source) = row?;
                if item != n as i64 {
                    return Err(corrupt("item numbers"));
                }
                // Recovery looks for a published item under this name.
                if output.as_ref().is_some_and(|o| OutputName::new(o).is_err()) {
                    return Err(corrupt("output name"));
                }
                states.push(ItemState::from_name(&state).ok_or_else(|| corrupt("item state"))?);
                outputs.push(output);
                sources.push(source);
            }
            out.push(raw.validate(states, outputs, sources)?);
        }
        Ok(out)
    }
}

fn set_next_epoch(tx: &Transaction<'_>, next_epoch: u64) -> Result<(), StoreError> {
    let changed = tx.execute(
        "UPDATE counters SET next_epoch = ?1 WHERE id = 1 AND next_epoch <= ?1",
        params![to_sql(next_epoch)],
    )?;
    if changed != 1 {
        return Err(corrupt("the epoch counter would move backward"));
    }
    Ok(())
}

/// What a saved item state means after a restart: an item that was only
/// moving bytes was interrupted, so it is saved as queued.
pub fn stored(state: ItemState) -> ItemState {
    match state {
        ItemState::Transferring | ItemState::Verifying | ItemState::Resuming => ItemState::Queued,
        other => other,
    }
}

fn save_states(tx: &Transaction<'_>, t: &Transfer, now_ms: u64) -> Result<(), StoreError> {
    let id = t.id();
    let id = &id.as_bytes()[..];
    tx.execute(
        "UPDATE transfers SET offer_state = ?2 WHERE id = ?1",
        params![id, t.offer_state().name()],
    )?;
    for (n, state) in t.items().iter().enumerate() {
        tx.execute(
            "UPDATE items SET state = ?3 WHERE transfer = ?1 AND item = ?2",
            params![id, n as i64, stored(*state).name()],
        )?;
        if state.is_terminal() {
            tx.execute(
                "UPDATE files SET state = 'delete', delete_since = ?3, next_attempt = ?3
                 WHERE transfer = ?1 AND item = ?2 AND state IN ('reserved', 'live')",
                params![id, n as i64, to_sql(now_ms)],
            )?;
        }
    }
    Ok(())
}

fn add_file(tx: &Transaction<'_>, file: &NewFile<'_>) -> Result<i64, StoreError> {
    let state = if file.identity.is_some() {
        FileState::Live
    } else {
        FileState::Reserved
    };
    tx.execute(
        "INSERT INTO files (kind, transfer, item, dir, dir_dev, dir_ino, name, dev, ino, bytes, state)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            file.kind.name(),
            &file.transfer.as_bytes()[..],
            file.item as i64,
            file.dir.dir,
            to_sql(file.dir.dev),
            to_sql(file.dir.ino),
            file.name,
            file.identity.map(|i| to_sql(i.0)),
            file.identity.map(|i| to_sql(i.1)),
            to_sql(file.bytes),
            state.name(),
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

fn device_id(bytes: &[u8]) -> Result<DeviceId, StoreError> {
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| corrupt("device ID"))?;
    DeviceId::from_bytes(&bytes).ok_or_else(|| corrupt("device ID"))
}

fn transfer_id(bytes: &[u8]) -> Result<TransferId, StoreError> {
    let bytes: [u8; 16] = bytes.try_into().map_err(|_| corrupt("transfer ID"))?;
    Ok(TransferId::from_bytes(bytes))
}

struct RawTransfer {
    id: Vec<u8>,
    outgoing: bool,
    peer: Vec<u8>,
    epoch: i64,
    offer: Vec<u8>,
    offer_state: String,
    created_ms: i64,
    expires_ms: i64,
    place_dir: Option<Vec<u8>>,
    place_dev: Option<i64>,
    place_ino: Option<i64>,
    finished: Option<i64>,
}

impl RawTransfer {
    fn validate(
        self,
        states: Vec<ItemState>,
        outputs: Vec<Option<Vec<u8>>>,
        sources: Vec<Option<Vec<u8>>>,
    ) -> Result<SavedTransfer, StoreError> {
        let id = transfer_id(&self.id)?;
        let fields = match protocol::decode_payload(&self.offer) {
            Ok(Message::Offer(fields)) => fields,
            _ => return Err(corrupt("saved offer")),
        };
        let offer = Offer::validate(fields).map_err(|_| corrupt("saved offer"))?;
        if offer.transfer() != id || offer.items().len() != states.len() {
            return Err(corrupt("saved offer does not match its transfer"));
        }
        let authority =
            Authority::restore(device_id(&self.peer)?, Epoch::restore(from_sql(self.epoch)));
        let offer_state =
            OfferState::from_name(&self.offer_state).ok_or_else(|| corrupt("offer state"))?;
        let transfer = Transfer::restore(id, authority, offer_state, states)
            .map_err(|_| corrupt("transfer states"))?;
        let place = match (self.place_dir, self.place_dev, self.place_ino) {
            (Some(dir), Some(dev), Some(ino)) => Some(Place {
                dir,
                dev: from_sql(dev),
                ino: from_sql(ino),
            }),
            (None, None, None) => None,
            _ => return Err(corrupt("receive place")),
        };
        Ok(SavedTransfer {
            transfer,
            info: TransferInfo {
                outgoing: self.outgoing,
                offer,
                created_ms: from_sql(self.created_ms),
                expires_ms: from_sql(self.expires_ms),
                place,
                outputs,
                sources,
            },
            finished: self.finished.map(from_sql),
        })
    }
}

struct RawFile {
    id: i64,
    kind: String,
    transfer: Vec<u8>,
    item: i64,
    dir: Vec<u8>,
    dir_dev: i64,
    dir_ino: i64,
    name: Vec<u8>,
    dev: Option<i64>,
    ino: Option<i64>,
    bytes: i64,
    state: String,
    attempts: i64,
    delete_since: Option<i64>,
    next_attempt: Option<i64>,
    last_error: Option<String>,
}

impl RawFile {
    fn validate(self) -> Result<OwnedFile, StoreError> {
        let kind = FileKind::from_name(&self.kind).ok_or_else(|| corrupt("file kind"))?;
        let transfer = transfer_id(&self.transfer)?;
        let item = usize::try_from(self.item).map_err(|_| corrupt("file item"))?;
        // Cleanup deletes this name, so it must be the very name Hyakume
        // gives such a file, never a path that could reach elsewhere.
        let expected = match kind {
            FileKind::Partial => partial_name(transfer, item),
            FileKind::Spool if item == 0 => spool_name(transfer),
            FileKind::Spool => return Err(corrupt("file item")),
        };
        if self.name != expected.as_bytes() {
            return Err(corrupt("file name"));
        }

        Ok(OwnedFile {
            id: self.id,
            kind,
            transfer,
            item,
            dir: Place {
                dir: self.dir,
                dev: from_sql(self.dir_dev),
                ino: from_sql(self.dir_ino),
            },
            name: self.name,
            identity: match (self.dev, self.ino) {
                (Some(dev), Some(ino)) => Some((from_sql(dev), from_sql(ino))),
                (None, None) => None,
                _ => return Err(corrupt("file identity")),
            },
            bytes: u64::try_from(self.bytes).map_err(|_| corrupt("file size"))?,
            state: FileState::from_name(&self.state).ok_or_else(|| corrupt("file state"))?,
            attempts: u32::try_from(self.attempts).map_err(|_| corrupt("attempts"))?,
            delete_since_ms: self.delete_since.map(from_sql),
            next_attempt_ms: self.next_attempt.map(from_sql),
            last_error: self.last_error,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::PeerText;
    use crate::identity::DeviceKey;
    use crate::state::{PartialFile, State};
    use crate::transfer::{CancelCause, ItemEvent};
    use crate::trust::VerifiedPeer;
    use rand_core::{OsRng, RngCore};
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use zeroize::Zeroizing;

    struct Dir(PathBuf);

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn dir() -> Dir {
        let path = std::env::temp_dir().join(format!(
            "hyakume-store-{}-{:x}",
            std::process::id(),
            OsRng.next_u64()
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Dir(path)
    }

    fn device(seed: u8) -> DeviceId {
        DeviceKey::from_seed(&Zeroizing::new([seed; 32])).id()
    }

    fn trusted(state: &mut State, seed: u8, alias: &str) -> Authority {
        let reservation = state
            .trust
            .reserve(device(seed), LocalAlias::new(alias).unwrap())
            .unwrap();
        state
            .trust_peer(
                VerifiedPeer::new(device(seed), PeerText::default()),
                reservation,
            )
            .unwrap()
    }

    #[test]
    fn a_restart_reproduces_trust_revocation_transfers_and_charged_bytes() {
        let dir = dir();
        let db = dir.0.join("hyakume.db");
        let place = Place {
            dir: b"/in".to_vec(),
            dev: 1,
            ino: 2,
        };
        let (kept, revoked_id) = {
            let mut state = State::load(Store::open(&db).unwrap()).unwrap();
            let kept = trusted(&mut state, 1, "laptop");
            trusted(&mut state, 2, "phone");
            state.revoke(&device(2)).unwrap();
            let id = TransferId::from_bytes([7; 16]);
            let offer = Offer::for_files(id, &[("a", b"one"), ("b", b"two")]).unwrap();
            let t = Transfer::offered(id, kept, 2).unwrap();
            state
                .insert_transfer(t, TransferInfo::new(false, offer, 1_000))
                .unwrap();
            let partials = [0, 1].map(|item| PartialFile {
                item,
                output: format!("{item}").into_bytes(),
                partial: partial_name(id, item).into_bytes(),
                bytes: 3,
            });
            state.accept(&id, Some((&place, &partials))).unwrap();
            state.update_transfer(&id, |t| {
                t.apply(0, ItemEvent::Start).unwrap();
                t.apply(0, ItemEvent::Cancel(CancelCause::Receiver))
                    .unwrap();
            });
            (kept, id)
        };
        let mut state = State::load(Store::open(&db).unwrap()).unwrap();
        assert_eq!(state.trust().check(&kept), Ok(()));
        assert_eq!(
            state.trust().authorize(&device(2)),
            Err(crate::trust::TrustError::Revoked)
        );
        let t = state.transfer(&revoked_id).unwrap();
        assert_eq!(t.offer_state(), OfferState::Accepted);
        assert_eq!(
            t.items(),
            [
                ItemState::Cancelled(CancelCause::Receiver),
                ItemState::Queued
            ]
        );
        assert_eq!(state.info(&revoked_id).unwrap().place, Some(place));
        // The cancelled item's partial is marked for deletion but stays
        // charged until it is gone; the other is still reserved.
        assert_eq!(state.store().charged().unwrap(), (0, 6));
        let states: Vec<FileState> = state
            .store()
            .files()
            .unwrap()
            .iter()
            .map(|f| f.state)
            .collect();
        assert_eq!(states, [FileState::Delete, FileState::Reserved]);
    }

    #[test]
    fn transfers_that_ended_just_before_a_crash_still_make_room_after_it() {
        let dir = dir();
        let db = dir.0.join("hyakume.db");
        {
            let mut state = State::load(Store::open(&db).unwrap()).unwrap();
            let peer = trusted(&mut state, 1, "laptop");
            for n in 0..MAX_KEPT_TRANSFERS {
                let id = TransferId::from_bytes((n as u128).to_be_bytes());
                let offer = Offer::for_files(id, &[("a", b"one")]).unwrap();
                let mut t = Transfer::offered(id, peer, 1).unwrap();
                state
                    .insert_transfer(t.clone(), TransferInfo::new(false, offer, 1_000))
                    .unwrap();
                // The end is saved; the crash comes before its finish order.
                t.cancel(CancelCause::Receiver);
                state.store().save_transfer(&t, 2_000).unwrap();
            }
        }

        let mut state = State::load(Store::open(&db).unwrap()).unwrap();
        let peer = trusted(&mut state, 2, "phone");
        let id = TransferId::from_bytes([0xff; 16]);
        let offer = Offer::for_files(id, &[("a", b"one")]).unwrap();
        let t = Transfer::offered(id, peer, 1).unwrap();
        state
            .insert_transfer(t, TransferInfo::new(false, offer, 3_000))
            .unwrap();
        assert_eq!(state.transfers().count(), MAX_KEPT_TRANSFERS);
    }

    #[test]
    fn transfers_that_failed_to_make_room_still_make_room_later() {
        let mut state = State::load(Store::memory().unwrap()).unwrap();
        let peer = trusted(&mut state, 1, "laptop");
        let offered = |n: u128| {
            let id = TransferId::from_bytes(n.to_be_bytes());
            let offer = Offer::for_files(id, &[("a", b"one")]).unwrap();
            let t = Transfer::offered(id, peer, 1).unwrap();
            (id, t, TransferInfo::new(false, offer, 1_000))
        };
        for n in 0..MAX_KEPT_TRANSFERS as u128 {
            let (id, t, info) = offered(n);
            state.insert_transfer(t, info).unwrap();
            state.update_transfer(&id, |t| t.cancel(CancelCause::Receiver));
        }

        // Every finished transfer fails to be deleted once.
        let next = MAX_KEPT_TRANSFERS as u128;
        for _ in 0..MAX_KEPT_TRANSFERS {
            state.store().refuse_writes();
            let (_, t, info) = offered(next);
            assert!(state.insert_transfer(t, info).is_err());
            state.store().allow_writes();
        }

        let (_, t, info) = offered(next);
        state.insert_transfer(t, info).unwrap();
        assert_eq!(state.transfers().count(), MAX_KEPT_TRANSFERS);
    }

    #[test]
    fn a_late_deletion_result_never_touches_a_file_added_after_it() {
        let mut store = Store::memory().unwrap();
        let place = Place {
            dir: b"/spool".to_vec(),
            dev: 1,
            ino: 2,
        };
        let transfer = TransferId::from_bytes([1; 16]);
        let name = spool_name(transfer);
        let spool = NewFile {
            kind: FileKind::Spool,
            transfer,
            item: 0,
            dir: &place,
            name: name.as_bytes(),
            identity: Some((1, 3)),
            bytes: 100,
        };
        let old = store.add_file(&spool).unwrap();
        store.delete_file(old, 0).unwrap();
        // Two cleanup passes took the same snapshot; the first finishes.
        store.file_deleted(old).unwrap();
        let new = store.add_file(&spool).unwrap();
        assert_ne!(new, old);
        // The second pass reports on the same old file afterwards.
        store.file_deleted(old).unwrap();
        store.deletion_failed(old, "busy", Some(10)).unwrap();
        let files = store.files().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].id, new);
        assert_eq!(files[0].state, FileState::Live);
        assert_eq!(files[0].attempts, 0);
        assert_eq!(store.charged().unwrap(), (100, 0));
    }

    #[test]
    fn a_new_grant_of_trust_after_a_restart_never_reuses_an_epoch() {
        let dir = dir();
        let db = dir.0.join("hyakume.db");
        let first = {
            let mut state = State::load(Store::open(&db).unwrap()).unwrap();
            let first = trusted(&mut state, 1, "laptop");
            state.revoke(&device(1)).unwrap();
            first
        };
        let mut state = State::load(Store::open(&db).unwrap()).unwrap();
        let again = trusted(&mut state, 1, "laptop");
        assert!(again.epoch() > first.epoch());
        assert_eq!(
            state.trust().check(&first),
            Err(crate::trust::TrustError::Stale)
        );
    }

    #[test]
    fn a_database_from_a_newer_build_is_refused() {
        let dir = dir();
        let db = dir.0.join("hyakume.db");
        drop(Store::open(&db).unwrap());
        let conn = Connection::open(&db).unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        drop(conn);
        assert!(matches!(
            Store::open(&db),
            Err(StoreError::NewerSchema(v)) if v == SCHEMA_VERSION + 1
        ));
    }

    #[test]
    fn damaged_rows_are_refused_instead_of_repaired() {
        let dir = dir();
        let db = dir.0.join("hyakume.db");
        {
            let mut state = State::load(Store::open(&db).unwrap()).unwrap();
            trusted(&mut state, 1, "laptop");
        }
        let conn = Connection::open(&db).unwrap();
        // An epoch the counter has not passed would let an old grant come
        // back to life.
        conn.execute("UPDATE counters SET next_epoch = 0", [])
            .unwrap();
        drop(conn);
        assert!(State::load(Store::open(&db).unwrap()).is_err());

        let conn = Connection::open(&db).unwrap();
        conn.execute("UPDATE counters SET next_epoch = 5", [])
            .unwrap();
        conn.execute("UPDATE trust SET alias = 'bad\nalias'", [])
            .unwrap();
        drop(conn);
        assert!(State::load(Store::open(&db).unwrap()).is_err());

        // A device name gets the rules init gave it, so a changed one
        // cannot reach the terminal with controls in it.
        let mut store = Store::open(&db).unwrap();
        store
            .begin_installation(&Installation {
                id: [1; 16],
                device: device(1),
                store: StoreKind::KeyFile,
                name: "alpha".to_owned(),
                ready: false,
            })
            .unwrap();
        assert_eq!(store.installation().unwrap().unwrap().name, "alpha");
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE installation SET name = CAST(?1 AS BLOB)",
            ["alpha\u{1b}[2J\nbeta"],
        )
        .unwrap();
        drop(conn);
        assert!(store.installation().is_err());
    }

    #[test]
    fn a_file_row_whose_name_could_leave_its_folder_is_refused() {
        let mut store = Store::memory().unwrap();
        let place = Place {
            dir: b"/spool".to_vec(),
            dev: 1,
            ino: 2,
        };
        let transfer = TransferId::from_bytes([1; 16]);
        let name = spool_name(transfer);
        store
            .add_file(&NewFile {
                kind: FileKind::Spool,
                transfer,
                item: 0,
                dir: &place,
                name: name.as_bytes(),
                identity: Some((1, 3)),
                bytes: 10,
            })
            .unwrap();
        assert_eq!(store.files().unwrap().len(), 1);
        store
            .conn
            .execute("UPDATE files SET name = CAST('../canary' AS BLOB)", [])
            .unwrap();
        // Cleanup would act on this row, so loading it fails instead.
        assert!(matches!(store.files(), Err(StoreError::Corrupt(_))));
    }

    #[test]
    fn the_database_file_is_private() {
        let dir = dir();
        let db = dir.0.join("hyakume.db");
        drop(Store::open(&db).unwrap());
        let mode = std::fs::metadata(&db).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
