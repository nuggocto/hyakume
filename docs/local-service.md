# The local service, its storage, and the file pipeline

Status: accepted for the Linux command line. Recorded 2026-10-13, updated
2026-10-17.

This record covers what turned the loopback library into a command-line
application: one service per user that owns identity, trust, and work;
local IPC; SQLite storage; the two Linux key stores; and safe files on
disk. It states the current rules, what was rejected, and what later work
must keep. The [build book](book.md) tells how each rule came about.

## 1. Contracts settled first

These rules were fixed before anything was stored, because storage keeps
them.

**Invitations.**

- An inviting device uses up a permanent invitation when its person answers
  yes to the verifier, before its decision is sent.
- Trust is saved only after both authenticated decisions are yes.
- A verified exchange that ends without acceptance closes the invitation.
- An exchange that ends before verification uses one attempt.
- The alias and trusted place a yes reserves belong to the running exchange
  and disappear with it.
- A restart cannot reopen a used, declined, or abandoned invitation. In this
  version that holds because no invitation outlives the endpoint its token
  names (section 3).

**Replay.** The window of offered transfer IDs is separate from the status
history. Resuming existing work will be its own authenticated operation,
never a repeated offer.

**Completion.** A received file is complete only when all of these hold:

| Condition | Where it is enforced |
| --- | --- |
| The peer is authenticated and still trusted under the same epoch | `State::may_publish`, under the state lock, just before the rename |
| The person accepted this exact offer | `OfferAccept` names the offer digest; `State::accept` saved it before it was sent |
| Exactly the offered length arrived | `Receiver::on_chunk` and `on_item_end` count every byte |
| The bytes match the offered digest | `ContentCheck` hashes as the runtime writes; `Receiver::on_verified` compares |
| An atomic rename that never replaces anything committed it | `Destination::publish`: `renameat2(RENAME_NOREPLACE)`, then the directory is synced |
| The record of it is saved, or recovery can prove it | `Store::publish`; otherwise startup recovery checks the file's device and inode |

## 2. Ownership

`hyakume service run` is the one owner of an installation. Command-line
processes only ask: they never open the database, create an identity, or
hold a key. Work they start belongs to the service. A send keeps going when
its client goes away, and only an explicit cancel stops it.

**One owner for durable state: the state lock.** The plan proposed a state
worker thread that receives commands. The runtime already had one owner of
decision order, the `Mutex<State>` behind `StateCell::change`, which orders
revocation against publication. The SQLite connection lives inside
`State`, so every durable write happens in the same critical section as the
decision it records. A separate thread would have duplicated that order
with a message type per mutation. Rejected for that reason.

Writes happen per offer, per acceptance, per item outcome, and per trust
change, never per chunk. The rename and directory sync of a publication
also run under the lock, which is what makes revocation and publication one
order. Chunk writes, hashing, and syncs of partial files run outside it.

The lock's holder may be writing to the database or renaming a file, so
waiting for it is blocking work. `StateCell` waits through
`tasks::blocking_here`, which on the service's multi-thread runtime hands
the worker's other tasks to another thread first, so a stalled holder
cannot take the threads that timers and shutdown need.

**How a write reaches the store.**

- A change that claims success is written first: trust granted, an offer
  accepted, an item published. If the write fails, memory keeps the old
  value and the operation fails.
- A change that ends or interrupts work is applied in memory first, then
  written. If that write fails, the store still shows the work unfinished.
  Startup recovery settles it, and the service refuses new work until it
  restarts (`State::store_failed`). Neither path can make a failure look
  like a success.

## 3. Storage: SQLite

**SQLite through rusqlite 0.40, with bundled SQLite: accepted.** Trust
epochs, item outcomes, the file ledger, and the offer window change
together and must not tear. With JSON files, every write would need its own
journal and recovery. `bundled` avoids depending on the system's SQLite
version and its build flags.

- Rollback journal with `synchronous = FULL`, so a committed write is on
  disk when the call returns. WAL would only pay off with concurrent
  readers; status reads come from memory under the same lock.
- `PRAGMA quick_check` runs at open. `user_version` is the schema version;
  a newer one is refused, never downgraded. Tables are `STRICT`.
- Every row is validated as it loads: IDs and keys decode, names are valid
  aliases and bounded peer text, the device's own name passes the rules
  `init` gave it, saved offers pass `Offer::validate`, item states fit their
  offer state, the epoch counter is past every saved epoch, and counts are
  within their limits. A row that fails is an explicit error; nothing is
  repaired silently.
- The database file is 0600 inside the 0700 state directory.
- `secure_delete` is on, so SQLite zeroes the bytes a write replaces or
  removes instead of leaving them in a free page.

| Table | Holds |
| --- | --- |
| `installation` | installation ID, device public key, chosen store, declared name, whether `init` finished |
| `counters` | the next trust epoch, which survives `reset` so no epoch is reused; the last lifetime-clock reading |
| `trust` | device, alias, declared name, epoch or revoked |
| `offered` | the last 4,096 offered transfer IDs |
| `transfers`, `items` | direction, peer and epoch, the encoded offer (without its preview once finished) and its digest, offer and item states, receive folder, output names, source paths, created and expiry times |
| `files` | the ledger of Hyakume's own files (below) |

Nothing secret is stored: no key, passphrase, or invitation.

**A finished transfer keeps no content: accepted.** A text or link offer
carries a preview of up to 4 KiB, which for short text is the whole
payload. The preview lives as long as the files that hold the payload:
while the transfer is unfinished, because a resume would check the text
against it. The write that gives a finished transfer its finish order also
replaces its saved offer with one that has no preview. The offer's digest,
which the acceptance named, is saved in its own column and computed once at
validation, so a finished offer still names what was accepted. Loading
checks an unfinished transfer's offer against its saved digest and takes a
finished one's as it is.

Moving a schema 1 database first runs `VACUUM`, because schema 1 wrote
without `secure_delete`. Then one transaction adds the column, drops the
previews of finished transfers, and records schema 2. The version changes
last, so a stop anywhere before it redoes the whole step.

**Invitations are not stored: accepted, as a change to the plan.** The plan
proposed saving invitations with their secrets encrypted under a vault key.
But a token names the inviter's transport endpoint, and that key is new on
every start and every unlock (pairing record, section 2). After a restart
no token can reach this device, so a saved secret would protect an
invitation nobody can use. A restart therefore ends every open invitation.
Locking stops the endpoint too, and `Node::shutdown` clears its invitations
once every exchange has ended, because the next node's clock would give
their deadlines no meaning. Revisit this when discovery lets a token
outlive its endpoint.

**Time across restarts.** Saved deadlines are milliseconds of a lifetime
clock (`State::now_ms`) that outlives the process, and every deadline is
both stamped and checked with it, never with the raw wall clock.

- It is built on the wall clock, never reads earlier than the latest time it
  gave, and while the service runs advances at least as fast as the
  monotonic clock. Setting the clock back three days cannot give paused
  work three more days.
- About once a minute its reading is saved with the wall clock's time. At
  the next start, the time stopped is what the wall clock advanced since
  then. If the wall clock reads earlier than at the save, that time cannot
  be measured, and startup recovery expires paused work.
- Paused work also expires when the clock reads earlier than the moment the
  work was created.
- It counts whole milliseconds and carries the rest, so frequent readings
  still move it after a rollback.

**The file ledger.** Every file Hyakume owns has a row: sealed sender spools
and receive partials. The row charges its bytes to its kind's quota from
reservation until the file is published or deleted. When an item ends, its
file's row moves to `delete` in the same write.

```text
reserved ──created──▶ live ──published──▶ (row removed with the commit record)
    └──item ended────────┴──▶ delete ──deleted──▶ (row removed, bytes freed)
                                │ still failing after 24 h
                                ▼
                             retained ──startup or cleanup --retry──▶ delete
```

- Row IDs are never reused (`AUTOINCREMENT`), cleanup passes run one at a
  time, and a deletion result changes a row only while it is still marked
  `delete`, so a late result cannot touch a newer file's row.
- The folders a row names are real paths: the service resolves its state
  and runtime folders once at start (`Locations::prepare`), because cleanup
  and receiving reopen folders without following any link.
- A partial's row must name `.hyakume-<transfer>-<item>.part` and a spool's
  `<transfer>.spool`; any other name fails loading. Deletion refuses
  anything but a single name in its folder, and saved output names must be
  valid file names.
- Deletion retries after 10 s, then four times longer each time, at most an
  hour apart, for 24 hours. A file is deleted only if it is a regular file
  of this user with the recorded inode. A folder that has disappeared counts
  as deleted; a folder replaced by another does not.

## 4. Receiving and publication

**Destinations are qualified when someone starts receiving, before any
offer.** A folder qualifies when:

1. its path resolves with `openat2(RESOLVE_NO_SYMLINKS)`;
2. it belongs to this user and neither its group nor others can write to it;
3. its filesystem is ext4, btrfs, XFS, or tmpfs (overlay, FUSE, and network
   filesystems are refused);
4. a probe in it shows that exclusive creation refuses a used name, that a
   no-replace rename refuses an existing name and leaves its content
   unchanged, that a no-replace rename onto a free name works, and that file
   and directory sync work.

The filesystem name alone is not trusted; the probe must pass too. A folder
that fails gets a typed refusal naming the rule, and the command offers
Hyakume's own folder, `<state>/received`, instead. Every later operation
goes through the opened directory handle, never the path.

**Names.** `OutputName` accepts one UTF-8 component of at most 255 bytes.
It refuses separators, NUL, `.` and `..`, control and invisible formatting
characters, and the reserved prefix `.hyakume-`. An offer with an unusable
name, or two items with the same name, is refused before the person is
asked, and a name is never rewritten. Text and links get generated names
(`text-<id>.txt`, `link-<id>.txt`).

**Before acceptance**, the offer must fit the 32 GiB quota for partial files
and leave 64 MiB free on the destination's filesystem. Free space is only
advice; an actual write can still fail. Acceptance reserves a partial file
per item in the ledger, in the same write as the accepted state.

**The intent to publish is already on disk.** Each item's output name is
saved at acceptance and its partial file's device and inode as soon as the
file exists. Together they say which file may become which name, so no
separate intent record is written. If the inode cannot be saved, or the
reservation is already gone because a cancel or revoke ended the item, the
partial file is removed before any byte is written to it.

```text
partial written, hashed, synced (outside the lock)
  └▶ under the state lock:
       may_publish: cancelled? trust epoch unchanged?
       renameat2(partial, output, RENAME_NOREPLACE)   ◀ the filesystem commit
       fsync(directory)
       Store::publish: item published, partial's row removed
  └▶ ItemResult::Published to the peer
```

If the record cannot be saved after the rename, memory shows the item
published because the file is, the peer is not told, and startup recovery
records it. A file that already had the output name makes the rename fail
with `EEXIST`; the item fails as `exists` and the batch goes on. A
published file keeps mode 0600 from its partial.

**An unconfirmed rename is its own item state: accepted.** If the rename
succeeds but the directory sync fails, nobody can say whether the name
survives a crash. The item becomes `unconfirmed`, keeps its ledger row with
the file's inode, and the rest of the batch is cancelled. The sender hears
`storage-failed`, never `published`. A folder that cannot be read at
startup makes an item unconfirmed too, because a crash right after the
rename leaves it saved as queued.

Only the disk settles an unconfirmed item. Startup recovery and every
maintenance pass (each 5 s) check it the same way: the file under its
output name counts as published only if a new sync of the folder succeeds.
On ext4, XFS, and btrfs a name is journaled metadata, and a failed commit
leaves the filesystem read-only, so a later sync that succeeds means the
rename is on disk.

A cancel, a revocation, or expiry does not end an unconfirmed item. It keeps
the first such stop in its state, saved as for example
`unconfirmed:cancelled:revoked`, and the item table refuses every other
event. `Transfer::found_on_disk` is the only way out:

| The disk shows | Result |
| --- | --- |
| the final file, folder synced | published; the kept stop is dropped, because the rename came before the stop in the lock's order |
| the final file, folder still not syncing | unchanged, checked again next pass |
| only the partial file, with a kept stop | that stop: cancelled or expired, and cleanup deletes the partial |
| only the partial file, no stop | paused |
| nothing readable | unchanged, checked again next pass |

A transfer whose only unfinished items are unconfirmed has no connection,
so it counts as paused for its place among the unfinished transfers, which
the state holds until it ends. Its outcome shows as `unconfirmed`, and
`cancel` returns at once instead of waiting for an end only the disk can
bring.

**Startup recovery** runs before the socket opens:

| Saved state | Evidence | Result |
| --- | --- | --- |
| offer not answered | — | rejected: interrupted |
| receiving item, partial recorded (unconfirmed included) | the output name has the partial's inode, and the folder syncs | published |
| | the output name has the partial's inode, but the folder does not sync | unconfirmed |
| | the partial still has its inode | kept, paused |
| | the folder or a name in it cannot be read | unconfirmed |
| | the folder is gone or replaced, or neither name has the inode | failed: lost |
| sending item from a spool | spool present, same inode and size, read-only | kept, paused |
| | otherwise | failed: lost |
| peer revoked or re-paired | — | cancelled: revoked; an unconfirmed item keeps it |
| resume window over, clock behind, or time stopped unmeasurable | — | expired; an unconfirmed item keeps it |
| a spool file no row records | named `<transfer>.spool`, a regular file of this user | recorded for deletion, charged until cleanup deletes it |
| | anything else in the spool folder | left alone, counted, and logged |
| transfer ended but not given its finish order | — | given the next one |

An identical file that was already under the output name proves nothing,
because its inode differs. Ambiguous evidence never becomes a success.
Making room for a new transfer drops the oldest finished one from the
finish order only after its row is deleted.

## 5. Sending

**Regular files** are opened with `O_NOFOLLOW | O_NONBLOCK`, so a symbolic
link is refused and a FIFO cannot block the open. Each must be a regular
file of at most 16 GiB. It is hashed with a 1 MiB buffer, and its device,
inode, size, and times are recorded before the offer exists.

A file must end where its size says: preparation reads one byte at the
reported size and refuses the file if that byte exists and the file kept its
identity (a file written to meanwhile is refused as changed). Files under `/proc`
report size zero and still have content, and offered at their reported size
they would arrive empty. The refusal points to standard input, which gets an
exact length from the spool.

While sending, the bytes are hashed again, and before an item is reported
sent the file must still have its identity and still end at its size. A
different digest or a changed file fails the item as `source-changed` and
cancels the rest, so different bytes are never sent under the same offer.

One offer carries at most 256 items, and its names, sizes, and digests
together take at most 64 KiB on the wire. `Offer::validate` checks both
before anything is recorded, measuring with the encoder's own writer.

**Standard input, text, and links** go through one spool pipeline, so every
item is read from a file:

```text
client data frames ──▶ <state>/spool/<transfer>.spool (0600, created new)
   limits: 2 GiB per input (1 MiB text, 8 KiB link), 8 GiB for all spools,
           64 MiB left free, the input deadline, client disconnection
EOF ──▶ chmod 0400, fsync, fsync directory, reopen read-only
     ──▶ ledger row (charged) ──▶ offer
```

- Anything that stops the input before its end, including the client going
  away, deletes the spool, and no offer is made. Text and links are checked
  for UTF-8 before sealing.
- Until sealing, the input's bytes are counted in memory, and they stay
  counted until the file is really gone. A file that cannot be deleted gets
  a ledger row marked for deletion, charged by its size on disk.
- The deadline is 10 minutes; `service run --stdin-deadline` can lower it.
  It is checked on every piece of input, so a backlog cannot carry a stream
  past it.
- From the end of input on, sealing, the ledger row, and starting the send
  run as one task the service owns (`Service::own`). If the row cannot be
  written, the sealed spool is deleted and the failed write stops new work.
- Admitting more input and moving a sealed spool's bytes into the ledger
  take the same two locks in the same order, so no byte is counted twice or
  not at all.

**Interrupted work.** Work cut off by a stop, a lock, or a crash pauses and
keeps its spool or partial file and its quota until it expires after 7 days
or is cancelled. A connection lost while the service runs fails the work
instead. Resuming is the reliable-transfer work.

## 6. Key stores and the lock lifecycle

Both stores hold a `KeyBundle`: a magic prefix, the installation ID, and the
device key's seed, 60 bytes in all. Unlocking checks the bundle's length,
prefix, and installation, then that the key's public half is the device
recorded in the database. A key that does not match is refused, never
adopted, and nothing creates a new identity because loading failed.

**Passphrase key file: age 0.12 with scrypt, accepted.** A maintained format
with authenticated streaming encryption. The work factor is fixed at
N = 2^17: 128 MiB of memory, about half a second. A file asking for more
work is refused before any is done. 2^18 would use 256 MiB, the service's
whole memory budget. The file is written to a new name, synced, renamed, and
its directory synced. A new passphrase must be at least 8 bytes.

Passphrases come from the terminal with echo off, turned off before the
prompt appears, or from `--passphrase-fd`, which warns that a stored
passphrase gives up the protection against a stolen disk. A passphrase
travels to the service only in a secret frame, never in a JSON message.

**Secret Service over D-Bus through `zbus`: accepted.** Every call has a
3 s timeout. A locked collection or item reads as locked, and Hyakume never
calls `unlock`, which can open a graphical prompt. The key is stored with
the attributes `application=hyakume` and `installation=<id>`, and loads at
start when the keyring is unlocked.

A keyring can be set to ask someone to confirm creating or deleting an
item, and answers with a prompt object. The `secret-service` crate showed
that prompt from inside its calls and waited, opening a window nobody on SSH
could reach. Rejected for that reason. Hyakume makes the calls itself
(`OpenSession`, `ReadAlias`, `Locked`, `SearchItems`, `CreateItem`,
`GetSecret`, `Item.Delete`) and answers any prompt with `Dismiss`, failing
with advice to allow the action without confirmation or use the key file.
The session is `dh-ietf1024-sha256-aes128-cbc-pkcs7`: Diffie-Hellman in
RFC 2409's 1024-bit group with `crypto-bigint`'s constant-time arithmetic,
HKDF-SHA256 into an AES-128 key, and AES-CBC with a fresh IV per secret. A
public value of 0, 1, p − 1, or outside the group is refused.

```text
Uninitialized ──init──▶ Unlocked ◀──unlock── Locked
                           └────lock, stop────▶┘
```

- `init` records the installation as unfinished, writes the key, then marks
  it finished. If the endpoint cannot start, the identity stays saved and
  locked, and `unlock` tries again. Startup and a later `init` remove only
  an initialization that never finished.
- `reset` deletes the key first, then forgets trust, transfers, and the
  offer window. The installation record is what finds the key, so a key
  that cannot be confirmed deleted stops `reset` with nothing forgotten.
  Hyakume's files stay in the ledger until they are deleted.
- Locked, the service answers status, devices, transfers, inspect, cancel,
  remove, and cleanup, but runs no peer endpoint. Expiry and cleanup go on.
- Locking stops the node: work pauses, prompts are answered no, and the key
  leaves memory with the node. Requests reach the node only inside a
  synchronous closure (`Service::with_node`), so none can keep it alive.
  A join runs as a task of the node, so stopping the node waits for it.
- `init`, `unlock`, `lock`, and `reset` run as tasks the service owns. A
  client that disconnects stops waiting for the answer, never the
  transition. Once the service is stopping, no transition starts.
- If the node's work does not stop within the drain deadline, `lock` or
  `reset` fails and the whole service stops instead of starting a new node
  beside work that still runs; the next start recovers.

## 7. Local IPC

- The socket is `$XDG_RUNTIME_DIR/hyakume/hyakume.sock`, mode 0600, in a
  0700 directory that must belong to this user and must not be a link. A
  socket left by an earlier run is replaced only while the instance lock is
  held.
- The state and runtime directories each hold a `service.lock`. When both
  are one directory, under one path or two, the files compare equal by
  device and inode after opening and the lock is taken once, because a
  second `flock` on the same file would refuse its own process.
- Every connection's peer credentials must show this user's UID.
- A frame is a 4-byte length, a kind byte, and a body. Message and data
  bodies are at most 1 MiB, secret bodies at most 4 KiB, and the length is
  checked before allocation. Messages are versioned JSON with request IDs,
  unknown fields and operations are refused, and errors carry stable codes.
  `FrameReader` keeps a frame read in part, so a cancelled wait loses no
  bytes.
- At most 8 clients connect at once; the ninth gets a `busy` reply.
- `transfers` lists one summary per kept transfer, built straight from
  state, so the list of all 256 always fits in a frame; `inspect` shows the
  items.
- One request runs per connection: zero or more events, then one reply. The
  reply goes out only after the request has ended, so a client that asks
  again at once is never told `busy`. Input that arrives when no request
  wants it is dropped.
- **Prompts belong to one connection.** Verifier words go to the client
  running that pairing, offers to the client receiving. An answer must come
  on the same connection, name the prompt, and repeat its context: a hash of
  the invitation, peer, and verifier, or the offer digest. Anything else
  gets `answer-refused` and approves nothing. Disconnecting answers every
  open prompt no.
- A question the node stopped waiting for gives its place back. A client
  holds at most 8 open questions; an offer that finds no place, or nobody
  receiving, is refused as `busy`, never as the person's decline.
- The receiving client and the node's destination change together under the
  routes lock, so a client that stops receiving cannot clear the
  destination of the one that replaced it.
- Each client has a queue of 64 events. A client that lets it fill is
  disconnected. What is still queued for a departing client gets 5 seconds
  to reach it, then the client is cut off, so it cannot keep its slot. This
  version sends no progress events.

Same-user malware is outside the threat model; these checks stop other
users and accidental calls.

## 8. Limits added

| Resource | Limit | Where |
| --- | ---: | --- |
| IPC frame | 1 MiB | `ipc::MAX_FRAME_BYTES` |
| IPC clients | 8 | `ipc::MAX_CLIENTS` |
| Events queued per client | 64 | `ipc::EVENT_QUEUE` |
| Credential | 4 KiB | `secrets::MAX_CREDENTIAL_BYTES` |
| Prompts held per client | 8 | `node::PROMPT_QUEUE` |
| Secret Service call | 3 s | `secrets::SECRET_SERVICE_TIMEOUT` |
| scrypt work factor | 2^17 (128 MiB) | `secrets::KEY_FILE_WORK_FACTOR` |
| Service blocking jobs at once (preparing, spooling, key files, folder checks, deletions) | 4 | `service::SERVICE_JOBS` |
| Blocking file operations at once | 4 | `node::FS_JOBS` |
| Standard input deadline | 10 min, can be lowered | `service::STDIN_DEADLINE` |
| One spool | 2 GiB | `limits::MAX_SPOOL_BYTES` |
| All spools | 8 GiB | `limits::MAX_SPOOL_TOTAL` |
| All partial files | 32 GiB | `limits::MAX_PARTIAL_TOTAL` |
| Free space a write leaves | 64 MiB | `limits::MIN_FREE_BYTES` |
| Resume window | 7 days | `limits::RESUME_WINDOW` |
| Cleanup retry window | 24 h | `limits::CLEANUP_WINDOW` |
| Graceful drain | 20 s, can be lowered | `Limits::drain_deadline` |

The plan proposed a work queue of 32 jobs. Each queue here is a permit
count instead: a job waits for a permit, and the waiters are bounded by the
admission limits behind them (sessions, transfers, clients).

## 9. Starting and stopping

```text
start: locations ─▶ instance lock ─▶ open, check, migrate ─▶ recovery
       ─▶ socket ─▶ "listening" ─▶ unlock if the Secret Service allows it
stop:  close the socket ─▶ clients end and drain (5 s each, 6 s in all)
       ─▶ stop the node (accepted work pauses) ─▶ wait for tasks and
          blocking jobs, sharing one deadline
```

The connection that sent `stop` asks the service to stop only after the
reply is in its queue, so the command always hears that the service
stopped.

`Node::shutdown` takes its deadline before it waits for anything. It sets
the pausing flag without the state lock, closes the endpoint, and waits for
the accept task, which owns every connection and send task. Then it takes
back every file-job permit and clears the node's invitations in a blocking
job that holds those permits until it has the lock. Each wait ends at the
one deadline. A stalled lock holder, such as a publication or a database
write on a stuck disk, therefore fails the drain in time instead of
delaying it, and a call cut short leaves its work to the next call, which
waits for the same permits. A send recorded but still queued when the node
stops never runs; a guard it carries from the moment it was recorded ends
its transfer.

Each blocking job runs through `tasks::spawn_blocking`, which stops the
process if the job panics, from the job's own thread, because the request
awaiting the job may be gone. Each also holds a permit until it really
ends: file work one of the node's, the service's own jobs one of the
service's. A drain succeeds only after taking back every permit of both.
The publishing rename runs inside the state lock instead, so revocation
cannot slip in before it. If the deadline passes, `service run` exits with
status 1, work still running is not counted as finished, and the next start
recovers.

## 10. Not in this version

- **Routes are explicit.** `hyakume status` shows the device's route and
  `send --at ROUTE` names the receiver's. The endpoint key is new every run,
  and finding devices on a LAN is the discovery work. `service run
  --listen` refuses `0.0.0.0` and `[::]`, which no other device can dial;
  listen on this machine's LAN address.
- **No resume and no progress events.** Paused work waits, as above.
- **A disk that fills during a receive is tested by injection.** A real full
  disk is tested at both refusal points on small tmpfs mounts in an
  unprivileged user namespace (`mise run test-full-disk`). A write failing
  halfway through a received item is injected, because the free-space check
  refuses before any real write can fill the disk.

## 11. Conditions for later work

- **Resume** reads only the sealed spool or the recorded source, and checks
  the spool's inode, size, and digest first. It authorizes under the
  transfer's saved epoch; a revoked or re-paired peer's work stays
  cancelled.
- **Discovery** that lets a token outlive its endpoint must persist open
  invitations with their attempt counts. On load, each claim is settled the
  way `PairingOwner` settles it.
- **Windows** needs its own qualified destinations: an atomic no-replace
  rename, a refusal to follow links, and owner checks. It also needs its own
  key store, IPC peer check, and spool permissions. None of the Linux system
  calls carry over.
- **A schema change** raises `SCHEMA_VERSION` and adds a migration step in
  one transaction; the version is never downgraded.

## 12. Verification

| What | Tests |
| --- | --- |
| Pairing through a real terminal, every payload kind, rejection, an existing file, hostile text, revocation | `tests/service.rs` `two_services_pair_and_exchange_every_kind_of_payload_through_the_cli` |
| Crashes on both sides of the rename, an unreadable folder at restart | `a_crash_after_the_rename_is_recovered_as_published`, `a_crash_before_the_rename_keeps_the_partial_private_until_its_work_is_cancelled` |
| A sealed spool across a sender crash | `a_sender_crash_keeps_the_sealed_spool_of_its_paused_transfer_across_restart` |
| Oversized, timed-out, cut-off, and backlogged input | `standard_input_that_is_too_large_or_too_slow_offers_nothing`, `service::clients::tests::input_still_queued_when_its_deadline_passes_is_refused` |
| A file that reads past its reported size | `files::source::tests::a_file_with_more_bytes_than_its_size_is_refused` |
| Linked, shared, and unsupported folders; no terminal | `an_unusable_destination_is_refused_before_any_offer_is_shown` |
| Encrypted key at rest, locked after restart, wrong passphrase, no echo | `a_locked_service_keeps_its_key_on_disk_encrypted_and_refuses_work_until_unlocked` |
| A Secret Service that never answers, that wants confirmation, and a real isolated keyring | `an_unresponsive_secret_service_is_refused_quickly_instead_of_hanging`, `secrets::tests::a_secret_service_that_wants_confirmation_never_gets_to_ask`, `a_secret_service_key_unlocks_at_start_and_is_deleted_on_reset` (`mise run test-keyring`) |
| Unconfirmed renames: cancel, revocation, expiry, cleanup, maintenance, unreadable folders, kept stops | `receive::tests::a_rename_whose_folder_cannot_be_synced_is_never_acknowledged`, `recovery::tests` |
| Previews of finished transfers and the schema 1 upgrade | `storage::tests::a_finished_transfer_keeps_no_content_in_the_database`, `storage::tests::a_schema_1_database_drops_the_previews_of_finished_transfers`, `declined_text_leaves_no_copy_in_either_database` |
| Drains that overrun, are cut short, or meet a held state lock, from another thread or from a task on the only worker | `node::tests::a_drain_that_overruns_fails_and_never_counts_stalled_work_as_finished`, `node::tests::a_shutdown_cut_short_leaves_its_work_to_the_next_one`, `node::tests::a_state_lock_held_past_the_drain_deadline_fails_the_drain_in_time`, `node::tests::a_task_stalled_inside_the_state_lock_cannot_starve_the_drain_timer` |
| Sends still queued, and invitations open, when the node stops | `node::tests::sends_still_queued_when_shutdown_begins_never_stay_offered`, `node::tests::invitations_open_when_the_node_stops_leave_room_for_the_next_node` |
| A partial file without its record or reservation | `node::receiving::tests` |
| Bad frames, stale answers, the ninth client, a client that stops reading, questions given up on | `the_local_socket_refuses_bad_frames_stale_answers_and_extra_clients`, `a_client_that_stops_reading_gives_its_place_back`, `service::clients::tests` |
| The reply to `stop` | `the_stop_command_is_told_the_service_stopped`; 200 times under `mise run test-stop-stress` |
| The ledger: undeletable spools and orphans, names outside a folder, making room, linked ancestors | `storage::tests`, `recovery::tests`, `paths::tests`, `destination::tests`, `unfinished_input_that_cannot_be_deleted_stays_charged_until_cleanup_deletes_it` |
| A batch whose names pass the offer's metadata limit | `a_batch_too_large_for_one_offer_is_refused_and_the_service_still_restarts`, `offer::tests::every_offer_that_validates_can_be_sent` |
| The lifetime clock | `clock::tests`, `receive::tests::work_received_after_the_wall_clock_went_back_keeps_its_whole_window` |
| A second service for the same state | `a_second_service_for_the_same_state_is_refused` |
| Real full disks | `a_full_disk_refuses_offers_and_input_and_leaves_nothing_behind` (`mise run test-full-disk`) |
| The packaged binary on a clean host | `cargo xtask qualify`, the `clean_host_journey` test |
| Decoders | fuzz targets `ipc_message` and `stored_values` |

Crash points (`HYAKUME_CRASH_AT`) exist only in debug builds; release builds
compile them to nothing.

## Verdicts

- **SQLite through rusqlite, bundled: go.**
- **The state lock as the one owner of durable state: go.**
- **Invitations kept in memory only: go**, until discovery lets a token
  outlive its endpoint.
- **age with scrypt at N = 2^17 for the key file: go.**
- **Secret Service through `zbus`, with a 3 s timeout and no unlock or
  prompt calls: go.**
- **Partial files beside the output, published by `renameat2` with
  `RENAME_NOREPLACE` into qualified folders on ext4, btrfs, XFS, or tmpfs:
  go.**
- **One spool pipeline for standard input, text, and links: go.**
