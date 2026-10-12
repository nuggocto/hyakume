# Pairing, identity, sessions, and the wire protocol

Status: implemented and tested on loopback. External design review not yet
done; the verdicts below are provisional until it is. Recorded 2026-10-12.

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

Move to `spake2` 0.5 when it is stable, or record why not.

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
  online guess. An invitation takes at most 8 claims and lives 15 minutes;
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
   trust and cancel check, and answers ItemResult.
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
   the behaviour was broken.
2. `vectors/protocol-v1.txt` reproduces from public inputs.
3. An external reviewer has read this record and the vectors, and every
   finding has a disposition.

## External review package

Give the reviewer this record, `docs/resource-estimate.md`,
`vectors/protocol-v1.txt`, `src/protocol.rs`, `src/pairing.rs`,
`src/session.rs`, `src/invitation.rs`, `src/trust.rs`, `src/transfer.rs`,
`src/receive.rs`, `src/send.rs`, and the known limitations below. **Reviewer: not yet
identified.**

## Remaining risks

- No independent audit of `spake2`, and no external review of this
  composition yet.
- The SPAKE2 vectors depend on how `spake2` 0.4.0 turns random bytes into a
  scalar; another implementation would need the scalars, not only the seed.
- Each side answers yes only after checking that it can still save the trust
  (invitation open, alias free, room in the trust store), so a conflict that
  appears during the exchange makes both sides decline
  (`pairing_that_cannot_be_saved_on_one_side_is_saved_on_neither`). Two
  windows remain where one side can save trust and the other not: the
  connection dropping between the two decisions, and the inviting side's
  state changing between its answer and its commit. Recovery is to remove
  the device and pair again.

## Verdicts

- **SPAKE2 (`spake2` 0.4.0, A/B roles): go, provisional** on external review.
- **Separate device key with a per-process transport key: go.**
- **Exporter-bound exchange with committed verifier and per-claim binding:
  go, provisional** on external review.
- **Authenticated QUIC without extra content encryption: go.**
