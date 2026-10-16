# The Hyakume build book

What got built, how it works, and why, one step at a time. The newest step
is at the end. Decision records sit next to this file:
[pairing, identity, sessions, and the wire protocol](pairing-identity-session.md),
[the local service, its storage, and the file pipeline](local-service.md),
and the [resource estimate](resource-estimate.md).

## 1. The trust and transfer core

**What got built.** A library that pairs two devices, proves who they are on
every connection, and moves text and files between them with the person's
consent. Two endpoints run the whole journey over real QUIC connections
(Iroh 1.3.0) on 127.0.0.1. The `hyakume` command still prints only help and
version, and received items stay in memory.

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
what to do next, so a duplicate, reordered, oversized, or hostile message gets
an exact unit test. `node.rs` only moves bytes between them and QUIC.

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
 can I still save this trust? ┳━━━━━━━━━━━━┳ can I still save this trust?
 Decision ⇄ Decision          ┗━ both yes ━┛ → saved on both sides
```

The transcript binds the TLS exporter, both endpoint IDs, the version
exchange, the invitation, and the claim. A relay in the middle, a downgraded
version, or a claim replayed on another connection fails the MAC check. The
joiner commits to its contribution before seeing the inviter's, so neither
side can steer the four words. Each side checks that it can still save the
trust before it answers yes, so a known conflict, such as a taken alias or a
full trust list, makes both sides decline. A connection that drops between
the two answers can still leave trust on one side only. The fix is to remove
the device and pair again.

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
reflected. The listener checks the dialer before it reveals its own identity.

### Revocation against publication

Every piece of work carries an `Authority` naming the trust epoch it was
authorized under. Revoking a device, or pairing it again, starts a new epoch,
and older authorities stop working. Revocation and publication go through the
same lock in `state.rs`, so whichever arrives first decides.

```diagram
 Revocation first                         Publication first
 revoke → epoch changes                   commit: epoch still valid → Published
 item verified → commit refused           revoke → the remaining items are cancelled
 ● item Cancelled(Revoked)                ● the published item stays published
```

Pairing again allows new transfers but never restarts the cancelled ones.

### Limits and backpressure

Every count and size a peer influences has a compiled maximum in `limits.rs`.
Configuration can lower a limit, never raise it. Capacity comes as permits
that give it back when dropped, so it cannot be released twice. Frames,
chunk copies, and staged bytes all lease from one 128 MiB buffer budget. When
the budget is full, the reader stops pulling from QUIC, and QUIC's flow
control pushes back on the peer.

### Peer text

A peer chooses device names, file names, and previews. `PeerText` keeps
those original bytes, and nothing prints them directly. `SafeLine` and
`SafeBlock` escape control characters, line breaks, bidirectional and
invisible characters, and spaces that only look like spaces, and stop at a
column limit with a visible marker. The consent screen puts previews behind a
fixed gutter, so a hostile name cannot add a line or imitate the question.

## 2. Fixes after the first outside review

Two reviewers read the core and found six defects. Each fix has a test that
fails when the fix is reverted.

**A stalled write could hang forever.** A peer that stopped reading left the
writer waiting with the transfer's resources held.

```diagram
Before                                 After
write_all(frame)                       write_or_stop(frame)
  peer stops reading                     ┣━ written ──────────▶ ok
  ● waits forever                        ┣━ idle timeout ━━━━━▶ ● reset stream, close
  revocation and cancel not seen         ┗━ revoke or cancel ━▶ ● reset stream, close
```

A frame cut halfway cannot be continued, so the stream is reset and the peer
fails its unfinished items. Waiting for a frame also ends when the connection
closes.

**A full question queue blocked revocation and shutdown.** With eight
questions waiting, a ninth offer waited for room and its connection stopped
reacting. It is now refused as busy at once. Shutdown keeps the accept task
when its deadline passes and stops it, so no connection task outlives it.

**Cancel could lose to publication.** An item's last message arriving just
after the person cancelled was still published. The receiver now checks for
a cancel request before every message, and the commit refuses a transfer
being cancelled.

**Empty received files had no limit.** They used no bytes, so they piled up.
The state now counts held items as well as bytes, up to 1,024.

**A 1-byte send reserved 3 MiB.** The sender now reserves what the next chunk
costs, and chunks shrink to fit a lowered budget. A real lack of room is
reported as busy, not as a lost connection.

**Previews hid that they were cut.** A cut line shows ` [shortened]`, and the
screen says `[preview shortened]` when any line was cut or the sender's
preview covers only the start of the text.

## 3. A simpler repository

**What changed.** The separate threat-model file is gone, and plans no longer
ask for one. The decision records moved from `decisions/` to `docs/`, next to
this book. The README now only explains the project and how to build it.
Progress lives only in `PROJECT.md`.

**Why.** One place for progress and one place for explanations. Every
document that repeated status went stale within a day of the code changing.

The protocol record went too. It mostly restated `src/protocol.rs`, which
with the published vectors is the real contract. The few facts the code
does not show in one place, such as the order of messages and how a failed
write ends a session, moved into the pairing record.

## 4. Fixes after the second outside review

The second review confirmed the first six fixes and found two more problems,
both about waiting for buffer room.

**Staging could starve frame reading.** The receiver leases the whole
accepted payload from the same budget that reads frames. With a small budget,
staging could take nearly all of it, and the next frame never fit.

```diagram
Before                                  After
accept → stage 60 of 64 KiB             offer → can 60 KiB be staged next to one frame?
read chunk → needs 4 KiB more             ┣━ no ━━▶ ● refused as too large, nobody asked
  ● waits forever, nothing published      ┗━ yes ─▶ accept, every frame still fits
```

Long-held leases, like staged bytes, now stay within the budget minus
`FRAME_HEADROOM`, which is room for the largest frame, raw and decoded. An
offer that could never fit beside it is refused before the person is asked.

**Waiting for buffer room ignored cancel and revocation.** Before each chunk,
the sender waited for a lease and noticed nothing else, possibly for the
full ten-minute idle timeout.

```diagram
Before                                  After
lease_wait(cost)                        wait for any of:
  ● cancel, revoke, replies unseen        ┣━ lease ──────────────▶ send the chunk
                                          ┣━ cancel or revoke ━━━▶ ● stop, release everything
                                          ┗━ reply or closed ━━━━▶ handle it
```

**How the tests make a device stop reading.** The stalled-write test used to
rely on the staging starvation above, which no longer happens. Nodes now take
an optional `read_gate` in `NodeConfig`. A test lets a few frames through and
then holds the rest, the same way `SendOptions::gate` holds a sender.

## 5. Fixes after the third outside review

The third review confirmed the second round of fixes and found two more
problems. Both came from limits that worked alone but not together.

**Two devices sending to each other could wait on each other forever.**
Staging left room to read a frame, but not to send one. With a lowered
budget, each device staged the other's file and then had no room for its own
first chunk. Neither sent anything, so neither transfer ever finished.

```diagram
A stages 1 MiB from B ──▶ A waits for 3 MiB to send ━┓
                                                     ┃ each waits for the
B stages 1 MiB from A ──▶ B waits for 3 MiB to send ━┛ other's first chunk
● nothing moves until the idle timeout
```

The buffer now has three kinds of lease, and each stops at its own line:

```diagram
 buffer budget (128 MiB; never below 5 MiB)
 ┌────────────────────────────┬──────────────────┬─────────────────┐
 │ staged bytes               │ one chunk sent   │ one frame read  │
 │                            │ 3 MiB            │ 2 MiB           │
 └────────────────────────────┴──────────────────┴─────────────────┘
  staged bytes stop here ━━━━━┛                  ┃                 ┃
  staged bytes + chunks being sent stop here ━━━━┛                 ┃
  frames being read may use all of it ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
```

Staging can never take the room to send one chunk and read one frame, so
every accepted transfer can still move. An offer that would eat into that
room is refused as too large before the person is asked. A budget set below
5 MiB is raised to it, because below that a device cannot send a full chunk
and read a full frame at once. Chunks no longer shrink to fit a small
budget, since every budget now fits a full one.

Two smaller fixes came with it. A session no longer keeps the Hello frame's
lease for its whole life. And a connection that is closing no longer waits
for its reader when the reader is stuck waiting for buffer room. The
connection's close is enough.

**More connections could be open than the estimate counted.** The limit of
40 only counted incoming connections. Outgoing sessions came on top, up to
72 in all. At 2.5 MiB of QUIC windows each, the estimate would have been
328 MiB, past the 256 MiB the service may use. Now every connection counts:

```diagram
 open connections, at most 40
 ┣━ one per session, incoming or outgoing ············ 32
 ┗━ connections with no session yet:
    pairing, or incoming ones still proving who
    they are ········································· 8   the 9th incoming is
                                                            refused before the
                                                            handshake
```

The two groups have separate limits on purpose. A stranger who opens many
connections without proving who it is can only fill the 8 places, so it can
never stop this device's own sends. It can still hold off incoming sessions
and pairing until the handshake deadline closes its connections, which was
already true with the old limit.

## 6. Fixes after the fourth outside review

**A sender could keep its connection after the receiver closed it.** When
a send ends, the dialing side finishes its half of the stream and reads
until the listener finishes too. That read only watched the reader task's
channel. If the reader was stuck waiting for buffer room, the channel never
ended, even though the listener had already closed the connection:

```diagram
Before                                After

receiver closes the connection        receiver closes the connection
        │                                     │
        ▼                                     ▼
send sees the close,                  send sees the close,
transfer ends                         transfer ends
        │                                     │
        ▼                                     ▼
finish_dialer waits for the reader    finish_dialer waits for the reader
        │                                     ┃ or the close, whichever
        ▼                                     ┃ comes first
reader waits for unrelated            ┏━━━━━━━┻━━━━━━━━━━┓
buffer room                           ┃ send returns,    ┃
        │                             ┃ connection freed ┃
        ▼                             ┗━━━━━━━━━━━━━━━━━━┛
send returns after the idle
timeout (10 minutes); its
connection holds one of the
32 session places until then
```

`finish_dialer` now races the drain against `conn.closed()`, the way
`finish_listener` already did. A closed connection has no more frames worth
waiting for. The reader task is aborted right after, as before.

The test `a_sender_whose_reader_is_stuck_still_ends_when_the_receiver_closes`
shuts the sender's read gate after the session's first two frames, so the
reader is stuck the same way a reader waiting for buffer room is. The
receiver then shuts down while the offer is still on screen. The send has to
return, with the offer withdrawn and every connection and lease given back,
while the gate stays shut. With the old drain it waits out the test's
20-second deadline.

## 7. Fixes after the fifth outside review

Two timeouts measured the wrong thing. Both now count from a fixed instant
kept on the `Wire`, instead of restarting with every wait.

**The handshake deadline restarted at each step.** A connection that has not
proved who it is holds one of only 8 places. Each step before the proof
(accepting the stream, reading the Hello, reading the session proof or each
pairing message) had its own 30 s timeout, so a peer that sent each step
just in time could hold a place for minutes. Filling all 8 that way would
keep out incoming sessions and pairing far longer than the documented 30 s.

```diagram
Before                                    After

accept stream  ├── 30 s ──┤                 ┏━━━━━━━━ 30 s in all ━━━━━━━━┓
read Hello          ├── 30 s ──┤            ┃ accept, Hello, proof,       ┃
read proof               ├── 30 s ──┤       ┃ pairing steps, closing      ┃
close                         ├── 30 s ──┤  ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
                                             then the person's answer and
slow peer: 2 min or more per place          the session use their own
                                             timeouts
```

`serve` and `Node::dial` take the instant when the connection starts plus
the handshake deadline and give it to the `Wire`. Until `end_handshake`,
`Wire::cap` limits every read, write, and closing drain to that instant.
Each side calls `end_handshake` once the peer is proved: after the session
proofs, and in pairing after the identity proofs, just before the person
compares the words. The person keeps the longer idle timeout to answer.

**The idle timeout restarted on any change in the node.** The send and
receive loops wait for a frame, a person's answer, or a state change at
once. Any state change, even another transfer finishing or the app taking
received items, ended that wait. The next wait then started a fresh
10-minute timeout. A busy node could keep an unanswered offer, and its
session place, open forever.

```diagram
Before                                    After

wait for a frame ── timeout: now + 10 min    wait for a frame ── timeout:
        │                                    ┃   last frame in or out + 10 min
  another transfer changes the state         ┃
        │                                    ┃ another transfer changes the
        ▼                                    ┃ state: the deadline stays put
wait again ── timeout: now + 10 min          ▼
   (never reached on a busy node)         ● session ends at its own idle time
```

`Wire` now records when the session last read or wrote one of its own
frames, and `Wire::idle_until` gives that time plus the idle timeout. The
send and receive loops wait until then, including the sender's wait for
buffer room. Other sessions' changes still wake them, so they still notice
cancellation and revocation at once. The wake just no longer moves the
deadline.

Three tests cover this:

- `a_peer_that_proves_nothing_gets_one_handshake_deadline_however_slowly_it_writes`.
  A raw client sends a Hello in two parts, each within 2 s of the step
  before, but finished after 2.8 s. With a 2 s deadline the node must not
  answer it.
- `an_unanswered_offer_times_out_even_while_the_receiver_is_busy_elsewhere`.
  The receiver's state changes every 100 ms while an offer waits. With a 2 s
  idle timeout, the offer must still end and give back its place.
- `people_who_answer_after_the_handshake_deadline_still_pair_and_receive`.
  Both people answer after a 1 s handshake deadline has passed, and pairing
  and the transfer must still succeed. Without this test, forgetting to end
  the handshake would cut every session at 30 s, and no other test would
  notice.

The slow-client test and the late-answer test sleep on purpose, because
letting a deadline pass is the behavior under test. The busy test ticks
every 100 ms instead.

## 8. Generated test files leave the repository

Two folders held files that tools write on their own:

```diff
 fuzz/
-├── corpus/frame/            229 inputs libFuzzer saved
-├── corpus/invitation_token/  70 inputs libFuzzer saved
 ├── fuzz_targets/
 └── Cargo.toml
-proptest-regressions/
-└── properties.txt           3 seeds proptest saved after failures
```

The fuzz corpus grew with every `mise run fuzz`, so each run left the tree
dirty. It also gave no head start: both targets decode small inputs, and an
empty corpus reached 153 inputs in 6 seconds. `/fuzz/corpus/` is now
ignored, and `mise run fuzz` lets cargo-fuzz create it.

Proptest replays its saved seeds before each run, but nobody reading the
tests could see what they checked. Each seed is now a named unit test:

- `one_column_never_holds_part_of_an_escape` covers two seeds. A no-break
  space, then a run of control characters, are projected into one column.
  No escape is ever cut in half.
- `truncated_trailing_and_unknown_messages_are_refused` gains the third. A
  pairing commit with one byte past its 32-byte field is refused as
  trailing.

`failure_persistence: None` stops proptest from writing new seed files. A
failure still prints the shrunk input, and that input should become a named
test the same way.

The plan's rule about time in tests now says what the tests actually do.
None sleeps to wait for state. State machines read time only from the
injected clock. Tests about network deadlines let real time pass on
purpose, with wide margins.

## 9. The external design review and what it changed

Two reviewers read the pairing and session design separately. Neither
found a way for an impostor to become trusted, and both gave all four core
choices "go". `docs/pairing-identity-session.md` lists every finding with
its outcome. Six of them changed code.

**Claims made at once could guess more often than allowed.** An invitation
allows 5 failed attempts, but an attempt was only used once a claim
failed. Eight claims opened together could each reach the inviter's
confirmation, and each of those is one online guess:

```diagram
Before                                  After

attempts left: 5                        attempts left: 5
claims in progress: 8 ━▶ 8 guesses      claims in progress: 5
                                        a 6th claim ━▶ ● AttemptsInUse
                                        a claim that proves the secret
                                        gives its place back
```

`Invitation::open_claim` now refuses a claim while the claims still in
progress already match the attempts left. This made no difference for
256-bit tokens, but short human codes will need it.

**A failed save used up the invitation anyway.** The inviter used the
invitation, then tried to save trust. If another pairing had taken the
alias meanwhile, the save failed and the invitation stayed used up.
`save_pairing` now checks that trust can be saved first. Saving itself can
then no longer fail, so a conflict changes nothing.

**Shares outside the prime-order group are refused.** `spake2` 0.4.0
accepts any curve point as the other side's share. A share with a
small-order part could leak a few bits of this side's secret scalar.
`finish_pake` checks the point with `curve25519-dalek`, the version spake2
already uses, so no new crate is built.

**The vectors are now enough for a second implementation.** Before, they
gave the random seed and the messages. Reproducing them needed spake2
0.4.0's own way of turning random bytes into a scalar. They now also list
the identities, the blinding points, the password and ephemeral scalars,
the SPAKE2 output `k`, the transcript `T`, and the word list. The test
does not just print these values; it proves them:

```text
x·B + w·M  == joiner's share          (scalars and password scalar)
y·B + w·N  == inviter's share
spake2(x, inviter's share) == k
keyed_hash(derive("confirm inviter", k‖T), T) == the inviter's MAC
```

**Text that fails its content check says so.** The receiver used to answer
`DigestMismatch` for text that matched its digest but was not UTF-8 or
differed from its preview. The sender then showed the wrong reason. The
new wire result `ContentMismatch` (code 6) carries the real one.

**The record names the real construction.** The wire uses SPAKE2 as
`spake2` 0.4.0 implements it, not RFC 9382. Section 1 now says so and
gives the crate's formulas.

The rest were accepted as they are, or as conditions for later work. The
conditions are written into the design record so the later work sees them:

- Short human codes need at least 23 bits, keeping a guesser's chance per
  invitation at or below 2^-20.
- Persistence must save the trust epoch counter together with the trust
  records.
- The local service must settle open claims when it stops.

One suggestion was turned down on purpose. A connection lost after
verification still closes the invitation. Each invitation allows one
comparison of the verifier words, and that is why 32 bits are enough.

## 10. Fixes after the code review of the core

A reviewer read the whole core at `27a5a5f` and reproduced four defects.
Three were in pairing and replay handling, which persistence will build on,
so they had to be fixed first. The design record lists them as R-1 to R-4.

**Every pairing exchange now has one owner.** The inviting side settled its
claim by hand on each error path, and one path was missing. If the joining
device vanished while the verifier words were on screen, writing the
inviter's decision failed, `?` returned early, and the claim stayed
verified. The invitation stayed open, so the same token could reach a
second comparison of words. One comparison per invitation is what makes a
32-bit verifier enough.

`PairingOwner` replaces the hand-written calls. It is created when the
claim opens and settles the claim when it is dropped, so an early return,
a lost connection, and an aborted task all reach the same code:

```diagram
Before                                   After

verifier shown                           verifier shown
      │                                        │
connection lost                          connection lost
      │                                        │
write decision fails                     write decision fails
      │                                        │
return Err ━▶ claim stays Verified       return Err ━▶ PairingOwner dropped
              invitation stays Open                    │
              ● second comparison                claim Verified ━▶ decline
                                                 invitation closed
```

What the owner does depends only on the claim's state when it is dropped:
in progress uses an attempt, verified closes the invitation, used needs
nothing. A service that shuts down mid-exchange now settles its claims the
same way, which closes an earlier finding (F-7).

**A yes holds what it promised.** Each side used to check that it could
save the trust, answer yes, and wait for the other side. Nothing stopped a
second pairing from taking the same alias during that wait:

```diagram
a invites b and c, both as "phone"

a ── yes to b ──────────────────────────── b's yes arrives ── save b ✗ AliasInUse
          c pairs fully, takes "phone"
b ─────────────── holds its answer ── yes ── save a ✓

result: b trusts a, a trusts c only
```

`TrustStore::reserve` now holds the alias and one trusted place at the
moment the person says yes. Other pairings see the reservation in
`check_alias` and `can_trust`, so c's pairing answers no and both of its
sides decline. When b's yes arrives, `trust_reserved` saves trust under the
held alias. The owner releases the reservation if the exchange ends without
trust.

The inviting side also uses the invitation at its yes, instead of after the
other side answers. Otherwise an invitation that expired while the other
person decided would fail the save in the same one-sided way. If the other
side then declines, the invitation stays used. That costs nothing, because
a verified claim closes the invitation anyway.

**Repeated transfer IDs have their own memory.** The receiver detected a
repeated offer by looking for its ID in the transfer map. That map is also
the status history, which drops the oldest finished transfer past 256.
Rejecting one offer, receiving 256 more, and sending the first again
brought a fresh consent question. `State::first_offer` now keeps the last
4,096 offered IDs in a queue of its own, 64 KiB at most. The policy is
written down: IDs older than that are forgotten. Only the trusted peer
itself can send an old offer again, over an authenticated session, and the
person is asked again as for any offer.

**Combining marks cannot grow a line.** `SafeLine` stops at a column limit,
but a combining accent has width zero. "a" followed by 100,000 accents
projected into one column produced 200,001 bytes. Each visible character
now keeps at most four zero-width characters, enough for stacked accents in
real writing. Further ones become escapes such as `\u{301}`, which take
columns, so the projection still ends at its limit with the shortened
marker.

Two unused items, `SessionId` and `Moment::saturating_add`, are gone.
The plan's table of principal types now names what does the session work:
`SessionBinding` and `SessionProof` authenticate a connection, and
`SessionSlot` holds its place and its internal `u64` key. That key becomes
`SessionId` once something outside the node has to point at one session,
such as resume or route changes. The old `Route` entry is gone from that row,
because the `Route` in the code is the inviter's address inside an invitation
token, not a session's path. Diagnostics now call the same hidden-format check as the projections; their
own copy missed soft hyphens, variation selectors, and tag characters.

## 11. The local service, command line, and safe files

**What got built.** Hyakume is now a command. `hyakume service run` starts
the one service that owns this user's installation. Every other command is
a client of it over a local socket: `init`, `status`, `unlock`, `lock`,
`pair`, `devices`, `remove`, `receive`, `send`, `cancel`, `inspect`,
`transfers`, `cleanup`, and `reset`. State survives restarts in SQLite, the
device key lives in the Secret Service or a passphrase-encrypted key file,
and received items are real files, published only after their length and
digest match. The decision record is
[the local service, its storage, and the file pipeline](local-service.md).

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
hyakume send ─┐                ┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
hyakume receive ─┼── Unix socket ━━▶┃ service                           ┃
hyakume pair ─┘  (this user only)  ┃   StateCell: Mutex<State + SQLite>┃
                                   ┃   Node: endpoint, sessions, sends ┃
                                   ┃   blocking file jobs (4 permits)  ┃
                                   ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
                                         │              │
                                   hyakume.db     spools, partial files,
                                   key file       published files
```

A client never touches state. A send belongs to the service once it
starts: if its client goes away, the transfer goes on, and only `cancel`
stops it. Questions for the person are different. They belong to the client
that is pairing or receiving, travel to it as events with a random prompt
number and a context, and only an answer on that same connection, with that
context, counts. A client that disconnects answers no to everything it
held.

The state lock that already ordered revocation against publication now also
orders durable writes. The SQLite connection lives inside `State`, so each
decision and its record share one critical section. A change that claims
success, such as trust granted, an offer accepted, or an item published, is
saved before memory changes. A change that ends work is applied first and
saved after; if that save fails, the service refuses new work until a
restart, and recovery settles what the database still shows as unfinished.

### Receiving into real files

The receive state machine used to collect bytes in memory and hand them to
a model publisher. Now it tells the runtime where each chunk goes and
decides only the outcome:

```diagram
Before                                  After

Chunk ─▶ Receiver                       Chunk ─▶ Receiver ─▶ Output::Write
         buf.extend(chunk)                                     │ runtime, outside the lock:
         (64 MiB staging budget)                               │ pwrite to .hyakume-<id>-<n>.part (0600)
ItemEnd ─▶ hash buf, publish to memory                         │ hash as written
                                        ItemEnd ─▶ Output::Verify
                                                     │ runtime: fsync partial
                                                     ▼
                                        on_verified(digest, commit step), under the lock:
                                          may_publish: cancelled? trust epoch same?
                                          ┃ renameat2(partial, name, NOREPLACE)
                                          ┃ fsync folder
                                          ┃ save: item published
                                          ▼
                                        ItemResult::Published
```

Memory no longer grows with the file. A node on the smallest buffer budget
takes a file four times that budget, and two such nodes send several times
their budgets to each other at once (`tests/loopback.rs`). The rename runs
under the state lock, so revocation either happens before it, and the item
is never published, or after it, and the published file stays.

`renameat2` with `RENAME_NOREPLACE` is the only thing that decides whether
a name is free. A file already there makes the item fail as `exists` while
the batch goes on, and the old file is not touched. The folder is checked
when someone starts receiving: its path may not pass through a symbolic
link, nobody else may write to it, its filesystem must be ext4, btrfs, XFS,
or tmpfs, and a probe in it must show that the rename really refuses an
existing name. A folder that fails is refused with the reason, and
Hyakume's own folder is offered instead.

### Crashes and restarts

Nothing new is written to record the intent to publish. The output name is
saved at acceptance, and the partial file's device and inode as soon as the
file exists. After a crash, recovery looks at the folder:

```diagram
crash between the rename and its record
  output name has the partial's inode? ━━▶ published (record saved now)
  partial still has its inode?         ──▶ paused, partial kept and charged
  neither                              ──▶ failed: lost
```

An identical file that was already there has another inode, so it never
counts as the received item. Debug builds have crash points
(`HYAKUME_CRASH_AT`) that stop the service between those steps, so
`tests/service.rs` checks both sides of the rename with real processes.

Work cut off by a stop, a lock, or a crash pauses and keeps its partial
file or sealed spool, and its quota, for 7 days. Resuming it comes with the
reliable-transfer work; until then it can be inspected or cancelled, and it
expires on its own.

### Sending files and standard input

```text
send notes.txt   ──▶ open no-follow, regular file? ─▶ hash (1 MiB buffer)
                     remember device, inode, size, times ─▶ offer
send -  / --text ──▶ spool (0600) ─▶ EOF ─▶ chmod 0400, fsync ─▶ ledger ─▶ offer
                     limits: size, all spools, free space, deadline, client gone
while sending    ──▶ hash again; source changed? ─▶ fail item, cancel the rest
```

Nothing is offered before its exact length and digest are known. Input that
stops early, grows past its limit, or stops arriving drops its spool and
makes no offer. Text and links use the same spool, so every item reads from
a file.

### Files Hyakume owns

Spools and partial files are rows in one ledger table. A row charges its
bytes from reservation until the file is published or deleted. When an item
ends, its row is marked for deletion in the same write. A cleanup pass
every 5 seconds deletes due files, retrying with longer waits for up to 24
hours. A file that still cannot be deleted is reported by `status` as kept,
with its bytes still charged, until `cleanup --retry` or the next start.

### Keys and locking

```diagram
Uninitialized ──init──▶ Unlocked ◀──unlock── Locked
                            ┗━━━━lock, stop━━━━▶┛
```

`init` asks where to keep the key. The Secret Service is offered when it is
reachable and unlocked; otherwise the key goes in a file encrypted with age
and a passphrase. scrypt runs at N = 2^17, which uses 128 MiB, because 2^18
would take the service's whole memory budget while it unlocks. A key in the
Secret Service unlocks at start without anyone typing. A key file waits for
`hyakume unlock`. Every Secret Service call gives up after 3 seconds, and
Hyakume never asks the keyring to unlock, so an SSH session is never left
waiting on a desktop prompt. Unlocking checks that the key belongs to this
installation and is the device recorded in the database.

Invitations stay in memory. A token names the endpoint key, which is new
every run, so after a restart no saved invitation could be reached anyway.
Saving its secret would only have added a vault key to protect it.

### Packaging

`mise run package` builds `dist/hyakume-VERSION-TARGET.tar.gz` with the
release binary, the license, the notices of all 374 crates compiled in, a
systemd user unit, install notes, and the build's commit, toolchain, and
`Cargo.lock` digest, plus a checksum. `mise run qualify` checks the
checksum and runs that exact binary through the whole journey in a fresh
container with no network beyond loopback: install, init, restart and
unlock, pairing, every payload kind, rejection, cancellation, an existing
file, a crash, revocation, lock, stop, and uninstall.

**Why this shape.** Each piece keeps a rule the core already had, now on
disk. The state lock still decides every order. The state machines still do
no file or network I/O, so their tests did not need a filesystem; their
state changes now reach SQLite through `State`, and the runtime does the
file work behind them. The end-to-end tests check that through the public
command.

## 12. Fixes after the review of the local service

A reviewer read the whole uncommitted service work and reproduced ten
defects. Each one was checked against the code before it was fixed; all ten
held. They share one pattern. Work outlived its owner, an identifier was
reused, or an error did not say whether its side effect had already
happened.

**A name finds a trusted device, never a revoked one.** A revoked device's
alias is free for a new device, so two records can share it. `find_device`
scanned every record and took the first, which could be the revoked one:
`remove laptop` then revoked the old record again, reported success, and
left the new laptop trusted. `TrustStore::find` now looks only at trusted
records, and refuses a name that is one device's alias and another's
fingerprint.

**Requests cannot keep a stopped node alive.** A running `receive`, an
invitation, or a send preparing its files held an `Arc<Node>` across waits.
`lock` stopped the node, but the node, with its key, stayed in memory until
those requests ended. Requests now reach the node only through a closure
that cannot wait, and a joining pairing became a task of the node:

```diagram
Before                                  After

request ──holds Arc<Node>──┐            request ──with_node(|n| ..)──▶ node
  awaits files, input,     │              (synchronous; nothing kept)
  pairing ...              ▼
                     ● node + key       pair --join ━━▶ node task
lock ──shutdown──▶   still in memory                    ┃ shutdown waits
                                        lock ━━shutdown━┛ node and key gone
```

If the node's work cannot stop within the drain deadline, `lock` and
`reset` now stop the whole service, as `service stop` already did, instead
of starting a new node beside work that still runs. The service's own
blocking jobs (preparing files, spooling, key files, folder checks,
deletions) hold permits the way the node's file jobs do, and shutdown takes
them all back before it reports a clean stop.

**Late cleanup results touch only their own row.** Ledger rows used
SQLite's reusable row IDs, and two cleanup passes could act on one
snapshot. A late result about a deleted file could remove a new file's row
and free its charged bytes. Row IDs now never repeat, passes run one at a
time, and a result changes a row only while it is still marked for
deletion.

**After the rename, a failed directory sync still means published.** It was
recorded as `failed: storage` while the file sat under its final name, and
recovery never looked at it again. The file is there with its bytes synced,
so the item now counts as published and the sync error is logged. The
decision record explains why another `fsync` would prove nothing. (The next
review showed that this acknowledged a file nobody knew to be durable;
section 14 replaces it.)

**Smaller fixes.**

- Text that is not UTF-8 is refused before its spool is sealed, so dropping
  the spool still deletes it. Before, a sealed and uncharged spool stayed
  until the next start.
- `FrameReader` keeps a frame read in part. The service and the command
  line race reads against other work, and a cancelled read used to drop
  the bytes already taken from the socket.
- `reset` refuses while the old key cannot be confirmed deleted, for any
  reason, and an unfinished initialization keeps its record until its key
  is gone. The installation record is what finds the key.
- `transfers` returns one summary per transfer. The full list of 256
  transfers with 256 items each could exceed the 1 MiB frame.
- Deadlines are checked against a lifetime clock that never runs behind its
  latest reading and follows the monotonic clock while the service runs.
  Setting the wall clock back no longer extends paused work.
- The terminal is put back on every way out of a passphrase prompt; the
  guard exists before echo goes off.

Most fixes have a test that was seen to fail without the fix. Three have
none: the terminal guard, whose failure needs file-descriptor exhaustion
inside a pseudo-terminal; the permits on the service's own jobs; and
stopping the service when `lock` cannot drain, which would need a job that
outlasts the 20 s deadline. The node-holding rule needs no test, because
the compiler enforces it: `with_node` hands out a `&Node` inside a
synchronous closure, so a request cannot keep it.

## 13. Packaging tasks in Rust

**What changed.** The Linux package was built and qualified by two shell
scripts and a Python script. They are now one Rust package, `xtask`, run as
`cargo xtask package` and `cargo xtask qualify`; the mise tasks call them.

```diff
 packaging/linux/
-├── build.sh          cargo build, tar, sha256sum
-├── notices.py        license notices from cargo metadata
-├── qualify.sh        checksum, then docker running sh -c
 ├── hyakume.service
 └── INSTALL.md
+xtask/src/
+├── main.rs           the two tasks and the processes they run
+├── package.rs        archive, checksum, BUILD-INFO
+├── notices.rs        normal dependencies of hyakume only, typed
+└── qualify.rs        checksum, unpack and check, then the journey in docker
```

**How it works.** `package` reads `cargo metadata` into typed structs,
builds the release binary, and writes the archive in Rust (`tar` and
`flate2`): folders first, every entry owned by root and stamped with the
commit's time. The same commit gives the same archive, byte for byte, which
the shell version could not promise. The notices are those `notices.py`
wrote, byte for byte. `qualify` checks the checksum, unpacks the package on
the host and checks its files and the binary's mode, then runs the
end-to-end test binary directly in a fresh container. No shell runs inside
the container any more.

The tasks still start cargo, rustc, git, and docker, because those are the
source of truth for what they report. The seven crates they add (`tar`,
`flate2`, and their dependencies) reach only `xtask`, never the shipped
binary, and `cargo deny` checks them like the rest.

**Why.** Windows comes next, and its package needs the same steps; shell and
Python would not run there, and Rust code can be shared. The notices decide
which licenses ship with the binary, so the dependency walk now has a test:
development and build dependencies, and packages Hyakume does not depend on,
stay out. A package whose checksum does not match is refused before
anything runs.

## 14. Fixes after the second review of the local service

The reviewer reread everything and confirmed seven of the earlier fixes. Of
the other three, two were only partly fixed and one was fixed the wrong
way. The reviewer also reproduced two new defects. All five held when
checked against the code.

**An unsynced rename is neither published nor failed.** Section 12 turned a
failed folder sync after the rename into success, so the sender heard
`published` about a name that might not survive a crash. The item now waits
for evidence:

```diagram
Before                                  After

rename ok, folder sync fails            rename ok, folder sync fails
        │                                       ┃
        ▼                                       ▼
item published, row removed             ● item paused, row kept (inode)
sender told: published                    rest of the batch cancelled
                                          sender told: storage-failed
                                                ┃ next start
                                                ▼
                                        name has the inode and the
                                        folder syncs ━━▶ published
                                        otherwise ━━━━━▶ stays paused
```

A later sync that succeeds is real evidence for a name, unlike for file
data: ext4, XFS, and btrfs journal names, and a failed commit leaves the
filesystem read-only.

**A restart measures the time it was stopped.** Each saved reading of the
lifetime clock now carries the wall clock's time. At the next start, the
time stopped is what the wall clock advanced since then. If the wall clock
reads earlier than the saved one, nobody can tell how long the service was
stopped, and recovery expires paused work. Before, a rollback across a
restart counted the stop as no time at all.

**A finished identity is never replaced by `init`.** `init` saved the
identity and then started the endpoint. When that failed, for example
because another program held the port, the service still said
`uninitialized`, and a second `init` deleted the saved key and made a new
identity. The identity now stays saved and locked, and `unlock` tries the
endpoint again; `init` undoes only an initialization that never finished.

**Spool accounting holds through every outcome.**

- A sealed spool whose ledger row cannot be written is deleted at once, and
  the failed write stops new work. If the spool cannot be deleted either,
  its bytes stay counted until the next start removes it.
- Admission read the ledger's total and the in-progress total under two
  locks, so a spool sealed between the two reads was counted in neither.
  Admission and the handoff now take the same two locks in the same order.

Each fix has a test that failed without it, except the lock order, whose
failure is a thread interleaving that no test could produce reliably.


## 15. Fixes after the third review of the local service

Two reviewers read the whole uncommitted service work and reported eight
defects between them. Each was checked against the code before anything
changed, and all eight held; the wildcard address was confirmed with a
probe. Most share one cause: work that has to finish was owned by
something that can be cancelled, such as a client's request.

**Lock-state changes belong to the service.** A request runs as a task of
its client's connection, and a disconnect aborts it. For `lock` and `reset`
that abort could land in the middle of the node's drain:

```diagram
Before                                   After

client: lock ──▶ request task            client: lock ──▶ request task
                   │ phase := locked                         │ waits for
                   │ node.shutdown() ━┓                      ▼
client leaves ──▶  ✗ aborted          ┃   service task ━━▶ phase := locked
                     drain dropped,   ┃     node.shutdown() runs to its end
                     pausing stays on ┃   client leaves ──▶ only the wait ends
unlock ──▶ new node beside old work ◀━┛   unlock waits for the transition
```

`init`, `unlock`, `lock`, and `reset` now run through `Service::own`, which
spawns them into a task set the service keeps and shutdown waits for. A
client that leaves stops waiting for the answer, never the change. Once the
service is stopping, no transition starts, so none can start a node after
the last one stopped. `Node::shutdown` was hardened as well: it keeps the
accept task's handle until it has seen the task end, and it reports success
only after every file job's permit is back. A second call after one that
was cut short waits for the same work instead of saying all is done.

**Input stays charged until its file is gone.** Input being spooled is
counted in memory until it is sealed. When its client left, the spool's
destructor tried to delete the file, ignored a failure, and the count went
anyway: the file stayed on disk, uncounted and unknown to cleanup.
`InputSpool` now holds the spool and its count together. Dropping it
deletes the file through `Spool::discard`, which reports failure; a file
that stays gets a ledger row marked for deletion, still charged, and the
regular cleanup retries it and reports it if it never goes.

**A sealed spool always reaches the ledger or the bin.** Sealing ran in a
blocking job awaited by the request. A client that left while it ran left a
sealed spool with no row. From the end of input on, sealing, the row, and
starting the send are now one service task (`Service::offer_input`), so the
request's end no longer decides whether they happen.

**Saved file names never reach outside their folder.** Cleanup deletes the
name a ledger row records. A row naming `../something` was accepted on load,
and `unlinkat` follows `..`, so a damaged database could delete a file
beside the spool folder. Rows now must carry exactly the name Hyakume gives
their kind (`.hyakume-<transfer>-<item>.part` or `<transfer>.spool`), saved
output names must be valid file names, and the deletion itself refuses
anything but a single name.

**One clock for every deadline.** Expiry was checked with the lifetime
clock, but transfers were stamped with the raw wall clock. After the wall
clock was set back, a new transfer's stamp sat behind the lifetime clock,
which cut its 7-day window short, or ended it at once after a rollback of
more than a week. Cleanup windows had the same mix. `State::now_ms` now
stamps and checks everything.

**Questions hold a place only while someone waits.** A receiving client
holds at most eight questions. A question whose offer was withdrawn stayed
in its list until answered, so eight withdrawn offers made every later
offer fail as if the person had declined. Questions the node gave up on now
free their place, an acceptance counts only once the node received it, and
a question nobody could take is refused as `busy`. A client that leaves
still answers no to what it held.

**A route names an address others can dial.** The unit file suggested
`--listen 0.0.0.0:7400` for LAN use, but the route then told other devices
to dial `0.0.0.0`, which is never this machine for them. `service run` and
`Node::bind` now refuse an unspecified address and ask for the LAN address
instead. Advertising every interface is discovery work.

**Smaller changes.** `Store::delete_trust`, `ipc::MAX_PATHS`, and two
one-line wrappers had no use and are gone. The session sequence in the
pairing record no longer describes the removed in-memory staging, and the
resource estimate counts 256 kept transfers, not 320. Long functions got
blank lines between their steps, so each one reads in parts.

Every fix has a test that failed without it, checked by undoing the fix
once: `a_shutdown_cut_short_leaves_its_work_to_the_next_one`,
`unfinished_input_that_cannot_be_deleted_stays_charged_until_cleanup_deletes_it`,
`a_file_row_whose_name_could_leave_its_folder_is_refused`,
`removal_never_reaches_outside_its_folder`,
`work_received_after_the_wall_clock_went_back_keeps_its_whole_window`,
`questions_the_node_stopped_waiting_for_hold_no_place`,
`a_question_dropped_without_an_answer_is_refused_as_busy_not_declined`, and
`an_address_other_devices_cannot_dial_is_refused`. Two changes have no test
of their own. Moving transitions and sealing into service tasks would need
a client to leave at an exact moment inside a blocking job; the node-level
test covers what that left broken. `a_spool_that_cannot_be_deleted_says_what_is_left`
covers the report the ledger row is built from.

## 16. Two more gaps from the fourth review

A last review reproduced two defects. Both held when checked against the
code.

**A queued send is settled even if it never runs.** `start_send` records
the transfer, then queues a task for the accept loop. The guard that ends
an unfinished transfer was created inside that task, so a task still in the
queue when shutdown began was dropped before it existed. The transfer
stayed `offered` with nothing behind it:

```diagram
Before                                    After

start_send ─▶ record transfer             start_send ─▶ record transfer
           ─▶ queue task                                 + guard ━━┓
                │ first poll                         ─▶ queue task  ┃ carried
                ▼                                                   ┃ inside
              guard created               shutdown: queue dropped ━━┛
shutdown: queue dropped                     guard ends it: interrupted
  transfer stays offered ✗
```

The guard now travels in `Outgoing` from the moment the transfer is
recorded. Whoever drops the send settles it: the task when it finishes, the
accept loop when it drains its queue at shutdown, or `start_send` when the
queue refuses it.

**State paths are resolved once.** The state folder was checked only at its
last component, while cleanup and receiving reopen folders without
following any link. A state folder under a linked ancestor, such as a home
folder that is a link, therefore worked for writing spools but not for
deleting them, and the default receive folder was refused. The service now
resolves its state and runtime folders to real paths at start, checks them
again there, and records only those.

`sends_still_queued_when_shutdown_begins_never_stay_offered`,
`folders_under_a_linked_ancestor_are_kept_by_their_real_path`, and
`a_state_folder_under_a_linked_ancestor_still_cleans_up_its_spools` each
failed before the fix.

## 17. Three gaps from the fifth review

All three held against the code. Each one is a step that must happen
before the next is safe, but nothing enforced the order.

**No publication without the record recovery needs.** When a partial file
is created, its inode goes to the ledger. That record is the only way
recovery can tell the received file from anything else under the output
name. If the write failed, `State::file_created` only logged it, and
receiving went on to publish. A crash after the rename then left a complete
file that recovery could only call paused, with its bytes still charged.
`file_created` now returns the failure. `open_partial` removes the new file
and fails the item as a storage failure before any byte is written to it.

```diagram
Before                                    After

create partial ─▶ save inode ✗ (logged)   create partial ─▶ save inode ✗
write, verify, rename ━━▶ published                          │
crash before the record                   remove the partial ┃
restart: inode unknown ─▶ paused ✗        item fails: storage, nothing published
```

**The read-only mode is synced with the bytes.** Sealing synced the spool,
then made it read-only. Syncing the directory afterwards does not cover the
file's own mode, so after a power loss a spool could come back writable,
and recovery treats a writable spool as lost. The mode now changes first,
and the one sync covers both.

**A client that stops reading gives its place back.** A client that lets
its event queue fill is disconnected, but the service then waited for its
writer to finish, and that writer was blocked on a socket nobody read. The
connection and one of the eight client places stayed taken for good.
Whatever is still queued now gets 5 seconds to go out; then the writer is
stopped and its end observed.

`a_partial_file_whose_record_cannot_be_saved_is_never_used` and
`a_client_that_stops_reading_gives_its_place_back` failed before their
fixes. The mode order has no test: showing it needs a power cut between
the sync and the next write of the inode.

## 18. Five gaps from the sixth review

All five held against the code. Four were confirmed by a test that failed
before its fix. The first was not: no test could show it, so the fix makes
it impossible by design.

**A reply goes out only after its request has ended.** A connection runs
one request at a time and answers `busy` to a second. The request task sent
its own reply and then returned, so for a moment the client could hold the
reply while the connection still counted the request as running. A client
that asked again at once, as the command line does after `status`, could
be told `busy`. The request now returns its reply, and the connection loop
sends it after taking the finished task off its slot.

```diagram
Before                                   After

request task: send reply ─▶ return       request task: return reply
client: reply ─▶ next request            loop: slot free ─▶ send reply
loop: slot still taken ━━▶ busy ✗        client: reply ─▶ next request ─▶ runs
```

Over a real socket the round trip almost always gave the task time to
finish: 30,000 requests in a row never hit it. So there is no test for this
one; the order of the two steps is the fix.

**A partial file needs its reservation.** Recording a new partial file's
inode updated the reserved ledger row and reported success even when no row
matched. A cancel or revoke marks the item's row for deletion in the same
write that ends the item, so a partial file created just after was written
into with no row naming it, and cleanup would never find it.
`Store::file_created` now says whether a reservation took the record, and
`open_partial` removes the file when none did, as it already did when the
write itself failed.

That exposed an ordering question in the receiver. The revoke test
revoked the sender between the first chunk arriving and its partial file
being opened, and the transfer then reported a storage failure instead of
the revoke. `Receiver::on_storage_failure` now lets a cancel or revoke that
already reached the state win, the rule `on_message` already followed.

**Locking ends the endpoint's invitations.** Invitations live in the shared
state, but their deadlines come from the node's monotonic clock and their
tokens name its endpoint. A lock stopped the node and left them there. The
node started at unlock has a new clock starting at zero, so the old
invitations looked open for another full lifetime and held the eight
invitation slots. `Node::shutdown` now clears them once every exchange has
ended.

**The lifetime clock keeps fractions of a millisecond.** Each reading
counted whole milliseconds since the last one and then dropped the
remainder. After the wall clock went back, only that count moved the clock,
so readings every half millisecond moved it not at all. The clock now
advances its starting point by the milliseconds it counted, and the rest
carries over.

**A leftover spool is charged by its real size.** A spool write that fails
partway can leave bytes the spool never counted. If the file then could not
be deleted, its ledger row charged only the counted bytes, and the
in-memory charge, which had counted them all, was released. The row now
takes the file's size on disk, or the most the file can hold when that
cannot be read.

`a_partial_file_whose_reservation_is_gone_is_never_used`,
`invitations_open_when_the_node_stops_leave_room_for_the_next_node`,
`readings_less_than_a_millisecond_apart_still_add_up`, and the extended
`a_spool_that_cannot_be_deleted_says_what_is_left` each failed before their
fix. `revoking_the_sender_mid_transfer_publishes_nothing_and_refuses_later_sessions`
failed after the reservation fix and before the receiver change.

## 19. Three gaps from the seventh review

Each one was an order of two steps that held only most of the time.

**An ended transfer always becomes evictable.** The service keeps 256
transfers and makes room by dropping the oldest finished one. A transfer's
end and its finish order are two separate writes, so a crash between them
left a row that was ended but had no finish order. Loading skipped it for
eviction, and recovery skipped it because it had already ended. Enough of
those and every new send failed with `TooManyTransfers`. `State::load` now
gives each such transfer the next finish order, which closes the gap at the
next start whatever caused it.

**`stop` gets its reply before shutdown ends its connection.** The request
signalled the service to stop and then returned its reply. Shutdown could
run first and abort every client, reply included, so the command line
reported a lost connection for a stop that worked: about one stop in 500.
Two changes fix it:

```diagram
Before                                   After

stop request: signal ─▶ return reply     stop request: return reply
shutdown: abort clients ━━▶ reply lost   connection: queue reply ─▶ signal
                                         shutdown: clients end ─▶ queues flushed
```

The connection that ran `stop` signals only after queueing the reply. And
shutdown no longer aborts clients outright: it sets `closing`, each client
ends its request and drains its queue through the usual 5-second writer
limit, and only a client still there after 6 seconds is aborted.

**The input deadline holds under a backlog.** `timeout_at` polls the
receive first, so a frame already queued was accepted even after the
deadline. A stream that kept the queue full, because spool writes were
slow, could outlast `--stdin-deadline` and be offered. `next_input` now
checks the deadline on every frame.

`transfers_that_ended_just_before_a_crash_still_make_room_after_it` and
`input_still_queued_when_its_deadline_passes_is_refused` fail every time
without their fix. `the_stop_command_is_always_told_the_service_stopped`
stops the service 200 times. Before the fix it failed in about a third of
its runs. After it, 1,200 stops in a row all got their reply.

## 20. Four gaps from the eighth review

Three of these are ownership handed over at the wrong moment. The fourth
replaces a library that could open a window.

**A receiver leaving cannot clear its replacement.** Ending a receive did
two things under two separate locks: clear the route, then clear the node's
destination. A new receiver could register and set its destination between
them, and the old one's second step then wiped it out. The new client said
it was receiving while every offer was refused as busy.

```diagram
Before                                      After

old: routes.receiver = None   (unlock)      old: lock routes
new: routes.receiver = new                       receiver = None
new: node.destination = new                      node.destination = None
old: node.destination = None  ━━▶ lost           unlock
                                            new: lock routes ─▶ register ─▶ set
```

Registering, leaving, and the node starting all change the destination
with the routes locked, and the routes lock is always taken before the
phase lock. No test covers it: a whole receive request would have to fit
between two lock acquisitions in one `Drop`, so the lock itself is the fix.

**A paused transfer keeps its place.** Each unfinished transfer holds a
`TransferSlot` from the node's budget, which caps how many can exist. When
a rename succeeds but the folder sync fails, the transfer pauses until
recovery can tell whether the file is on disk. The slot lived in the
receiver's `Active` state and was dropped with it, so the paused transfer
counted for nothing, and new offers could be admitted past the limit until
the next unlock. `unconfirmed` now hands the slot to the state's
`paused_slots`, the same place `hold_paused` puts slots for work paused
before a node started. The state lets it go when the transfer ends.

```diagram
session ── Active { slot } ──▶ unconfirmed ──▶ State.paused_slots[id]
                                                  │
                                    expire/cancel ▼ settle() drops the slot
```

**Making room survives a failed delete.** To keep 256 transfers, inserting
one drops the oldest finished one. The code popped it from the finish queue
before deleting its row, so when the delete failed, the transfer stayed in
memory and in SQLite but was gone from the queue: never evictable again in
that run. It now reads the head of the queue, deletes, and pops only on
success.

**The Secret Service never gets to show a prompt.** A keyring can be set to
ask someone to confirm creating or deleting an item. The Secret Service
then answers with a prompt object, and nothing shows until the client calls
`Prompt`. The `secret-service` crate made that call inside `create_item`
and `delete` and waited, so on a desktop session a window opened that
nobody on SSH could answer. The timeout ended the wait but not the window.
The crate keeps its D-Bus proxies and session key private, so there was no
way to stop it.

`src/secrets.rs` now talks to the Secret Service through `zbus` directly. A
`Keyring` opens one session and makes each call itself. A reply that names
a prompt is dismissed unseen and fails with `SecretError::Confirmation`,
which tells the person to allow the action without confirmation or use the
key file.

```diagram
CreateItem / Item.Delete
        │
        ▼
 prompt == "/" ? ── yes ──▶ done
        │ no
        ▼
 Prompt.Dismiss  (never Prompt.Prompt)
        │
        ▼
 Err(Confirmation)
```

The session keeps the encryption the crate used. `Exchange` runs
Diffie-Hellman in RFC 2409's 1024-bit group with `crypto-bigint`'s
constant-time arithmetic, refuses public values of 0, 1, and p − 1, and
derives the AES-128 key with HKDF-SHA256. Secrets go out and come back as
AES-CBC with a fresh IV. These crates were already in the tree; only `cbc`
is new, and `secret-service` with its own copies of them is gone.

`a_secret_service_that_wants_confirmation_never_gets_to_ask` runs a
stand-in Secret Service on a private D-Bus connection that wants
confirmation for everything, and counts the prompts it is asked to show. It
must stay at zero. Running the prompt, as the crate did, fails it. The real
check that the session interoperates is
`a_secret_service_key_unlocks_at_start_and_is_deleted_on_reset` under
`mise run test-keyring`. Against gnome-keyring, it stores the key at init,
loads it at start (the key must match the recorded device), and deletes it
at reset.

`a_rename_whose_folder_cannot_be_synced_is_never_acknowledged` now checks
that the paused transfer still counts after its session ends, and stops
counting when it expires. `transfers_that_failed_to_make_room_still_make_room_later`
makes every finished transfer fail its delete once, then inserts one more.
Both fail without their fix.

## 21. Four fixes from the ninth and tenth reviews

**The device's own name is checked when it loads.** `init` accepts a name
only through `LocalAlias::new`: 1 to 64 bytes, no control or formatting
characters, no surrounding spaces. Loading checked only that it was UTF-8
and at most 64 bytes, so a name changed in the database could carry an
escape sequence and a newline, and `hyakume status` printed them as they
were. That needs write access to the user's own database, so it is not a
peer attack, but the rule is now applied on both sides.
`Store::installation` runs the same check, so a changed name is a damaged
row, refused like any other. The status view also escapes the name with
`SafeLine`. A valid name comes through unchanged, and the display no longer
depends on the check that came before it.

```diagram
init ──LocalAlias::new──▶ installation.name ──load: LocalAlias::new──▶ Installation
                                                                        │
                                              status: SafeLine (no quotes)
```

**One folder can hold both state and socket.** The service takes a lock
in the state directory, to own the database, and one in the runtime
directory, to own the socket. `flock` locks belong to an open file, not to
a process. With `--state-dir` and `--runtime-dir` naming the same folder,
the service opened `service.lock` twice, and the second lock was refused
by the first, which this same process held. The service reported "another
Hyakume service already uses" the folder and never started.
`paths::lock_service` opens both lock files and compares their device and
inode. A match means one file, so the lock it already holds covers both.
Comparing paths would not do: two paths, such as through a bind mount, can
name one folder.

```diagram
lock_service
  ├─ open + flock  state/service.lock
  ├─ open          runtime/service.lock
  └─ same (dev, ino)?  yes ─▶ keep the one lock
                       no  ─▶ flock the second too
```

**The keyring test cannot hang.** Its shell script waited for each
service start with an unbounded `until` loop, and the test collected the
script's output with no deadline. A service that failed to start turned a
test failure into a stall. The script now waits at most 10 seconds for
each start and stops any service still running when it exits, keeping its
own exit status. The test runs the session in its own process group, and
`wait_group` kills the whole group after 60 seconds. A simulated startup
failure now ends the test in about 10 seconds with "the service never
answered". A simulated hang ends at the deadline and leaves no process
behind.

One trap in that script was easy to miss. `/bin/sh` is bash here, and
`set -e` still applies inside an exit trap. `kill` on a service that has
already stopped fails, the trap exits 1, and the script fails although
every step passed. The trap ends its `kill` with `|| true`.

**rustls 0.23.46.** A new advisory, RUSTSEC-2026-0346, covers rustls
0.23.45, which reaches Hyakume through Iroh, and it failed the dependency
check. It concerns `Debug` output of TLS session values in custom session
stores, which Hyakume does not have. Both lockfiles move to 0.23.46, and
`cargo deny check` passes again.

`damaged_rows_are_refused_instead_of_repaired` now saves a device name,
rewrites it in the database with an escape sequence and a newline, and
expects loading to refuse it. `one_folder_for_state_and_socket_is_locked_once`
gives the same folder by two paths and expects one lock that still keeps a
second service out. Both fail without their fix.

## 22. Three fixes from the eleventh review

The review found one theme behind all three defects. Each component
passed its own tests, but two components disagreed on the rule between
them.

**An offer that validates can be sent and loaded again.** `Offer::validate`
checked each name, the item count, and the sizes. The 64 KiB limit on the
whole offer lived only in the wire encoder. A batch of 256 files with
255-byte names passes every per-item check and still encodes to 76,568
bytes. The sender recorded that offer, the encoder refused it, and at the
next start loading ran the offer through the decoder, which refused it as
well. The service stopped with "the state database is damaged: saved offer"
before its socket opened, so `reset` could not reach it either.

```diagram
before                                   after
validate ─ok─▶ record ─▶ encode ✗        validate ─✗ MetadataTooLarge (nothing recorded)
                 │
restart ─▶ decode saved offer ✗
```

`protocol::offer_payload_len` measures the payload with the same writer the
encoder uses, so the size rule exists in one place. `validate` checks it
after the per-item checks, so one bad name still gets its own error. The
receiver never sees the new error, because its decoder refuses oversized
offers first, but it maps it to `TooLarge` anyway. The encoder keeps its
own check: it is the last line before bytes leave.

**A spool found at start goes through the ledger.** Recovery deleted
spools that no row recorded and ignored the result. When the deletion
failed, the file stayed on disk with no row, no charge, and no retry, and
only another restart could find it. Recovery now deletes nothing itself.
It records each such spool as a file to delete, with its inode and size,
and the cleanup pass that runs right after start deletes it, retrying on
the usual schedule. Its bytes stay charged until it is gone.

```diagram
recover ─▶ unknown <transfer>.spool ─▶ ledger row (delete, charged)
                                            │
maintain (first tick at start) ─▶ delete ─ok─▶ row removed, bytes freed
                                     └─✗─▶ retry in 10 s, 40 s, … up to 24 h
```

This puts orphans on the same path as every other owned file. Recovery
already only marked unneeded ledger files for deletion and left the
deleting to maintenance. Only a name Hyakume gives a spool, on a regular
file of this user, gets a row. Anything else in the folder is left alone
and counted in `Recovered::strays`, because the ledger cannot name it and
Hyakume did not create it. A folder that cannot be read in full sets
`Recovered::spool_unread` and logs the error instead of returning zero.

**A permanent invitation allows one comparison.** The 32-bit verifier is
enough because each invitation gets one comparison of words.
`claim_verified` let every claim that proved the secret reach that step,
so two devices joining with one invitation each showed the inviting person
a set of words. Now the first permanent claim to verify takes the
comparison, and a later one is refused in the same state change, before
any prompt. Its device gets `InvitationUnavailable`. Temporary invitations
keep their rule: every claim may verify, and the sender approves one.

```diagram
claim A verifies ─▶ Verified(words) ─▶ prompt ─▶ confirm_permanent
claim B verifies ─▶ A is Verified? yes ─▶ Refused(OtherClaimComparing), no prompt
```

The refused device still asks its own person, because a joining device
asks as soon as its exchange ends. Its answer cannot change anything.
Telling the joiner first would need a new message, so that waits for the
pairing work.

The old permanent-invitation test verified two claims before using one, so
it assumed the defect. It now leaves the second claim exchanging. A new
unit test checks the refusal itself, and a loopback test drives two real
joins with one invitation and fails at once if the inviting side is asked
twice.

`every_offer_that_validates_can_be_sent` grows a batch of longest names
until validation refuses it, then checks that the refusal is the metadata
limit and that the last accepted batch encodes as a frame.
`a_batch_too_large_for_one_offer_is_refused_and_the_service_still_restarts`
sends the review's batch through the CLI and restarts the service.
`an_orphan_spool_that_cannot_be_deleted_stays_charged_until_a_retry_deletes_it`
and its service-level twin plant a spool in a folder nothing can be
deleted from and follow it through start, a failed deletion, the charge,
and the retry. Each test fails without its fix. The restart test, with the
check removed, stops on the same "saved offer" error the review reported.

**Two races CI found after these fixes.** Neither came from them.
`terminal::secret` printed its prompt and only then turned echo off.
Anything typed or pasted the moment "Passphrase again:" appeared could be
echoed before the terminal changed mode, and the CI runner caught the
passphrase on screen that way. Echo now goes off first, and the prompt
follows. The `Restore` guard is already in place, so a failed write still
turns echo back on. The place test for a stalled client unwrapped each
retry's hello, but a refused client gets a busy reply and a socket closed
without reading, so the write can fail with a broken pipe. That failure
now counts as one more refusal.

## 23. Two fixes from the twelfth review

Like the last review, this one found defects between components that each
looked right alone. The item state machine, the file ledger, cleanup, and
startup recovery each followed their own rules, and the gaps sat between
them.

**An unconfirmed rename is its own state.** When the rename into the
destination succeeds but the folder sync after it fails, nobody knows yet
whether the new name survives a crash. The receiver never tells the sender
`published` in that case. It used to pause the item and leave the decision
to startup recovery, which counts the file as published only if a new sync
succeeds.

The trouble was that a paused item is ordinary resumable work, and
everything that ends paused work ended this one too:

```diagram
before
rename ok ─▶ sync ✗ ─▶ Paused ─▶ cancel / revoke / expire ─▶ Cancelled or Expired (terminal)
                                                                │
                                         ledger row ─▶ delete ─▶ cleanup removes the row
                                                                │
                                         restart ─▶ recovery skips finished transfers
                                                                │
                              the file sits under its final name; the transfer says cancelled
```

The review reproduced this for cancel, revocation, and expiry with a real
file, a real rename, and a restarted database. A run with none of the three
recovered the same file as published.

`ItemState::Unconfirmed` now holds that case. It is not terminal and not
active. `cancel`, `revoke`, `fail_rest`, `pause_all`, and `expire_all` skip
it, because they only touch items that are `stoppable`. The transition table
refuses every event from it. The only way out is
`Transfer::found_on_disk`, which records what a check of the disk showed:
published, still unconfirmed, paused again (the partial file is back, so
the rename did not survive), or failed as lost.

```diagram
after
rename ok ─▶ sync ✗ ─▶ Unconfirmed ─┬─ cancel / revoke / expire: no change, row stays live
                                    │
          maintenance (every 5 s) or startup recovery
                                    │
              recheck: same inode under the output name?
                     ├─ yes, folder syncs now ─▶ Published (row removed with the record)
                     ├─ yes, sync still fails ─▶ Unconfirmed
                     ├─ no, partial is back ───▶ Paused
                     └─ neither ──────────────▶ Failed: lost (row to delete)
```

Startup recovery and the running service share the check.
`recovery::partial_evidence` now returns `Evidence::Unconfirmed` where it
used to return `Kept`, and `recovery::record` turns evidence into a state
change for both. The maintenance pass calls `recovery::unsettled` under the
lock, runs `recheck` as a blocking job, and calls `settle` under the lock
again, which ignores an item that changed in between. Before, a long-running
service waited for a restart to settle the item, and expiry usually came
first.

Why not acknowledge the item once the rename succeeds? Because a rename
that is not yet durable can vanish in a crash, and the sender would then
believe in a file that does not exist. Why does revocation not cancel it?
The rename happened under the state lock after `may_publish` checked
trust, so it came before the revocation in the one order both share,
exactly like an item that published cleanly.

A few smaller rules follow from the new state. `is_paused` counts a
transfer with only unconfirmed items left as paused, so it keeps its place
among unfinished transfers. `State::expire` picks only transfers that still
have a paused item, so an unconfirmed transfer is not reported as expiring
on every pass. Status shows the outcome `unconfirmed`. `cancel` stops
waiting as soon as nothing with a connection is left, instead of sitting
out its 5 s for an end that only the disk can bring. The old transition
from verifying to paused existed only for this case, so it is gone.

**A finished transfer keeps no content.** The `transfers` table stored the
encoded offer, and a text or link offer carries a preview of up to 4 KiB,
which for short text is the whole payload. Declining, finishing, or
cancelling changed the states and left the offer as it was. History drops a
transfer only to make room for a new one, so a quiet installation kept the
text forever. The review sent a short text through the release binary,
declined it, waited for the spool to go, and found the whole text in a live
row on both devices. That contradicts the rule in the plan that nothing of
a payload is kept after a terminal outcome.

```diagram
offer received or made ─▶ saved: offer with preview, digest      (payload is on disk anyway)
            │
transfer finishes ─▶ settle ─▶ offer.forget_preview()
                            └▶ finish_transfer: finished = n, offer = without preview (one write)
                                                 secure_delete zeroes the old bytes
```

The preview stays while the transfer is unfinished, because a resume would
check the received text against it, and the spool or partial file holds
the payload for that long anyway. The rule is that the preview lives
exactly as long as those files.

The digest needed care. The acceptance names the digest of the whole
offer, preview included, so recomputing it after the preview is gone would
quietly give a different value under the same name. `Offer` now computes
its digest once at validation and keeps it, and `forget_preview` leaves it
alone. Storage saves it in a new `offer_digest` column. On load, an
unfinished transfer's offer must hash to its saved digest. A finished one
has no preview to hash, so it takes the saved digest as it is
(`Offer::with_saved_digest`). Nothing reads the digest of a finished
transfer today; the point is that nothing can read a wrong one later.

Rewriting a row is not enough on its own. Without `secure_delete`, SQLite
leaves the old bytes in a free page of the file, and the storage test found
the declined text there. The pragma is now on for every connection.

Schema 2 is the first migration. A schema 1 database gets the column, every
offer's digest, and the preview of every finished transfer removed, in one
transaction. Then `VACUUM` runs once, because schema 1 wrote without
`secure_delete`, and transfers dropped from history long ago may still sit
in free pages. A new database runs the schema 1 script and the same step,
so there is one path to the current schema.

**Tests.** The review asked for coverage past the point where the two
components disagree, and each new test fails with its fix taken out:

- `receive::tests::a_rename_whose_folder_cannot_be_synced_is_never_acknowledged`
  used to end by expiring the paused item, which was the defect. It now
  cancels, revokes, and expires, and checks that the item, its ledger row,
  and its place among unfinished transfers survive all three.
- `recovery::tests::an_unconfirmed_rename_survives_cancel_revocation_expiry_and_cleanup_until_recovery_publishes_it`
  repeats the review's probe in the tree: a real folder, a real rename, a
  database on disk, then each of the three actions, cleanup, a restart, and
  recovery. The file must end up published with nothing charged.
- `recovery::tests::maintenance_settles_an_unconfirmed_rename_from_the_disk`
  settles the item without a restart, and as lost when the file was removed
  first.
- The transfer property test gained random unconfirmed items and
  whole-transfer stops, and asserts that an unconfirmed item stays
  unconfirmed. Requiring a verifying item first made the state too rare for
  256 cases to find the break, so the property marks any unfinished item,
  as recovery can.
- `storage::tests::a_finished_transfer_keeps_no_content_in_the_database`
  reads the database file itself: a declined text is gone, a paused one is
  still there, and both digests survive a restart.
- `storage::tests::a_schema_1_database_drops_the_previews_of_finished_transfers`
  builds a schema 1 file with a finished row and a deleted one and checks
  that neither text is left after the upgrade.
- `declined_text_leaves_no_copy_in_either_database` declines a text through
  the CLI and searches both devices' database files for it.

Two older tests claimed more than they checked. The redaction test proves
that a value wrapped in `Redacted` stays out of the log, not that every call
site wraps its secrets, and the abort test proves that `tasks::observe`
aborts on a panicked task, not that every task owner calls it. They are
now `values_wrapped_in_redacted_never_reach_the_output` and
`observing_a_panicked_task_stops_the_process`, with module notes that say
what they leave out.

## 24. Two gaps in those fixes

A second pass over entry 23 found one gap in each fix. Both came from
treating two different situations as one.

**An unreadable folder is not a missing file.** The maintenance recheck
reused `partial_evidence`, which turned every error into `Lost`: a folder
it could not open, or a name it could not stat. The review made the
destination unreadable after an unsynced rename. The file was still there,
but the recheck marked it lost, cleanup removed its ledger row, and the
restart could no longer find it.

```diagram
before                                   after
reopen or stat fails ─▶ Lost             missing, replaced, linked ─▶ Lost
                                         EACCES, EIO, ... ─────────▶ Unreadable: no change
```

`Evidence::Unreadable` now covers any error that says nothing about the
file. `record` leaves the item as it is, so an unconfirmed item waits for
the next pass and an ordinary item is paused at startup like kept work. A
missing folder, one replaced by another directory, or a path that now runs
through a link still counts as lost, since the recorded folder is no longer
there to hold the file. `Recovered::unreadable` counts these items. If the
folder stays unreadable, the item stays unconfirmed and keeps its slot,
which is the cost of never discarding a file that may exist.

**The purge runs before the version changes.** The migration committed
schema 2 and only then ran `VACUUM`. A stop between the two left a schema 2
database that would never vacuum again, with old offers still in free
pages; the review showed this by killing the process right after the
commit.

```diagram
before: tx { schema 2 rows, version 2 } ─▶ VACUUM      (stop between: purge skipped forever)
after:  VACUUM ─▶ tx { schema 2 rows, version 2 }      (stop before commit: all redone)
```

`VACUUM` cannot run inside a transaction, so it now runs first, while the
file still says schema 1. The transaction that follows rewrites rows with
`secure_delete` on, so it leaves nothing in free pages itself. A durable
"purge pending" marker would also work, but it needs a new column and a
second code path, and the order alone closes the gap.

**Tests.** `an_unreadable_folder_leaves_an_unconfirmed_rename_for_a_later_check`
removes every permission from the destination after the rename. It runs a
maintenance check and a restart while the folder is unreadable, then
restores access and expects the file published with nothing charged. With
the old mapping it fails with `Failed(Lost)`, as the review saw.
`a_migration_stopped_once_recorded_leaves_no_old_text` reruns the test
binary as a child that aborts at the `migrated` crash point, right after
the commit, and then searches the file for the old texts. With the old
order, the dropped transfer's text is still there.

## 25. Unreadable at startup means unconfirmed

Entry 24 left items alone when their folder could not be read. That
protected items already saved as unconfirmed, but not the crash it was
meant for. A crash right after the rename, before the record, leaves the
item saved as queued. Recovery found the folder unreadable, left the item,
and then paused it with the rest of the transfer. Maintenance checks only
unconfirmed items, so it never looked again, and a later cancel deleted the
ledger row. The review drove exactly this through the debug CLI.

```diagram
before
crash after rename ─▶ saved Queued ─▶ restart, folder unreadable ─▶ Paused
                                                    cancel ─▶ Cancelled, row deleted, file orphaned
after
crash after rename ─▶ saved Queued ─▶ restart, folder unreadable ─▶ Unconfirmed
          cancel ─▶ skipped, request kept ─▶ access back ─▶ recheck ─┬─ final name ─▶ Published
                                                                     └─ partial    ─▶ Paused ─▶ Cancelled
```

`record` now maps `Evidence::Unreadable` to `Unconfirmed`. Only a
receiving item whose partial file was recorded can get that evidence, and
any such item may have been renamed, so it waits for the disk like an
unsynced rename. `ItemState::Unconfirmed` now covers both reasons.

More items can now sit unconfirmed while someone cancels or revokes, so
those requests must not just vanish. `State::request_cancel` keeps the
request in `cancels` when an unconfirmed item survives the cancel. When a
maintenance check finds the file back under its partial name, the rename
did not happen, and `recovery::settle` applies the revocation or the kept
cancel to the now paused item. Expiry needs nothing extra, since
`State::expire` picks up paused items on its next pass. A kept cancel lives
in memory only, so a restart in between forgets it, and the item then
pauses and expires like any paused work.

**Tests.** `a_crash_after_the_rename_is_recovered_as_published` now
restarts with the folder unreadable, cancels, restores access, waits for
maintenance to publish the file, and restarts again. With entry 24's
handling it ends `paused`, as in the review.
`a_cancel_or_revocation_waits_for_the_disk_and_applies_if_the_rename_was_undone`
moves the file back to its partial name after an unsynced rename, cancels
or revokes, and expects the next check to end the item that way and delete
the partial file.

## 26. Startup expiry waits for the disk too

Entry 25 carried a cancel and a revocation past an unreadable folder, but
not the third thing startup decides: expiry. Recovery expires paused work
when the resume window is over or when it cannot measure how long the
service was stopped, because the wall clock reads earlier than its last
saved reading. An unconfirmed item skips that expiry. Once access came
back and the check found only the partial file, the item became paused.
`State::expire` checks the deadline, which had not passed, and knows
nothing of the unmeasured downtime, so the item stayed paused with its
bytes on disk. The review reproduced this through the CLI with a clock
rollback.

```diagram
restart, downtime unknown ─▶ expire_all (skips Unconfirmed)
                          └▶ close_window: expires_ms = now, saved
access back ─▶ recheck ─▶ partial only ─▶ Paused ─▶ settle: window closed ─▶ Expired
```

Recovery now saves its decision as the deadline: `State::close_window`
sets `expires_ms` to the restart time, in memory and in the store. It runs
before the check that skips transfers with only unconfirmed items, so those
get it too. `recovery::settle` applies an expiry along with a revocation or
a kept cancel, and `State::expire` would catch the same item on its next
pass. Only transfers recovery handled at that start get a closed window,
so work that starts afterwards keeps its full 7 days. The deadline is saved,
so another restart before access returns still finds the window closed.
A kept cancel, by contrast, is still lost on restart.

**Tests.** The deferred-request test is now
`what_happened_while_the_disk_could_not_tell_applies_once_the_rename_proves_undone`,
with a case per request: cancel, revocation, and a restart with the folder
unreadable while the clock reads a day behind its saved value. Each moves
the file back to its partial name and runs two maintenance passes. Without
the saved deadline, the third case ends paused.

## 27. Taking the carried-over requests back out

Entries 25 and 26 made a cancel, a revocation, or a startup expiry that
skipped an unconfirmed item apply later, if the disk showed the rename had
never happened. That took a kept cancel in `cancels`, a saved deadline
(`State::close_window` and `Store::save_expiry`), three extra branches in
`recovery::settle`, and a three-case test. Looking at the whole change
again, it was more machinery than the case is worth, so it is gone.

The case needs a crash before the rename, a destination that cannot be
read at that very restart, and a cancel, a revocation, or a clock set back
while it stays unreadable. Without the extra code, what happens is this:

```diagram
unreadable at startup ─▶ Unconfirmed ─▶ cancel / revoke / expiry: skipped
access back ─▶ recheck ─▶ partial only ─▶ Paused ─▶ expires when its window ends ─▶ partial deleted
```

No record is lost and nothing is reported as published, so the defects of
entries 23 to 25 stay fixed. The cost is an unfinished partial file that
waits up to the 7-day resume window instead of going at once, and stays
charged until then. A revoked peer cannot use the paused item, because a
resume checks trust first. The rule is simpler to hold in one's head: an
item the disk cannot vouch for waits for the disk, and paused work expires
on its schedule.

What stays from entry 25 is the part that protects records:
`Evidence::Unreadable` maps to `Unconfirmed`, and
`a_crash_after_the_rename_is_recovered_as_published` still drives the
crash, the unreadable restart, the cancel, and the publication.

## 28. Unconfirmed items keep their stop, and blocking jobs supervise themselves

**Entry 27 broke a promise of the plan.** It took out the code that carried
a cancel, a revocation, or a startup expiry past an unconfirmed item, on
the argument that the worst case was a cleanup delay. The next review
pointed at `PROJECT.md`: revocation "cancels active and paused transfers",
and every terminal outcome schedules cleanup. Once the disk showed only the
partial file, the item became paused, nothing was scheduled for deletion,
and its bytes stayed charged. The argument had not checked the plan.

The fix is smaller than what entry 27 removed, and it survives a restart.
The kept stop lives in the item state itself:

```diagram
Unconfirmed(None) ── cancel / revoke / expire ──▶ Unconfirmed(Some(stop))    first stop wins
Unconfirmed(stop) ── disk: final file ──────────▶ Published                  stop dropped
Unconfirmed(Some) ── disk: partial only ────────▶ the stop: Cancelled or Expired, partial deleted
Unconfirmed(None) ── disk: partial only ────────▶ Paused
```

`ItemState::Unconfirmed` carries an `Option<Stop>`, where `Stop` is a cancel
with its cause or an expiry. `Transfer::cancel`, `revoke`, and `expire_all`
share `stop_all`, which ends stoppable items and lets an unconfirmed one
keep the first stop it meets, the way a terminal state never changes.
`found_on_disk` turns "paused" into the kept stop, and keeps the stop when
the item stays unconfirmed. The state saves as its name, for example
`unconfirmed:cancelled:revoked`, so no new column or in-memory set is
needed, and status shows the pending stop. Startup recovery now runs its
revocation and expiry checks for transfers whose only unfinished items are
unconfirmed, so those keep the decision too; it still pauses only items
that can pause.

Runtime expiry needs nothing new. `State::expire` checks paused items
against their deadline, and an item settled as paused after its window
ended expires on the next pass.

**A blocking job's panic no longer depends on its waiter.** Both blocking
helpers awaited the job's handle and passed the result to
`tasks::observe`. When the request awaiting a job was cancelled, as when
its client disconnects, the handle went with it. The job kept running and
kept its permit, but a later panic in it was caught by Tokio and seen by
nobody, and the process went on with possibly half-changed state. The bug
was there before these reviews.

`tasks::spawn_blocking` runs the job under `catch_unwind` and calls `fatal`
on a panic, on the job's own thread. Both helpers use it. The review
suggested moving the handles into an owner set instead; that would also
work, but every owner would then have to drain it, and checking at the
source covers every waiter at once. The command line's terminal reads keep
plain `spawn_blocking`: that process holds no shared state, and a panic
there already becomes an error.

**Tests.** `a_kept_stop_ends_the_item_once_the_disk_shows_no_rename` covers
the case the review found, the disk showing only the partial file. Three
stops (cancel, revocation, and a restart with the folder unreadable and the
clock a day behind) are each settled two ways, by a maintenance pass and by
a restart. Each must end the item as that stop and delete the partial file.
Without the kept stop, the first case ends paused, as in the review. The
crash journey covers the other case: it now expects
`unconfirmed:cancelled:receiver` after its cancel and still ends published,
because the final file is there. The transfer property gained a check that
a kept stop never changes. In `tests/fatal.rs`,
`a_blocking_job_that_panics_after_its_waiter_left_stops_the_process`
starts a job, aborts the task awaiting it, then lets the job panic. With
plain `spawn_blocking`, the child survives and prints its marker.

## 29. The unused redaction wrapper is gone

`diagnostics::Redacted<T>` printed any value as `[redacted]`, and the module
note said secrets and payloads go through it. Nothing did: no production
code used it, and its only test proved that a type printing a constant
prints that constant. The secrets that exist are protected another way.
`DeviceKey`, `KeyBundle`, and `InvitationSecret` print as `[redacted]` from
their own `Debug`, which their own tests check, so logging one by mistake
shows nothing.

The wrapper and its test are removed, and the module note now says where
redaction really happens. A wrapper that nothing calls only suggests a
protection that is not there. If a payload ever needs logging, it should
get a type of its own that cannot print its contents, as the keys have.

## 30. Parsing a kept stop without recursion

`ItemState::from_name` read `unconfirmed:<rest>` by parsing `<rest>` as a
whole item state and then refusing anything but a cancel or an expiry. A
damaged `items.state` value with many `unconfirmed:` prefixes recursed once
per prefix before reaching that refusal, so a long enough value overflowed
the stack and aborted startup instead of failing as a damaged database.
Only local, already damaged state could trigger it.

The parser now matches the two allowed suffixes directly, `cancelled:<cause>`
or `expired`, and refuses everything else, nesting included.
`unconfirmed_names_round_trip_and_nesting_is_refused` checks that every
kept stop survives its own name and that bad suffixes, including a million
nested prefixes, give `None`. With the recursive parser, that test
overflows its stack.
