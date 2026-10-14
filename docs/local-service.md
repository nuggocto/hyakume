# The local service, its storage, and the file pipeline

Status: accepted for the Linux command line. Recorded 2026-10-13.

This record covers what turned the loopback library into a command-line
application: one service per user that owns identity, trust, and work;
local IPC; SQLite storage; the two Linux key stores; and safe files on
disk. The [build book](book.md) explains the result with diagrams. This
record lists the decisions, what was rejected, and what later work must
keep.

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

**One owner for durable state: the state lock.** The plan proposed a
dedicated state worker thread that receives commands. The runtime already
had one owner of decision order, the `Mutex<State>` behind
`StateCell::change`, which orders revocation against publication. The
SQLite connection now lives inside `State`, so every durable write happens
in the same critical section as the decision it records. A separate thread
would have duplicated that order with a message type per mutation.
Rejected for that reason.

Writes happen per offer, per acceptance, per item outcome, and per trust
change, never per chunk. The rename and directory sync of a publication
also run under the lock, which is what makes revocation and publication
one order. Chunk writes, hashing, and syncs of partial files run outside
it.

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
  `init` gave it, saved offers pass `Offer::validate`, item
  states fit their offer state, the epoch counter is past every saved
  epoch, and counts are within their limits. A row that fails is an
  explicit error; nothing is repaired silently.
- The database file is 0600 inside the 0700 state directory.

| Table | Holds |
| --- | --- |
| `installation` | installation ID, device public key, chosen store, declared name, whether `init` finished |
| `counters` | the next trust epoch; it survives `reset`, so no epoch is ever reused |
| `trust` | device, alias, declared name, epoch or revoked |
| `offered` | the last 4,096 offered transfer IDs |
| `transfers`, `items` | direction, peer and epoch, the encoded offer, offer and item states, receive folder, output names, source paths, created and expiry times |
| `files` | the ledger of Hyakume's own files (below) |

Nothing secret is stored: no key, passphrase, or invitation.

**Invitations are not stored: accepted, as a change to the plan.** The plan
proposed saving invitations with their secrets encrypted under a vault key.
But a token names the inviter's transport endpoint, and that key is new on
every start and every unlock (pairing record, section 2). After a restart,
no token can reach this device, so a saved secret would protect an
invitation nobody can use. Keeping invitations in memory also removes the
vault key and its encrypted records. A restart therefore ends every open
invitation; it cannot reopen one. The `PairingOwner` rules still settle
every claim when its exchange ends, including at shutdown. Revisit this when
discovery lets a token outlive its endpoint.

Locking stops the endpoint too, and its invitations end with it. The node
started at the next unlock has a new endpoint and a new monotonic clock, so
an old invitation's deadline would mean nothing to it, and the invitation
would hold one of the 8 slots until that meaningless deadline passed.
`Node::shutdown` clears them once every exchange has ended.

**Time across restarts.** A `Moment` means nothing after a restart, so
saved deadlines are milliseconds of a lifetime clock (`State::now_ms`) that
outlives the process. Paused work expires when the clock reaches its
deadline, or when the clock reads earlier than the moment the work was
created, because then nobody can tell how much time passed. The cleanup
window follows the same rule.

The lifetime clock is built on the wall clock. It never reads earlier than
the latest time it gave, and while the service runs it advances at least as
fast as the monotonic clock. Setting the clock back three days therefore
cannot give paused work three more days. Every deadline is both stamped and
checked with this clock, never with the raw wall clock: a stamp from the
wall clock after a rollback would sit behind the lifetime clock and cut a
new transfer's window short, or end it at once.

About once a minute the clock's reading is saved in `counters`, with the
wall clock's time at that moment. At the next start, the time the service
was stopped is what the wall clock advanced since that save, so a rollback
during the earlier run is not forgotten either. If the wall clock reads
earlier than at the save, that time cannot be measured, and startup
recovery expires paused work, the same rule as for a clock that reads
earlier than the work's start. The minute between saves counts as time
stopped, which can only shorten a lifetime.

The clock counts whole milliseconds and carries the rest to the next
reading. A busy transfer reads it many times a millisecond; if each reading
dropped its fraction, the clock would stand still after a rollback and
paused work would never expire.

**The file ledger.** Every file Hyakume owns has a row: sealed sender spools
and receive partials. The row charges its bytes to its kind's quota from
reservation until the file is published or deleted. Retention follows the
item: when an item ends, its file's row moves to `delete` in the same write.

```text
reserved ──created──▶ live ──published──▶ (row removed with the commit record)
    └──item ended────────┴──▶ delete ──deleted──▶ (row removed, bytes freed)
                                │ still failing after 24 h
                                ▼
                             retained ──startup or cleanup --retry──▶ delete
```

Row IDs are never reused (`AUTOINCREMENT`), cleanup passes run one at a
time, and a deletion result changes a row only while it is still marked
`delete`. A late result about an old file cannot remove a newer file's row
or its charged bytes.

The folders a row names are real paths. At start the service resolves its
state and runtime folders once (`Locations::prepare`), so a linked ancestor,
such as a home folder that is a link, does not make cleanup or the default
receive folder refuse paths it recorded itself: both reopen folders without
following any link. The state and runtime folders themselves must still be
real folders, never links.

Cleanup deletes the name a row records, so loading checks it: a partial's
row must name `.hyakume-<transfer>-<item>.part` and a spool's row
`<transfer>.spool`, exactly the names Hyakume gives them. Any other name,
such as a path through `..`, makes loading fail instead of deleting
something outside the folder. Deletion itself also refuses anything but a
single name in its folder. Saved output names must be valid file names for
the same reason: recovery looks for a published item under them.

Deletion retries after 10 s, then four times longer each time, at most an
hour apart, until 24 hours have passed. A file is deleted only if it is a
regular file of this user and, when its inode was recorded, the same inode.
A folder that has disappeared counts as deleted; a folder replaced by
another one does not.

## 4. Receiving and publication

**Destinations are qualified when someone starts receiving, before any
offer.** A folder qualifies when:

1. its path resolves with `openat2(RESOLVE_NO_SYMLINKS)`, so no component is
   a symbolic link;
2. it belongs to this user and neither its group nor others can write to it;
3. its filesystem is ext4, btrfs, XFS, or tmpfs (overlay, FUSE, and network
   filesystems are refused);
4. a probe in it shows that exclusive creation refuses a used name, that a
   no-replace rename refuses an existing name and leaves its content
   unchanged, that a no-replace rename onto a free name works, and that file
   and directory sync work.

The filesystem name alone is not trusted; the probe must pass too. tmpfs is
included because it gives the same guarantees, though its files do not
survive a reboot. A folder that fails gets a typed refusal naming the rule.
The command then offers Hyakume's own folder, `<state>/received`, as an
explicit choice. Every later operation goes through the opened directory
handle, never the path.

**Names.** `OutputName` accepts one UTF-8 component of at most 255 bytes.
It refuses separators, NUL, `.` and `..`, control and invisible formatting
characters, and the reserved prefix `.hyakume-`. An offer with an unusable
name, or two items with the same name, is refused before the person is
asked. A name is never rewritten into a different one. Text and links get
generated names (`text-<id>.txt`, `link-<id>.txt`). Duplicate names in one
batch are refused on the sending side too.

**Before acceptance**, the offer must fit the 32 GiB quota for partial
files and leave 64 MiB free on the destination's filesystem. Free space is
only advice; an actual write can still fail. Acceptance reserves a partial
file per item in the ledger, in the same write as the accepted state.

**The intent to publish is already on disk.** Each item's output name is
saved at acceptance. Its partial file's device and inode are saved as soon
as the file exists. Together they say which file may become which name, so
no separate intent record is written before the rename.

If the inode cannot be saved, the partial file is removed at once and the
item fails as a storage failure before any byte is written to it. Without
the inode on record, recovery could not tell the received file from any
other under the output name, so it is never published.

The same happens when the reservation is gone. A cancel or revoke ends the
item, and the same write marks its row for deletion, so a partial file
created just after has no row to record it, and cleanup would never find
it. In that case the cancel or revoke has already reached the state, and
its cause is what the transfer reports, not a storage failure.

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
records it.

If the rename succeeds but the directory sync fails, nobody can say yet
whether the name survives a crash. The item is neither published nor
failed: it pauses, keeping its ledger row with the file's inode, and the
rest of the batch is cancelled. The sender is told `storage-failed`, never
`published`. Startup recovery settles the item: the file under its output
name counts as published only if a new sync of the folder succeeds then.
On ext4, XFS, and btrfs a name is journaled metadata, and a failed commit
aborts the journal and leaves the filesystem read-only, so a later sync
that succeeds means the rename is on disk. Until then the item stays
paused, and if it expires first, the file stays where it is. This needs a
new transition, verifying to paused, which shutdown already made directly.
The paused transfer outlives its session, so the session hands its place
among the unfinished transfers to the state, which lets it go when the
transfer ends. Before, the place went with the session, and paused
transfers could pile up past the limit until the next unlock.

The sender's spool goes as for any failed item. Keeping it would help only
a resume, which this version does not have.

A file that already had the output name makes the rename fail with
`EEXIST`, so the item fails as `exists` and the batch goes on. A published
file keeps mode 0600 from its partial.

**Startup recovery** runs before the socket opens:

| Saved state | Evidence | Result |
| --- | --- | --- |
| offer not answered | — | rejected: interrupted |
| receiving item, partial recorded | the output name has the partial's inode, and the folder syncs | published |
| | the output name has the partial's inode, but the folder does not sync | kept, paused |
| | the partial still has its inode | kept, paused |
| | neither | failed: lost |
| sending item from a spool | spool present, same inode and size, read-only | kept, paused |
| | otherwise | failed: lost |
| peer revoked or re-paired | — | cancelled: revoked |
| resume window over, clock behind, or time stopped unmeasurable | — | expired |
| a spool file no row records | — | deleted (input that never finished) |
| transfer ended but not given its finish order | — | given the next one |

An identical file that was already under the output name proves nothing,
because its inode differs. Ambiguous evidence never becomes a success.

A transfer's end and its place in the finish order are two writes. Only
finished transfers can be dropped to make room for new ones, so a crash
between the two used to leave an ended transfer that held one of the 256
places for good. Loading the state now gives such a transfer the next
finish order. Making room works the same way: the oldest finished transfer
leaves the finish order only after its row is deleted, so a failed delete
leaves it first in line instead of in the history for good.

## 5. Sending

**Regular files** are opened with `O_NOFOLLOW | O_NONBLOCK`, so a symbolic
link is refused and a FIFO cannot block the open. Each must be a regular
file of at most 16 GiB. It is hashed with a 1 MiB buffer, and its device,
inode, size, and times are recorded before the offer exists. While sending,
the bytes are hashed again; a different digest or a changed identity fails
the item as `source-changed` and cancels the rest. Different bytes are
never sent under the same transfer.

**Standard input, text, and links** go through one spool pipeline, so every
item is read from a file:

```text
client data frames ──▶ <state>/spool/<transfer>.spool (0600, created new)
   limits: 2 GiB per input (1 MiB text, 8 KiB link), 8 GiB for all spools,
           64 MiB left free, the input deadline, client disconnection
EOF ──▶ chmod 0400, fsync, fsync directory, reopen read-only
     ──▶ ledger row (charged) ──▶ offer
```

Anything that stops the input before its end, including the client going
away, deletes the spool, and no offer is made. Text and links are checked
for UTF-8 before sealing. A spool's ledger row normally exists only after
sealing, so until then the input's bytes are counted in memory, and they
stay counted until the file is really gone. If it cannot be deleted, it
gets a ledger row marked for deletion at once, still charged, and cleanup
retries and reports it like any other file. The row charges the file's
size on disk, which a write that failed partway leaves larger than the
bytes the spool counted; if that size cannot be read, it charges the most
the file can hold. The deadline is 10 minutes;
`service run --stdin-deadline` can lower it. It is checked on every piece
of input, not only while waiting: input already queued when it passes is
refused too, so a backlog cannot carry a stream past it.

From the end of input on, the offer is the service's to make, not the
request's. Sealing, the ledger row, and starting the send run as one task
the service owns (`Service::own`), so a client that leaves then cannot
leave a sealed spool behind without a row. If the row cannot be written,
the sealed spool is deleted at once and the failed write stops new work, as
any failed durable write does; if it cannot be deleted either, its bytes
stay counted until the next start removes it. A spool no transfer took,
because the send could not start, is marked for deletion.

Spool bytes are counted in memory while input arrives and in the ledger
once sealed. Admitting more input and moving a sealed spool's bytes into
the ledger take the same two locks in the same order, the in-memory count
first, so admission never sees bytes in both places or in neither.

**Interrupted work.** Work cut off by a stop, a lock, or a crash pauses and
keeps its spool or partial file and its quota until it expires after 7 days
or is cancelled. In this version, a connection lost while the service is
running fails the work instead, and paused work can only be inspected,
cancelled, or left to expire. Resuming it is the reliable-transfer work.

## 6. Key stores and the lock lifecycle

Both stores hold a `KeyBundle`: a magic prefix, the installation ID, and
the device key's seed, 60 bytes in all. Unlocking checks the bundle's
length, its prefix, and its installation. The service then checks that the
key's public half is the device recorded in the database. A key that does
not match is refused, never adopted, and nothing creates a new identity
because loading failed.

**Passphrase key file: age 0.12 with scrypt, accepted.** It is a maintained
format with authenticated streaming encryption, so it needs no new format.
The work factor is fixed at N = 2^17: 128 MiB of memory, about half a
second. A file asking for more work is refused before any is done. 2^18,
age's usual target, would use 256 MiB, the service's whole memory budget,
for the length of the unlock. The file is written to a new name, synced,
renamed, and its directory synced. A new passphrase must be at least
8 bytes.

Passphrases come from the terminal with echo off (`rustix::termios`), or
from `--passphrase-fd`, which prints a warning that a stored passphrase
gives up the protection against a stolen disk. Ctrl-C at the prompt puts
the terminal back. A passphrase travels to the service only in a secret
frame, never in a JSON message.

**Secret Service over D-Bus through `zbus`, with Diffie-Hellman encrypted
transport, accepted.** Every call has a 3 s timeout. A locked collection or
item reads as locked, and Hyakume never calls `unlock`, which can open a
graphical prompt. The key is stored with the attributes
`application=hyakume` and `installation=<id>`. When the keyring is unlocked,
the service loads the key at start. Nothing ever switches stores.

An unlocked keyring can still be set to ask someone to confirm creating or
deleting an item. It answers such a call with a prompt object, and nothing shows until the client calls `Prompt` on
it. The `secret-service` crate, used before, calls it from inside
`create_item` and `delete` and then waits, so the window opened on a
desktop nobody on SSH could reach. Its D-Bus calls and session key are
private, so it could not be told to stop. Hyakume now makes the calls it
needs itself: `OpenSession`, `ReadAlias`, the collection's `Locked`
property, `SearchItems`, `CreateItem`, `GetSecret`, and `Item.Delete`. A
reply that names a prompt is answered with `Dismiss` and fails as "wants
someone to confirm this", with the advice to allow it without confirmation
or to use the key file. Dismissing a prompt leaves the item as it was.

The session is the specification's `dh-ietf1024-sha256-aes128-cbc-pkcs7`,
as the crate used: Diffie-Hellman in RFC 2409's 1024-bit group with
generator 2, HKDF-SHA256 of the shared value padded to 128 bytes into an
AES-128 key, and AES-CBC with PKCS #7 padding and a fresh IV per secret.
Exponentiation uses `crypto-bigint`'s constant-time Montgomery arithmetic,
and a public value of 0, 1, p − 1, or outside the group is refused.
`aes`, `crypto-bigint`, `hkdf`, and `sha2` were already in the dependency
tree in these versions; only `cbc` is new.

```text
Uninitialized ──init──▶ Unlocked ◀──unlock── Locked
                           └────lock, stop────▶┘
```

`init` goes straight to unlocked, because it just held the key. It records
the installation as unfinished, writes the key, then marks it finished.
If the endpoint then cannot start, the identity stays saved and locked, and
`unlock` tries again. Startup and a later `init` remove only an
initialization that never finished, deleting only the key it wrote; a
finished one is never replaced without `reset`. `reset` deletes the key first and then
forgets trust, transfers, and the offer window. Hyakume's files stay in the
ledger until they are deleted.

The installation record is what finds the key, so it is forgotten only
after the key is confirmed gone. A locked or unreachable keyring, or a key
file that cannot be removed, stops `reset` with nothing forgotten. An
unfinished initialization whose key cannot be removed keeps its record;
the next start or `init` tries again first. If the old keyring is gone for
good, deleting the state directory is the way out, and it leaves the old
key where it was.

Locked, the service answers status, devices, transfers, inspect, cancel,
remove, and cleanup, but runs no peer endpoint. Expiry and cleanup go on.
Locking stops the node: work pauses, prompts are answered no, and the key
leaves memory with the node. Requests reach the node only inside a
synchronous closure (`Service::with_node`), so no request can hold it
across a wait and keep it alive after `lock`. A pairing started with
`pair --join` runs as a task of the node, so stopping the node waits for
it. If the node's work does not stop within the drain deadline, `lock` or
`reset` fails and the whole service stops instead of starting a new node
beside work that still runs; the next start recovers.

`init`, `unlock`, `lock`, and `reset` run as tasks the service owns, not as
parts of their requests. A client that disconnects stops waiting for the
answer, never the transition. Before, a disconnect during `lock` dropped
the node's shutdown halfway: the service already said locked, the drain was
lost, and the next `unlock` could start a node beside the old one's work.
`Node::shutdown` itself also keeps the accept task until it has seen it
end, and only reports success once no file job runs, so even a shutdown
that is cut short leaves the rest to the next call. Once the service is
stopping, no transition starts.

Stopping the service closes the socket, then asks every client to end. Each
ends its request and gets up to 5 seconds for what is still queued for it;
a client that has not ended after 6 seconds is cut off. The connection
that sent `stop` asks the service to stop only after the reply is in its
queue. Before, `stop` signalled first and shutdown aborted every client at
once, so about one `hyakume service stop` in 500 lost its reply and exited
with an error although the service stopped.

## 7. Local IPC

- The socket is `$XDG_RUNTIME_DIR/hyakume/hyakume.sock`, mode 0600, in a
  0700 directory that must belong to this user and must not be a link. A
  socket left by an earlier run is replaced only while the instance lock is
  held.
- The state and runtime directories each hold a `service.lock`. When both
  are one directory, under one path or two, the service takes its lock
  once: the files are compared by device and inode after opening. Taking
  it twice used to fail, because `flock` belongs to the open file, so the
  second lock refused the process that held the first, and the service
  never started.
- Every connection's peer credentials must show this user's UID.
- A frame is a 4-byte length, a kind byte, and a body. Message and data
  bodies are at most 1 MiB, secret bodies at most 4 KiB, and the length is
  checked before allocation. Messages are versioned JSON with request IDs,
  unknown fields and operations are refused, and errors carry stable codes.
- A frame read in part survives a cancelled wait: `FrameReader` keeps the
  bytes read so far, and each wait is a single read. The service and the
  command line both race reading against other work.
- At most 8 clients connect at once; the ninth gets a `busy` reply.
- `transfers` lists one summary per kept transfer, with an item count, so
  the list of all 256 always fits in a frame; `inspect` shows the items.
- One request runs per connection: zero or more events, then one reply.
  The reply goes out only after the request has ended, so a client that
  asks again as soon as it has a reply is never told `busy`. Input that
  arrives when no request wants it is dropped.
- **Prompts belong to one connection.** The node's questions go to the
  client that owns them: verifier words to the client running that pairing,
  offers to the client receiving. An answer must come on the same
  connection, name the prompt, and repeat its context: a hash of the
  invitation, peer, and verifier, or the offer digest. Anything else gets
  `answer-refused` and approves nothing. Disconnecting answers every open
  prompt no. An answer that arrives after the node stopped waiting is
  refused.
- A question the node stopped waiting for, because the offer was withdrawn,
  its session ended, or it timed out, gives its place back. A client holds
  at most 8 open questions; an offer that finds no place, or nobody
  receiving, is refused as `busy`, never as the person's decline.
- The receiving client and the node's destination change together, under
  one lock. Otherwise a client that stopped receiving could clear the
  destination of the client that had just taken its place, and that client
  would see every offer refused as `busy`.
- Each client has a queue of 64 events. A client that lets it fill is
  disconnected and must reconnect for a fresh view. What is still queued
  for a departing client gets 5 seconds to reach it; a client that stopped
  reading is then cut off, so it cannot keep its connection slot. This
  version sends no progress events, only questions and outcomes, so there
  is nothing to coalesce yet.

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
admission limits behind them (sessions, transfers, clients). The in-memory
staging limits of the loopback proof (64 MiB and 1,024 held items) are gone,
because received bytes now go to disk as they arrive.

## 9. Starting and stopping

```text
start: locations ─▶ instance lock ─▶ open, check, migrate ─▶ recovery
       ─▶ socket ─▶ "listening" ─▶ unlock if the Secret Service allows it
stop:  close the socket ─▶ end client tasks (prompts answer no)
       ─▶ stop the node (accepted work pauses) ─▶ wait for tasks and
          blocking jobs, sharing one deadline
```

A send recorded but still waiting for its task when the node stops never
runs; its transfer is ended by a guard the send carries from the moment it
is recorded, so nothing stays offered with no task behind it.

Each blocking job holds a permit until it really ends, even if the task
that started it is gone: file work holds one of the node's, and the
service's own jobs one of the service's. A drain therefore succeeds only
after taking back every permit of both. The rename that publishes an item
is the exception; it runs inside the state lock so revocation cannot slip
in before it, and costs one rename and one directory sync. If the deadline passes, the drain
fails: `service run` exits with status 1 and reports it, tasks are aborted,
blocking work that still runs is not counted as finished, and the next
start recovers. A worker that panics still stops the process.

## 10. Not in this version

- **Routes are explicit.** `hyakume status` shows the device's route and
  `send --at ROUTE` names the receiver's. The endpoint key is new every
  run, and finding devices on a LAN is the discovery work. A route carries
  the listening address as it is, so `service run --listen` refuses
  `0.0.0.0` and `[::]`, which no other device can dial; to reach another
  machine, listen on this machine's LAN address. Advertising every
  interface's address belongs to discovery.
- **No resume and no progress events.** Paused work waits, as above.
- **A disk that fills during a receive is tested by injection.** A real
  full disk is tested at both refusal points: an offer that would leave less
  than 64 MiB free, and input that fills the spool's disk. Both run on small
  tmpfs mounts in an unprivileged user namespace (`mise run
  test-full-disk`). A write failing halfway through a received item is
  injected where the partial file is created, because the free-space check
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
| A crash after the rename, and before it with an identical file already present | `a_crash_after_the_rename_is_recovered_as_published`, `a_crash_before_the_rename_keeps_the_partial_private_until_its_work_is_cancelled` |
| A sealed spool across a sender crash, and its release | `a_sender_crash_keeps_the_sealed_spool_of_its_paused_transfer_across_restart` |
| Oversized, timed-out, and cut-off input | `standard_input_that_is_too_large_or_too_slow_offers_nothing` |
| Linked, shared, and unsupported folders; no terminal | `an_unusable_destination_is_refused_before_any_offer_is_shown` |
| Encrypted key at rest, locked after restart, wrong passphrase, no echo | `a_locked_service_keeps_its_key_on_disk_encrypted_and_refuses_work_until_unlocked` |
| A Secret Service that never answers | `an_unresponsive_secret_service_is_refused_quickly_instead_of_hanging` |
| A Secret Service in an isolated keyring | `a_secret_service_key_unlocks_at_start_and_is_deleted_on_reset` (`mise run test-keyring`) |
| A Secret Service that wants someone to confirm creating or deleting the key | `secrets::tests::a_secret_service_that_wants_confirmation_never_gets_to_ask` |
| The session key exchange | `secrets::tests::both_sides_of_the_key_exchange_agree_and_trivial_values_are_refused` |
| A folder sync failure after the rename, and the paused transfer's place | `receive::tests::a_rename_whose_folder_cannot_be_synced_is_never_acknowledged` |
| Making room when deleting the oldest transfer fails | `storage::tests::transfers_that_failed_to_make_room_still_make_room_later` |
| Oversized frames, unknown operations, stale answers, the ninth client | `the_local_socket_refuses_bad_frames_stale_answers_and_extra_clients` |
| A second service for the same state | `a_second_service_for_the_same_state_is_refused` |
| A drain that overruns because a blocking job is stuck | `node::tests::a_drain_that_overruns_fails_and_never_counts_stalled_work_as_finished` |
| A shutdown cut short, then called again | `node::tests::a_shutdown_cut_short_leaves_its_work_to_the_next_one` |
| Sends still queued when the node stops | `node::tests::sends_still_queued_when_shutdown_begins_never_stay_offered` |
| A partial file whose inode cannot be saved, or whose reservation is gone | `node::tests::a_partial_file_whose_record_cannot_be_saved_is_never_used`, `a_partial_file_whose_reservation_is_gone_is_never_used` |
| A revoke that arrives before the partial file exists | `tests/loopback.rs` `revoking_the_sender_mid_transfer_publishes_nothing_and_refuses_later_sessions` |
| Invitations open when the device locks | `node::tests::invitations_open_when_the_node_stops_leave_room_for_the_next_node` |
| A client that stops reading its events | `a_client_that_stops_reading_gives_its_place_back` |
| A state folder under a linked ancestor | `paths::tests::folders_under_a_linked_ancestor_are_kept_by_their_real_path`, `a_state_folder_under_a_linked_ancestor_still_cleans_up_its_spools` |
| Input still queued when its deadline passes | `service::tests::input_still_queued_when_its_deadline_passes_is_refused` |
| The reply to `stop` | `the_stop_command_is_always_told_the_service_stopped` |
| Transfers that ended just before a crash | `storage::tests::transfers_that_ended_just_before_a_crash_still_make_room_after_it` |
| Unfinished input whose spool cannot be deleted | `unfinished_input_that_cannot_be_deleted_stays_charged_until_cleanup_deletes_it`, `source::tests::a_spool_that_cannot_be_deleted_says_what_is_left` |
| A ledger row naming a path outside its folder | `storage::tests::a_file_row_whose_name_could_leave_its_folder_is_refused`, `destination::tests::removal_never_reaches_outside_its_folder` |
| Questions the node gave up on, and one nobody took | `service::tests::questions_the_node_stopped_waiting_for_hold_no_place`, `a_question_dropped_without_an_answer_is_refused_as_busy_not_declined` |
| A transfer started after the wall clock went back | `receive::tests::work_received_after_the_wall_clock_went_back_keeps_its_whole_window` |
| Lifetime-clock readings less than a millisecond apart | `clock::tests::readings_less_than_a_millisecond_apart_still_add_up` |
| Restart, the epoch counter, a newer schema, damaged rows, a device name with terminal controls | `storage::tests` |
| One folder for state and socket | `paths::tests::one_folder_for_state_and_socket_is_locked_once` |
| Recovery rules | `recovery::tests` |
| A write failure while receiving | `tests/loopback.rs` `a_partial_file_that_cannot_be_written_fails_the_item_and_publishes_nothing` |
| Real full disks: a receiving folder and a spool directory | `a_full_disk_refuses_offers_and_input_and_leaves_nothing_behind` (`mise run test-full-disk`) |
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
- **Secret Service with a 3 s timeout and no unlock calls: go.**
- **Partial files beside the output, published by `renameat2` with
  `RENAME_NOREPLACE` into qualified folders on ext4, btrfs, XFS, or tmpfs:
  go.**
- **One spool pipeline for standard input, text, and links: go.**
