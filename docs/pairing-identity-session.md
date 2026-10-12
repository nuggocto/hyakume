# Pairing, identity, sessions, and the wire protocol

Status: implemented and tested on loopback. Two independent design reviews
were done on 2026-10-13; every finding and its outcome is under "External
review". Recorded 2026-10-12, updated 2026-10-13.

Hyakume composes existing primitives through maintained libraries. It
defines no new primitive.

## 1. PAKE

| Crate | Version (release) | Protocol | Maintenance and review | License | Verdict |
| --- | --- | --- | --- | --- | --- |
| `spake2` (RustCrypto) | 0.4.0 (2023-07-28); 0.5.0-pre.0 (2026-01-25) | SPAKE2 with A/B roles on Ed25519. Predates RFC 9382 and does not claim its exact transcript. | 1.42 M downloads; used by magic-wormhole. The README says it has never had an independent audit. It does not zeroize its internal state. | MIT OR Apache-2.0 | **go**, provisional |
| `pakery-cpace` | 0.6.0 (2026-09-22) | CPace on ristretto255 | First release 2026-03; 707 downloads; no review claim | MIT OR Apache-2.0 | **hold** |
| `pake-cpace` (jedisct1) | 0.1.7 (2023-12-15) | draft-haase-cpace-01 | Early draft, no recent releases | ISC | **replace** |
| `pake-cpace-embedded` | 0.2.2 (2026-09-22) | fork of `pake-cpace` | Same early draft | ISC | **replace** |
| `cpace` (hdevalence) | 0.1.0 (2020-05-01) | early draft | unmaintained | BSD-3-Clause | rejected |

Release dates, downloads, and licenses come from the crates.io API on
2026-10-12. An RFC or draft is not evidence that a Rust implementation is
maintained or reviewed.

The deciding difference is API shape. The construction below needs the
inviter to send its PAKE share before it sees the joiner's. `spake2`'s
`start_b_with_rng` and `finish` allow that. `pakery-cpace` takes the
initiator's share as an input to the responder's step, so the responder
cannot send first. CPace itself allows this ordering; this library does not.
CPace stays a candidate if a maintained crate exposes share generation on
its own. Asymmetric PAKEs such as OPAQUE do not fit two devices that hold the
same secret.

What goes on the wire is SPAKE2 as implemented by `spake2` 0.4.0, not
RFC 9382. The crate hashes the password to a scalar with HKDF-SHA256 (empty
salt, info `SPAKE2 pw`, 48 bytes, reduced mod l) and outputs
`k = SHA-256(SHA-256(pw) ‖ SHA-256(idA) ‖ SHA-256(idB) ‖ X ‖ Y ‖ K)`. No
document or interoperability plan should cite the RFC as the wire format.

The crate accepts any curve point as the peer's share. Hyakume refuses a
share outside the prime-order group before calling `finish`
(`a_share_with_a_small_order_part_is_refused`), because a small-order part
could leak a few bits of this side's scalar.

Move to `spake2` 0.5 when it is stable, or record why not. Before moving,
check whether 0.5 wipes the password and scalars from memory; 0.4.0 does not.

## 2. Identity and Iroh

A device's long-term identity is a separate Ed25519 key, the **device key**
(`src/identity.rs`). The Iroh transport key is generated for each process
and never stored.

Iroh evidence, from the `iroh` 1.3.0 and `iroh-base` 1.3.0 sources pinned in
`Cargo.lock`:

| Need | Iroh 1.3.0 API | Where |
| --- | --- | --- |
| Bind pairing and sessions to the TLS connection | `Connection::export_keying_material(&self, output, label, context)` | `iroh-1.3.0/src/endpoint/connection.rs:1115` |
| The peer's transport key, authenticated by the TLS handshake | `Connection::remote_id()` | `connection.rs:1167` |
| This endpoint's transport key | `Endpoint::id()` | `iroh-1.3.0/src/endpoint.rs:1199` |
| Fresh transport key per run | `SecretKey::from_bytes`, `Builder::secret_key` | `iroh-base-1.3.0/src/key.rs:337`, `endpoint.rs:531` |
| Strict signature checks for device keys | `PublicKey::verify` calls `verify_strict` | `key.rs:134` |
| Device key wiped on drop | `SecretKey` derives `ZeroizeOnDrop` | `key.rs:260` |
| No endpoint ID in TLS SNI | stopped after 1.0.3 | `iroh-1.3.0/src/tls/name.rs:3` |
| No public relays or address publication | `presets::Minimal`, `RelayMode::Disabled`, `PortmapperConfig::Disabled` | `src/node.rs` |

The loopback journeys in `tests/loopback.rs` exercise these calls over real
QUIC connections, so the binding is tested against the transport, not
assumed.

- **Why separate keys:** a stable transport key would be a stable identifier
  in discovery, relay traffic, and to anyone who connects. A key that rotates
  per process hides all three. The device key appears only inside
  authenticated, encrypted streams, and only after the PAKE succeeded.
- **Lost or corrupt identity:** creates a new device that must pair again.
  Persistence and loading come with the local service.
- **Revocation:** see `src/trust.rs`. Each grant of trust has a fresh epoch;
  revoking or pairing again makes every older authority stale.

## 3. Construction

### Exchange

```text
joiner (SPAKE2 A)                                 inviter (SPAKE2 B)
  Hello(pair) ↔ Hello(pair)                        negotiation digest N
  Claim(invitation, claim, kind) ->
  Commit = H_c(share_j, idc_j, nonce_j, name_j) ->
                       <- Share(share_i, idc_i, nonce_i, name_i)
  Reveal(share_j, idc_j, nonce_j, name_j) ->        check Commit
                       <- Confirm(MAC_i)
  Confirm(MAC_j) ->                                 joiner checked MAC_i
                       <- Identity(id_i, salt_i, sig_i(T))
  Identity(id_j, salt_j, sig_j(T)) ->               inviter checked MAC_j
  both show verifier words; each person answers
  Decision(yes/no, MAC) <-> Decision(yes/no, MAC)
```

### Key schedule and separation

All hashing is BLAKE3. `field(x)` is an 8-byte big-endian length followed by
`x`.

| Value | Definition |
| --- | --- |
| Exporter `E` | TLS exporter, 32 bytes, label `hyakume v1 pairing exporter`, empty context |
| Negotiation `N` | `derive_key("hyakume v1 version negotiation", len‖hello_dialer‖len‖hello_listener)` over the payloads exactly as sent |
| Identity commitment `idc` | `derive_key("hyakume v1 pairing identity commitment", id‖salt)` |
| Commitment | `derive_key("hyakume v1 pairing commitment", field(share)‖field(idc)‖field(nonce)‖field(name))` |
| Transcript `T` | `derive_key("hyakume v1 pairing transcript", field(E)‖field(inviter endpoint)‖field(joiner endpoint)‖field(N)‖field(kind)‖field(invitation)‖field(claim)‖field(commitment)‖inviter contribution‖joiner contribution)` |
| `K` | SPAKE2 output |
| Confirm keys | `derive_key("hyakume v1 pairing confirm inviter"/"... joiner", K‖T)` |
| Decision keys | `derive_key("hyakume v1 pairing decision inviter"/"... joiner", K‖T)` |
| Verifier | first 4 bytes of `derive_key("hyakume v1 pairing verifier", K‖T)`, shown as 4 words (32 bits) |
| Confirm MAC | `keyed_hash(confirm key, T)`, compared in constant time |
| Decision MAC | `keyed_hash(decision key, T‖accept)` |
| Identity signature | Ed25519 over `len(domain)‖domain‖T`, domain `hyakume v1 pairing identity inviter`/`... joiner` |
| Session transcript | `derive_key("hyakume v1 session transcript", E_s‖dialer endpoint‖listener endpoint‖N)`, `E_s` with label `hyakume v1 session exporter` |
| Session proof | Ed25519 over `len(domain)‖domain‖session transcript`, domain `hyakume v1 session dialer`/`... listener` |
| Offer digest | `derive_key("hyakume v1 offer digest", every offer field)` |

Each derived key has its own context, so no key, MAC, or signature made for
one purpose is valid for another.

### Properties and the tests that protect them

| Property | How | Test |
| --- | --- | --- |
| Wrong secret fails before identities | `K` differs; the confirmation MAC fails | `pairing::a_wrong_secret_fails_before_any_identity_is_revealed`, loopback `a_wrong_secret_fails_pairing_and_trusts_no_one` |
| No identity before both confirmations | device keys travel only after both MACs | `identities_are_absent_from_every_message_before_both_confirmations` |
| Bound to one connection | `E` differs per TLS session | `different_connections_fail_even_with_the_right_secret` |
| Interceptor with the secret is visible | unrelated verifiers on the two sides | `an_interceptor_with_the_secret_produces_different_verifiers_on_each_side` |
| No grinding of matching verifiers | the joiner commits first; the inviter sends before seeing the joiner's share; one guess per attempt, 2^-32 | `the_joiner_cannot_change_its_contribution_after_seeing_the_inviters` |
| Identity and role binding | commitment opening plus role-separated signatures | `substituting_the_committed_identity_is_detected`, `an_identity_signature_is_bound_to_its_role` |
| Negotiation binding | `N` is in `T` and the session transcript | `tampered_version_negotiation_fails_the_exchange`, `a_proof_is_bound_to_its_connection_and_role` |
| Invitation and claim binding | kind, invitation, and claim are in `T` | `disagreement_on_invitation_kind_or_claim_fails_the_exchange` |
| Per-claim verifier for temporary sharing | each claim has its own `E`, claim ID, and nonces | `each_temporary_claim_has_its_own_verifier` |
| Trust needs both yes answers | both decisions are MACed; trust comes only from `conclude_permanent` | `trust_needs_both_answers_to_be_yes`, `a_decision_cannot_be_forged_flipped_or_reflected` |
| A temporary claim never becomes trust | `conclude_temporary` returns a type with no path to the trust store | `a_temporary_claim_cannot_conclude_as_permanent_trust` |
| Session replay or reflection fails | exporter and role domain in every proof | `a_proof_is_bound_to_its_connection_and_role` |
| Listener reveals itself only to trusted devices | it verifies and authorizes the dialer first | loopback `revoking_the_sender_mid_transfer_publishes_nothing_and_refuses_later_sessions` |

### Replay protection and attempt limits

- Exporter values make every proof and MAC specific to one TLS connection.
- Claim IDs are unique per invitation, and transfer IDs unique per receiver;
  a repeated one is refused.
- Every claim that ends without proving the secret uses one of 5 attempts,
  whether it failed, timed out, or disconnected, because each could be one
  online guess. A claim still in progress holds one of the attempts left,
  so claims made at once never get more guesses than that
  (`claims_made_at_once_never_get_more_guesses_than_attempts_left`). An
  invitation takes at most 8 claims and lives 15 minutes;
  expiry counts only forward clock movement.
- Invitation tokens carry 256-bit secrets. Short human codes come with LAN
  pairing and use the same exchange.

### Content protection

QUIC with TLS 1.3 (rustls with the ring provider, through Iroh) encrypts and
authenticates everything on the connection, on direct and relayed paths.
Relays never hold TLS keys. The offer travels on the authenticated session
and fixes each item's exact length and BLAKE3 digest; the receiver checks
both before publication, and the acceptance names the offer digest. **No
second layer of content encryption.**

### Key lifetime

| Key | Lifetime | Destruction |
| --- | --- | --- |
| Device key | until reset; stored by the local service later | `ZeroizeOnDrop` |
| Transport key | one process | dropped with the endpoint |
| Invitation secret | until the invitation closes | `Zeroizing` |
| `K` and derived keys | one exchange | `Zeroizing` in Hyakume's code; `spake2` 0.4.0 does not zeroize its own state |

## 4. Wire protocol

`src/protocol.rs` defines the 17 messages and their limits, and
`vectors/protocol-v1.txt` holds their exact bytes. This section records what
the code does not spell out in one place.

- **Transport.** One QUIC connection per exchange, ALPN `hyakume/1`, one
  bidirectional stream that the dialer opens.
- **Framing.** A `u32be` length, then a one-byte tag and the fields. The
  receiver checks the frame length (at most 1 MiB) before reading the payload,
  and each variable field against its own limit before copying it. Leftover
  bytes, an unknown tag, or an unknown code end the exchange.
- **Versions.** Each side sends Hello with the versions it supports, and both
  use the highest one in common. The hash of both Hello payloads, as sent, is
  `N` above, so a changed Hello fails the exchange. Version 1 defines no
  capabilities and ignores unknown bits.
- **Pairing.** Any message out of the order in section 3, or an `Error`, ends
  the exchange. A claim that has not verified uses an attempt.

A session runs in this order:

1. Hello ↔ Hello.
2. The dialer sends its session proof. The listener verifies it and checks
   trust before sending its own. An untrusted dialer gets `Error(NotTrusted)`.
3. The dialer sends an offer. The receiver refuses it before asking the
   person when it breaks a size limit, does not fit the staging room, the
   buffer left after room for one chunk sent and one frame read, or the
   1,024 held items, exceeds transfer capacity, or arrives while eight
   questions already wait for an answer. Otherwise the person answers, and
   the receiver replies with OfferAccept, naming the offer digest, or
   OfferReject.
4. For each item in order, chunks with consecutive offsets, then ItemEnd. The
   receiver checks the exact length and digest, publishes only through the
   trust and cancel check, and answers ItemResult. Text that matches its
   digest but is not valid UTF-8, or differs from its preview, is answered
   with `ContentMismatch`.
5. Either side may send Cancel at any time. Revocation sends
   `Cancel(Revoked)`.
6. The dialer finishes its stream. The listener finishes after it and waits
   for the dialer to close the connection.

A duplicate, reordered, or out-of-step message ends the session with
`Error(UnexpectedMessage)` and fails the unfinished items. That covers a
chunk before acceptance, a chunk for another item or offset, a second offer
with a known transfer ID, and a result for an item that was not sent. Bytes
past an item's offered length fail it with a length mismatch. A side whose
write cannot finish, because the peer stopped reading or the transfer was
stopped mid-frame, resets the stream and closes the connection. The peer then
fails its unfinished items as a lost connection.

## Acceptance criteria

1. Every property in the table above has a test that was seen failing when
   the behaviour was broken. Met.
2. `vectors/protocol-v1.txt` reproduces from public inputs, and gives a
   second implementation the SPAKE2 scalars, `k`, the transcript, and the
   word list, each checked by `tests/vectors.rs` against the messages. Met.
3. An external reviewer has read this record and the vectors, and every
   finding has an outcome. Met on 2026-10-13; see below.

## External review

Two reviewers, working separately, read this record, the vectors, the
listed source, and the pinned `spake2` 0.4.0 and iroh 1.3.0 sources at
commit `a5b2e37`. Both reran the vectors and the test suite. Neither found
a way for an impostor to become trusted, and both gave every choice below
"go". Findings from the first review are F-1 to F-8, from the second C-01
to C-08; the same problem found by both is listed once.

| Finding | Severity | Outcome |
| --- | --- | --- |
| F-1, C-05: the inviter used up the invitation before saving trust, so a failed save left it used up with no trust saved | should fix | **Fixed.** `save_pairing` checks that trust can be saved before using the invitation; `a_pairing_that_cannot_be_saved_leaves_the_invitation_usable` |
| F-2, C-03: the vectors could not check the pairing key schedule without porting `spake2`'s scalar step | should fix | **Fixed.** The vectors list the identities, blinding points, password and ephemeral scalars, `k`, the transcript, and the word list |
| C-01: `spake2` 0.4.0 takes shares with a small-order part | note | **Fixed.** Such shares are refused before `finish` |
| F-6: text that failed its content check was reported to the sender as a digest mismatch | note | **Fixed.** New wire result `ContentMismatch` (code 6) |
| F-4: the record could be read as claiming RFC 9382 | note | **Fixed.** Section 1 names the construction as `spake2` 0.4.0's |
| Found while weighing C-04: up to 8 claims at once could each make a guess, against a limit of 5 | should fix | **Fixed.** A claim in progress holds an attempt |
| C-04, F-5: attempt limits are not tied to the strength of short human codes | before short codes | **Accepted as a condition** for LAN pairing; see below |
| F-3, C-02: `spake2` 0.4.0 does not wipe the password or scalars from memory | note | **Accepted.** Only the single-use invitation secret of one exchange is exposed, never the device key. Check 0.5 before upgrading |
| F-8, C-06: revocation stays final only if the epoch counter is saved with the trust store | note | **Accepted as a condition** for persistence; see below |
| F-7: shutdown leaves claims in progress until the invitation expires | note | **Accepted as a condition** for the local service; see below |
| C-07: silent connections can hold the 8 places without a session | note | **Accepted.** Bounded by one 30 s deadline per connection, listed in `docs/resource-estimate.md` |
| C-08: a connection lost after verification closes the invitation | note | **Kept as is.** Each invitation allows one comparison of verifier words, which is what makes 32 bits enough. A retry would give someone holding the secret another try |
| C-04 (optional): a 40-bit verifier | note | **Not adopted.** One comparison per invitation keeps 32 bits enough; revisit only if short codes change that |

Assumptions the reviewers stated, kept here so later work checks them:

- Clients never send TLS 0-RTT data: `Node::dial` waits for the full
  handshake. Check again whenever the iroh TLS setup changes.
- Content protection relies on relays never terminating TLS. Restate this
  when relay work begins.

## Conditions for later work

- **Short human codes (LAN pairing).** The chance that a guesser finds the
  code within one invitation must stay at or below 2^-20: attempts (5)
  divided by the number of possible codes. Codes therefore need at least
  23 bits, such as 7 random decimal digits. Keep the same attempt
  accounting and the 120 s code lifetime.
- **Persistence.** Save `TrustStore`'s next epoch in the same write as the
  trust records, and never load a lower one.
- **Local service.** When the service stops, settle each claim in progress
  as failed, so an invitation never keeps claims that no exchange owns.

## Remaining risks

- No independent audit of the `spake2` crate itself.
- One window remains where one side saves trust and the other does not:
  the connection dropping between the two decisions. Recovery is to remove
  the device and pair again. It never trusts an impostor. Before an answer,
  each side checks it can still save the trust, so a conflict during the
  exchange makes both sides decline
  (`pairing_that_cannot_be_saved_on_one_side_is_saved_on_neither`).

## Verdicts

- **SPAKE2 (`spake2` 0.4.0, A/B roles): go** for 256-bit invitation tokens.
  Short human codes need the condition above first.
- **Separate device key with a per-process transport key: go.**
- **Exporter-bound exchange with committed verifier and per-claim binding:
  go.**
- **Authenticated QUIC without extra content encryption: go.**
