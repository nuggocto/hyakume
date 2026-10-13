# The Hyakume build book

What got built, how it works, and why, one step at a time. The newest step
is at the end. Decision records sit next to this file:
[pairing, identity, sessions, and the wire protocol](pairing-identity-session.md)
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
├── receive.rs      receiving side, a state machine with no I/O
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
