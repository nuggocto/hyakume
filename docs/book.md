# The Hyakume build book

What got built, how it works, and why, one step at a time, newest last. The
decision records describe current behavior; this book says how it got
there. When a later step replaced an earlier one, the earlier entry keeps
only what changed and why.

- [Pairing, identity, sessions, and the wire protocol](pairing-identity-session.md)
- [The local service, its storage, and the file pipeline](local-service.md)
- [Resource estimate](resource-estimate.md)

## 1. The trust and transfer core

**What got built.** A library that pairs two devices, proves who they are on
every connection, and moves text and files between them with the person's
consent. Two endpoints ran the whole journey over real QUIC connections
(Iroh 1.3.0) on 127.0.0.1, with received items kept in memory.

```text
src/
├── identity.rs     device key: no Clone, redacted Debug, wiped on drop
├── trust.rs        trust records; every grant of trust gets a new epoch
├── invitation.rs   invitations and claims: used once, 5 failed attempts, 15 minutes
├── pairing.rs      SPAKE2 exchange bound to the QUIC connection, 4-word verifier
├── session.rs      trusted devices prove who they are on each connection
├── offer.rs        offer checks: exact sizes, digests, item and size limits
├── transfer.rs     item states; the batch outcome is computed from them
├── receive.rs      receiving side, a state machine; no file or network I/O
├── send.rs         sending side, the same
├── protocol.rs     17 message types, strict decoding, lengths checked first
├── display.rs      escaped projections of names and previews
├── consent.rs      the accept or reject screen
├── limits.rs       compiled limits and the buffer budget
├── state.rs        one lock that orders revocation against publication
├── tasks.rs        a panicking task stops the process
└── node.rs         runtime: owns the endpoint, connections, and tasks
```

**Why this shape.** The modules that decide (`pairing.rs`, `receive.rs`,
`send.rs`) never touch the network. They take a decoded message and return
what to do next, so a duplicate, reordered, oversized, or hostile message
gets an exact unit test. The node only moves bytes between them and QUIC.

### Pairing

The joiner holds an invitation token with a 256-bit secret. Both sides run
SPAKE2 with that secret, prove they reached the same key, and only then
reveal their long-term device keys.

```diagram
 joiner                                          inviter
 Hello ⇄ Hello  (both versions go into the transcript)
 Claim(invitation, claim) ──────────────────▶   opens the claim (8 at most, 5 failures)
 Commit ────────────────────────────────────▶
                 ◀──────────────────────────    Share
 Reveal ────────────────────────────────────▶   checks it matches the Commit
                 ◀──────────────────────────    Confirm MAC   ← a wrong secret stops here
 Confirm MAC ───────────────────────────────▶
                 ◀━━━━━━━━━━━━━━━━━━━━━━━━━━    device key + signature  ┓ only after
 device key + signature ━━━━━━━━━━━━━━━━━━━━▶                           ┛ both MACs
 both screens show four words, such as "sugar arrow globe wizard"
 Decision ⇄ Decision          both yes → saved on both sides
```

The transcript binds the TLS exporter, both endpoint IDs, the version
exchange, the invitation, and the claim. A relay in the middle, a downgraded
version, or a claim replayed on another connection fails the MAC check. The
joiner commits to its contribution before seeing the inviter's, so neither
side can steer the four words. A connection that drops between the two
answers can still leave trust on one side only; the fix is to remove the
device and pair again.

### Sessions and transfers

```diagram
 sender (send.rs)                       receiver (receive.rs)
 SessionProof ───────────────────────▶ checks trust first, then proves itself
 Offer {names, sizes, digests} ──────▶ limits, room, capacity → consent screen
              ◀───────────────────────  OfferAccept (names the offer digest) │ OfferReject
 Chunk, Chunk, … ItemEnd ────────────▶ every chunk: trust still valid? offset and length right?
              ◀───────────────────────  ItemResult (length and BLAKE3 checked, then commit)
```

Session proofs are Ed25519 signatures over the connection's exporter, and
each role signs under its own label, so a proof cannot be replayed or
reflected. The listener checks the dialer before it reveals its own
identity.

### Revocation against publication

Every piece of work carries an `Authority` naming the trust epoch it was
authorized under. Revoking a device, or pairing it again, starts a new
epoch, and older authorities stop working. Revocation and publication go
through the same lock in `state.rs`, so whichever arrives first decides.

```diagram
 Revocation first                         Publication first
 revoke → epoch changes                   commit: epoch still valid → Published
 item verified → commit refused           revoke → the remaining items are cancelled
 ● item Cancelled(Revoked)                ● the published item stays published
```

### Limits and peer text

Every count and size a peer influences has a compiled maximum in
`limits.rs`, and configuration can only lower one. Capacity comes as permits
that give it back when dropped. Frames and chunk copies lease from one
128 MiB buffer budget; when it is full, the reader stops pulling from QUIC
and flow control pushes back on the peer.

A peer chooses device names, file names, and previews. `PeerText` keeps the
original bytes and nothing prints them directly. `SafeLine` and `SafeBlock`
escape control, line-break, bidirectional, and invisible characters, and
stop at a column limit with a visible marker. Previews sit behind a fixed
gutter, so a hostile name cannot add a line or imitate the question.

## 2. Hardening the core

Five outside reviews of the core, a design review of pairing, and a code
review followed. Each fix below has a test that failed with the fix
reverted. The [pairing record](pairing-identity-session.md) lists the
design and code review findings with their outcomes.

**Nothing waits without a way out.** A peer that stopped reading left the
writer waiting forever with its resources held. Every frame write now races
the idle timeout and the transfer's revocation or cancel; when it loses, the
stream is reset, because a frame cut halfway cannot be continued. A ninth
question for the person is refused as busy at once instead of waiting for
room. Waiting for buffer room before a chunk also notices a cancel, a
revocation, a reply, and a closed connection. A dialer finishing its stream
stops waiting for its reader once the listener has closed the connection.

**Two limits that only worked alone.** Two devices sending to each other on
a small budget could each fill it with the other's data and then have no
room for their own first chunk. The budget now has two lines:

```diagram
 buffer budget (128 MiB; never below 5 MiB)
 ┌──────────────────────────────────────────────┬─────────────────┐
 │ chunks being sent                            │ one frame read  │
 │                                              │ 2 MiB           │
 └──────────────────────────────────────────────┴─────────────────┘
  sending stops here ━━━━━━━━━━━━━━━━━━━━━━━━━━━┛                 ┃
  frames being read may use all of it ━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
```

A budget below 5 MiB is raised to it, so a device can always send one full
chunk and read one full frame. Connections are counted the same way:

```diagram
 open connections, at most 40
 ┣━ one per session, incoming or outgoing ············ 32
 ┗━ no session yet: pairing, or proving who they are · 8   the 9th incoming is
                                                           refused before the handshake
```

A stranger opening connections without proving who it is can fill only the
8 places, so it never stops this device's own sends.

**Deadlines count from fixed instants.** Each step before a peer's proof had
its own 30 s timeout, so a peer answering each step just in time held a
place for minutes. Now `serve` and `dial` give the `Wire` one instant, and
`Wire::cap` limits every read, write, and closing drain to it until
`end_handshake`. The idle timeout restarted whenever any state changed, so a
busy node kept an unanswered offer open forever. `Wire::idle_until` now
counts from the session's own last frame.

**Pairing settles itself.** The inviting side settled its claim by hand on
each error path, and one path was missing: a joiner that vanished while the
words were on screen left the invitation open for a second comparison.
`PairingOwner` is created when the claim opens and settles it when dropped,
so an early return, a lost connection, and an aborted task reach the same
code. A yes now takes a `Reservation` of the alias and one trusted place, so
a second pairing cannot take the alias while the first waits for the other
person. Claims in progress hold the attempts they could use, so eight claims
at once never get more than the five guesses allowed.

**Smaller fixes.** Shares outside the prime-order group are refused before
`spake2` sees them. The vectors now give a second implementation every
scalar and the transcript, and the test proves them from the messages. Text
that fails its content check answers `ContentMismatch`. Repeated transfer IDs
have their own window of 4,096, separate from the 256 transfers kept for
status. Each visible character keeps at most four zero-width ones, so a run
of combining marks cannot grow a projection.

One change from this period was later replaced: an in-memory staging budget
for received bytes, gone once received bytes went to disk (step 4).

## 3. A simpler repository

The threat-model file is gone, decision records moved to `docs/`, progress
lives only in `PROJECT.md`, and the README explains the project and the
build. Every document that repeated status had gone stale within a day. The
protocol record went too: `src/protocol.rs` and the published vectors are
the contract, and the facts they do not show in one place moved into the
pairing record.

Generated files left the tree. The fuzz corpus grew with every run and gave
no head start, so `/fuzz/corpus/` is ignored. Proptest's three saved seeds
became named unit tests, and `failure_persistence: None` stops new seed
files; a failure still prints the shrunk input, which should become a named
test the same way.

## 4. The local service, command line, and safe files

**What got built.** Hyakume is a command. `hyakume service run` starts the
one service that owns this user's installation, and every other command is
a client over a local socket: `init`, `status`, `unlock`, `lock`, `pair`,
`devices`, `remove`, `receive`, `send`, `cancel`, `inspect`, `transfers`,
`cleanup`, and `reset`. State lives in SQLite, the device key in the Secret
Service or a passphrase-encrypted key file, and received items become real
files only after their length and digest match. The
[local service record](local-service.md) holds the decisions.

```text
src/
├── cli.rs, terminal.rs   the command: arguments, prompts, exit codes
├── service.rs            the owner: startup, lock lifecycle, requests, shutdown
├── ipc.rs                the local protocol: frames, messages, limits
├── storage.rs            SQLite: schema, migrations, checked loading
├── recovery.rs           startup reconciliation, expiry, cleanup
├── secrets.rs            key file (age) and Secret Service
├── paths.rs              private directories and the instance lock
└── files/
    ├── names.rs          which offered names can become file names
    ├── destination.rs    qualified folders, partial files, no-replace rename
    └── source.rs         prepared files and sealed spools
```

### Who owns what

```diagram
hyakume send ─┐                    ┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
hyakume receive ─┼── Unix socket ━━▶┃ service                           ┃
hyakume pair ─┘  (this user only)  ┃   StateCell: Mutex<State + SQLite>┃
                                   ┃   Node: endpoint, sessions, sends ┃
                                   ┃   blocking file jobs (4 permits)  ┃
                                   ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
                                         │              │
                                   hyakume.db     spools, partial files,
                                   key file       published files
```

A client never touches state. A send belongs to the service once it starts:
if its client goes away, the transfer goes on, and only `cancel` stops it.
Questions for the person belong to the client that is pairing or receiving.
They travel to it as events with a random prompt number and a context, and
only an answer on that connection, with that context, counts.

The state lock that ordered revocation against publication now also orders
durable writes. The SQLite connection lives inside `State`, so each decision
and its record share one critical section. A change that claims success is
saved before memory changes; a change that ends work is applied first and
saved after.

### Receiving into real files

```diagram
Chunk ─▶ Receiver ─▶ Output::Write ─▶ runtime, outside the lock:
                                      pwrite to .hyakume-<id>-<n>.part (0600), hash as written
ItemEnd ─▶ Output::Verify ─▶ runtime: fsync partial
           on_verified, under the lock:
             may_publish: cancelled? trust epoch same?
             renameat2(partial, name, NOREPLACE)
             fsync folder
             save: item published
           ─▶ ItemResult::Published
```

Memory no longer grows with the file. The rename runs under the state lock,
so a revocation either comes first, and the item is never published, or
after, and the published file stays. `RENAME_NOREPLACE` alone decides
whether a name is free; a file already there fails the item as `exists` and
is not touched. A receive folder is qualified first: no symbolic link in its
path, nobody else may write to it, ext4, btrfs, XFS, or tmpfs, and a probe
that shows the rename really refuses an existing name.

### Crashes, sending, and owned files

The output name is saved at acceptance and the partial file's inode as soon
as it exists, so after a crash recovery can tell from the folder whether the
rename happened. Debug builds have crash points (`HYAKUME_CRASH_AT`) that
stop the service between those steps, and the service tests check both
sides of the rename with real processes. Work cut off by a stop, a lock, or
a crash pauses for 7 days with its files and quota.

Files are opened without following links and hashed before the offer, and
hashed again while sent; a changed source fails the item. Standard input,
text, and links go to a spool that is sealed read-only before anything is
offered. Spools and partial files are rows in one ledger table that charges
their bytes until they are published or deleted, and a cleanup pass every
5 seconds deletes what is due.

### Keys and packaging

```diagram
Uninitialized ──init──▶ Unlocked ◀──unlock── Locked
                            ┗━━━━lock, stop━━━━▶┛
```

A key in the Secret Service unlocks at start without anyone typing; a key
file waits for `hyakume unlock`. Hyakume never asks the keyring to unlock,
so an SSH session never waits on a desktop prompt. Invitations stay in
memory, because a token names an endpoint key that is new every run.

`mise run package` builds a reproducible archive with the binary, license
notices for every crate compiled in, a systemd user unit, and build
information. `mise run qualify` runs that exact binary through the whole
journey in a fresh container with no network beyond loopback.

## 5. Packaging tasks in Rust

Two shell scripts and a Python script became one Rust package, `xtask`, run
as `cargo xtask package` and `cargo xtask qualify`. The archive is written
in Rust, folders first, every entry owned by root and stamped with the
commit's time, so the same commit gives the same bytes. Windows needs the
same steps, and shell and Python would not run there. The seven crates the
tasks add reach only `xtask`, never the shipped binary. The dependency walk
that decides which license notices ship now has a test.

## 6. Hardening the service

Twelve reviews of the service followed. Each finding was checked against the
code before anything changed. Most shared one cause: work that had to finish
was owned by something that could be cancelled, or two components each
followed their own rule and disagreed between them.

**Work belongs to what outlives it.**

- Requests reach the node only through `Service::with_node`, a synchronous
  closure, so no request keeps a stopped node and its key alive.
- `init`, `unlock`, `lock`, and `reset` run through `Service::own`, so a
  client that leaves ends its wait, never the transition. A drain that
  overruns stops the whole service instead of starting a node beside work
  that still runs.
- From the end of input on, sealing the spool, its ledger row, and starting
  the send are one service task. `InputSpool` keeps unfinished input
  charged until its file is gone.
- A queued send carries the guard that ends its transfer from the moment it
  is recorded, so a send still queued at shutdown never stays `offered`.
- A reply goes out only after its request has left the connection's slot,
  and the connection that ran `stop` signals the service only after the
  reply is queued. Shutdown lets clients drain their queues for up to
  5 seconds before cutting them off.

```diagram
Before
client: lock ──▶ request task ──▶ node.shutdown()
client leaves ──▶ ● request aborted, drain dropped halfway

After
client: lock ──▶ request task (waits)
                 service task ━━▶ node.shutdown() runs to its end
client leaves ──▶ only the wait ends
```

**The ledger and the disk agree.**

- Row IDs never repeat, cleanup passes run one at a time, and a late result
  changes a row only while it is still marked for deletion.
- A row must name exactly the file Hyakume gives its kind, and deletion
  refuses anything but a single name, so a damaged row cannot reach outside
  its folder.
- A partial file whose inode cannot be recorded, or whose reservation is
  gone, is removed before any byte is written to it.
- A spool found at start with no row is recorded for deletion and stays
  charged until cleanup deletes it. A leftover spool is charged by its real
  size.
- Sealing makes the spool read-only before the sync, so the mode is durable
  with the bytes.
- Making room for a new transfer pops the oldest finished one only after its
  row is deleted, and a transfer that ended just before a crash gets its
  finish order at the next start.

**One clock for every deadline.** Every deadline is stamped and checked with
`State::now_ms`, a lifetime clock that never runs behind its last reading
and follows the monotonic clock while the service runs. Each saved reading
carries the wall clock's time, so a restart can measure how long the service
was stopped, and expires paused work when it cannot. The clock carries
fractions of a millisecond, so frequent readings still add up.

**Two components, one rule.**

- An offer that validates also encodes: `Offer::validate` measures the
  payload with the encoder's own writer. Before, a batch of long names was
  recorded, refused by the encoder, and then refused by loading at the next
  start, which kept the service from starting.
- A permanent invitation allows one comparison of words: the first claim to
  verify takes it, and a later one is refused before any prompt.
- A name finds only a trusted device, never a revoked record with the same
  alias.
- A question the node gave up on frees its place, and the receiving client
  and the node's destination change together under the routes lock.
- The device's own name is checked as it loads and escaped when shown.

**The Secret Service never gets to show a prompt.** The `secret-service`
crate answered a keyring's confirmation request by showing the prompt and
waiting, which opened a window nobody on SSH could answer, and its D-Bus
calls were private. `src/secrets.rs` now calls the Secret Service through
`zbus` itself and dismisses any prompt unseen, failing with
`SecretError::Confirmation`. The session keeps the same encryption,
Diffie-Hellman in RFC 2409's 1024-bit group with constant-time arithmetic.

**Smaller fixes.** The service resolves its state and runtime folders to
real paths once at start. One folder for state and socket takes its lock
once. A finished identity is never replaced by `init`. The input deadline
holds when a backlog of frames is already queued. A client that stops
reading gets 5 seconds for what is queued, then loses its place. Echo goes
off before the passphrase prompt appears. The keyring test's shell script
and its process group are bounded by deadlines.

## 7. Unconfirmed renames

A rename into the destination can succeed while the folder sync after it
fails. Nobody knows then whether the name survives a crash. The rule changed
six times:

1. Counting the item as published. The sender heard `published` about a
   file nobody knew to be durable.
2. Pausing it. Anything that ends paused work, a cancel, a revocation, or
   expiry, then ended it too, and the file sat under its final name while
   the transfer said cancelled.
3. A state of its own, `Unconfirmed`, that cancel, revoke, and expiry skip.
4. Treating a folder that cannot be read as unconfirmed too, not as lost.
5. Carrying a skipped cancel or expiry in memory and in a saved deadline.
   This was taken out as more than the case was worth, which broke the
   plan's rule that revocation cancels paused work.
6. Keeping the first skipped stop inside the state itself.

```diagram
Unconfirmed(None) ── cancel / revoke / expire ──▶ Unconfirmed(Some(stop))    first stop wins
Unconfirmed(stop) ── disk: final file ──────────▶ Published                  stop dropped
Unconfirmed(Some) ── disk: partial only ────────▶ the stop: Cancelled or Expired, partial deleted
Unconfirmed(None) ── disk: partial only ────────▶ Paused
```

The state saves as its name, such as `unconfirmed:cancelled:revoked`, so no
new column is needed. Startup recovery and each 5-second maintenance pass
share one check: the file under its output name counts as published only if
a new folder sync succeeds, and an error that says nothing about the file
leaves the item as it is. A published item keeps no stop, because its rename
happened under the state lock before trust changed. The parser for these
names matches the two allowed suffixes directly, so a damaged value with a
million nested prefixes fails as damaged instead of overflowing the stack.

## 8. A finished transfer keeps no content

A text offer carries a preview of up to 4 KiB, which for short text is the
whole payload, and the database kept it as long as the transfer stayed in
history. Now the write that settles a finished transfer replaces its offer
with one that has no preview, and `secure_delete` zeroes the replaced bytes.
The offer keeps the digest computed at validation, saved in its own column,
so a finished transfer still names what was accepted.

Schema 2 is the first migration. `VACUUM` runs first, while the file still
says schema 1, then one transaction drops the old previews and records the
new version. A stop anywhere before the commit redoes the whole step.

Two tests claimed more than they checked and were renamed to say what they
cover. An unused `Redacted` wrapper was removed: no code called it, and the
secrets that exist print as `[redacted]` from their own `Debug`.

## 9. The shutdown deadline, files that lie about their size, and smaller modules

A review of the whole tree at `0ed8b04` found two defects and asked for
simpler code and tests. Both defects were reproduced before they were fixed.

**Shutdown now starts its deadline before it waits for anything.**

```diagram
Before
shutdown ─▶ lock(state) ━━ stalled ━━┓
                                     ┃ no deadline yet
deadline = now + drain ◀━━━━━━━━━━━━━┛
close endpoint, join tasks
lock(state): clear invitations
● Ok(()) after the stall

After
deadline = now + drain
pausing: atomic, set without the lock
close endpoint, join tasks          ┐
take every file-job permit          │ each wait ends
clear invitations in a job holding  │ at the deadline
  those permits                     ┘
lock still held ━━▶ ● DrainFailed in time
```

`Node::shutdown` used to take the state lock to set its pausing flag, and
only then start its drain timer. A publication rename or a database write
stuck on a disk holds that lock, so shutdown waited for it with no deadline.
A lock held for 300 ms against a 30 ms deadline gave `Ok(())` after 301 ms.

The pausing flag is now an `AtomicBool` that the state and `StateCell`
share. Clearing invitations still needs the lock, so it runs as a blocking
job that holds every file-job permit. If the deadline passes first, shutdown
fails, and the job keeps the permits until it ends, so the next call waits
for it instead of succeeding early. Removing the lock was not an option:
revocation and publication must stay in one order.

A deadline only helps if its timer can run. A task waiting for the state
lock blocked its worker thread, and on a runtime with few workers that left
none to drive timers.

```diagram
worker 1: task ─▶ lock(state) ━━ waits ━━▶ block_in_place:
                                            its other tasks and the
                                            timer move to a new thread
shutdown's timer fires ━━▶ DrainFailed
```

`StateCell::lock`, `change`, and `wake` go through `tasks::blocking_here`,
which uses `block_in_place` on the service's multi-thread runtime and simply
runs on a single-thread one, as in most unit tests. An uncontended call
costs about 25 ns instead of 11 ns. A `Drop` impl that takes the lock while
the runtime shuts down still works, which was checked separately because
guards such as `ReceiverGuard` do exactly that.

Two tests cover the two halves.
`a_state_lock_held_past_the_drain_deadline_fails_the_drain_in_time` holds
the lock from another thread. `a_task_stalled_inside_the_state_lock_cannot_starve_the_drain_timer`
stalls a task inside the lock on a runtime with one worker. Each releases
the lock only after shutdown returns, within a 3 s bound. Against the old
code, each waits out those 3 s and fails.

**A file must now end where its size says.**

```diagram
Before   fstat: size 0 ─▶ hash 0 bytes ─▶ ● offered as an empty file
After    fstat: size 0 ─▶ hash 0 bytes ─▶ read 1 byte at 0: got one
                        ─▶ ● refused: send its content as standard input
```

Files under `/proc` report a size of zero and still have bytes to read, so
`/proc/version` was offered and received empty. Standard input goes through
the spool, which gets an exact length. A file whose identity changed during
preparation is still reported as changed, because that check comes first;
only a file that kept its identity is refused for its size.
`Source::unchanged` makes the same check before the sender reports an item
sent. Through the CLI, the refusal comes before any offer, and sending the
same bytes on standard input delivers all of them.

**The two largest files are now split by responsibility.**

```diff
 src/
-├── node.rs
+├── node/
+│   ├── mod.rs          Node, Shared, shutdown
+│   ├── connections.rs  bind, dial, accept loop
+│   ├── exchange.rs     both sides of pairing
+│   ├── sending.rs      send sessions
+│   ├── receiving.rs    receive sessions, publication
+│   └── wire.rs         frames on one QUIC stream
-├── service.rs
+└── service/
+    ├── mod.rs          Service, run, maintenance
+    ├── clients.rs      socket connections, requests, prompts
+    ├── lifecycle.rs    init, unlock, lock, reset
+    ├── pairing.rs      invite, join
+    ├── transfers.rs    receive, send, spools, cancel
+    └── views.rs        status, devices, transfers
```

No new traits came with the split. `transfers` builds each summary straight
from state under one lock and one pass over the file ledger, instead of a
full view per transfer whose items it then dropped. `Node::send`,
`Node::join`, and `Payload` existed only for tests. The loopback tests now go
through `start_send` and `start_join`, the paths the service uses, and keep
`Payload` in their own file. The two tests that relied on dropping a
caller's future now stop the node, as a locked service does, and check the
exact result on both sides.

**Tests now check what they say.** The consent test no longer counts the
screen's lines, which broke on any new line of text; it checks that neither
the forged question nor a forged field starts a line. The JSON round trip of
IPC messages is gone: matching derives pass it whatever they do, and the
binary journeys exercise those messages. The identity-disclosure test
searches every encoded message sent before both confirmations, the claim
included, so a field added to any of them is covered. It confirmed that
declared names travel with the shares, as the pairing record says, and
device keys do not. The 200 restarts of the stop test moved to
`mise run test-stop-stress`, with one stop left in the routine suite.

Three fixtures gained owners. The silent bus thread stops when its test
ends. A service that never becomes ready is still killed. The fatal tests
read the child's output while it runs and kill a child that hangs instead of
aborting.
