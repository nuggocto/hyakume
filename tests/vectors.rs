//! Published protocol and cryptographic test vectors, rebuilt from fixed
//! inputs so a wire or key-schedule change cannot pass unnoticed. After an
//! intended change, run `HYAKUME_WRITE_VECTORS=1 cargo test --locked --test
//! vectors` and review the diff.

// Test helpers may stop the test on unexpected failures.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fmt::Write as _;
use std::path::PathBuf;

use hyakume::display::PeerText;
use hyakume::identity::DeviceKey;
use hyakume::ids::{ClaimId, InvitationId, TransferId};
use hyakume::invitation::{InvitationKind, InvitationSecret, InvitationToken, Route};
use hyakume::offer::{Offer, PayloadKind};
use hyakume::pairing::{self, ChannelBinding, ExchangeContext};
use hyakume::protocol::{
    self, CancelReason, ErrorCode, Hello, ItemResult, Message, Purpose, RejectReason, VersionRange,
};
use hyakume::session::{self, SessionBinding, Side};
use hyakume::words::WORDS;
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use zeroize::Zeroizing;

use curve25519_dalek::constants::ED25519_BASEPOINT_POINT;
use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsPoint};
use curve25519_dalek::scalar::Scalar;
use hkdf::Hkdf;
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};

const RNG_SEED: u64 = 0x6879_616b_756d_6531;
const SECRET: [u8; 32] = [0x55; 32];
const SPAKE_JOINER: &[u8] = b"hyakume v1 pairing joiner";
const SPAKE_INVITER: &[u8] = b"hyakume v1 pairing inviter";
/// spake2 0.4.0's blinding points for the Ed25519 group.
const SPAKE_M: [u8; 32] = hex32("15cfd18e385952982b6a8f8c7854963b58e34388c8e6dae891db756481a02312");
const SPAKE_N: [u8; 32] = hex32("f04f2e7eb734b2a8f8b472eaf9c3c632576ac64aea650b496a8a20ff00e583c3");

const fn hex32(text: &str) -> [u8; 32] {
    let t = text.as_bytes();
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = (nibble(t[2 * i]) << 4) | nibble(t[2 * i + 1]);
        i += 1;
    }
    out
}

const fn nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        _ => c - b'a' + 10,
    }
}

fn point(bytes: &[u8]) -> EdwardsPoint {
    CompressedEdwardsY(bytes.try_into().unwrap())
        .decompress()
        .unwrap()
}

/// spake2 0.4.0's password scalar: HKDF-SHA256 with an empty salt and info
/// "SPAKE2 pw", 48 bytes read as a big-endian number, reduced mod l.
fn password_scalar(password: &[u8]) -> Scalar {
    let mut okm = [0u8; 48];
    Hkdf::<Sha256>::new(Some(b""), password)
        .expand(b"SPAKE2 pw", &mut okm)
        .unwrap();
    let mut wide = [0u8; 64];
    for (i, b) in okm.iter().enumerate() {
        wide[47 - i] = *b;
    }
    Scalar::from_bytes_mod_order_wide(&wide)
}

/// A SPAKE2 ephemeral scalar: 64 bytes from the stream, reduced mod l.
fn ephemeral_scalar(mut rng: ChaCha20Rng) -> Scalar {
    let mut wide = [0u8; 64];
    rng.fill_bytes(&mut wide);
    Scalar::from_bytes_mod_order_wide(&wide)
}

fn field(h: &mut blake3::Hasher, bytes: &[u8]) {
    h.update(&(bytes.len() as u64).to_be_bytes());
    h.update(bytes);
}

fn contribution(h: &mut blake3::Hasher, c: &pairing::Contribution) {
    field(h, &c.share);
    field(h, &c.id_commit);
    field(h, &c.nonce);
    field(h, c.name.as_bytes());
}

fn derive(context: &str, key: &[u8], transcript: &[u8; 32]) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key(context);
    h.update(key);
    h.update(transcript);
    *h.finalize().as_bytes()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn key(fill: u8) -> DeviceKey {
    DeviceKey::from_seed(&Zeroizing::new([fill; 32]))
}

fn line(out: &mut String, name: &str, value: impl AsRef<str>) {
    let _ = writeln!(out, "{name} = {}", value.as_ref());
}

fn frame(out: &mut String, name: &str, msg: &Message) {
    line(out, name, hex(&protocol::encode_frame(msg).unwrap()));
}

fn generate() -> String {
    let mut out = String::new();
    out.push_str(
        "# Hyakume protocol version 1 test vectors.\n\
         # Public test material only: every key, secret, and seed below is synthetic.\n\
         # Regenerate: HYAKUME_WRITE_VECTORS=1 cargo test --locked --test vectors\n\
         # Implementations: spake2 0.4.0 (Ed25519 group), blake3 1.8.7, Ed25519 from iroh-base 1.3.0.\n\
         # Random values come from one ChaCha20 stream (rand_chacha 0.3) seeded with\n\
         # the u64 below, drawn in protocol order. The SPAKE2 scalars, the\n\
         # SPAKE2 output k, and the transcript are listed too, so a second\n\
         # implementation can check its key schedule without spake2 0.4.0's\n\
         # random-to-scalar step. Scalars are 32 bytes, little-endian. The\n\
         # verifier shows byte n as word n of the word list.\n",
    );
    line(&mut out, "rng.seed", format!("{RNG_SEED:#018x}"));

    out.push_str("\n[frames]\n");
    let transfer = TransferId::from_bytes([0x11; 16]);
    frame(
        &mut out,
        "hello.pair",
        &Message::Hello(Hello::new(Purpose::Pair)),
    );
    frame(
        &mut out,
        "hello.session",
        &Message::Hello(Hello::new(Purpose::Session)),
    );
    frame(
        &mut out,
        "claim.permanent",
        &Message::Claim {
            invitation: InvitationId::from_bytes([0x22; 16]),
            claim: ClaimId::from_bytes([0x33; 16]),
            kind: InvitationKind::Permanent,
        },
    );
    let offer = Offer::for_files(transfer, &[("notes.txt", b"hello"), ("empty", b"")]).unwrap();
    frame(&mut out, "offer.files", &Message::Offer(offer.fields()));
    line(&mut out, "offer.files.digest", hex(&offer.digest()));
    let text =
        Offer::for_text(transfer, PayloadKind::Text, "remember the milk".as_bytes()).unwrap();
    frame(&mut out, "offer.text", &Message::Offer(text.fields()));
    frame(
        &mut out,
        "offer.accept",
        &Message::OfferAccept {
            transfer,
            offer_digest: offer.digest(),
        },
    );
    frame(
        &mut out,
        "offer.reject",
        &Message::OfferReject {
            transfer,
            reason: RejectReason::Declined,
        },
    );
    frame(
        &mut out,
        "chunk",
        &Message::Chunk {
            transfer,
            item: 0,
            offset: 0,
            data: b"hello".to_vec(),
        },
    );
    frame(
        &mut out,
        "item.end",
        &Message::ItemEnd { transfer, item: 0 },
    );
    frame(
        &mut out,
        "item.result.published",
        &Message::ItemResult {
            transfer,
            item: 0,
            result: ItemResult::Published,
        },
    );
    frame(
        &mut out,
        "item.result.content_mismatch",
        &Message::ItemResult {
            transfer,
            item: 0,
            result: ItemResult::ContentMismatch,
        },
    );
    frame(
        &mut out,
        "cancel.revoked",
        &Message::Cancel {
            transfer,
            reason: CancelReason::Revoked,
        },
    );
    frame(
        &mut out,
        "error.not_trusted",
        &Message::Error(ErrorCode::NotTrusted),
    );

    out.push_str("\n[invitation]\n");
    let endpoint = *key(0x44).id().as_bytes();
    let token = InvitationToken {
        kind: InvitationKind::Permanent,
        id: InvitationId::from_bytes([0x22; 16]),
        secret: InvitationSecret::from_bytes([0x55; 32]),
        route: Route {
            endpoint,
            addresses: vec!["127.0.0.1:4433".parse().unwrap()],
        },
    };
    line(&mut out, "token.bytes", hex(&token.to_bytes().unwrap()));
    line(&mut out, "token.text", token.to_text().unwrap().as_str());

    out.push_str("\n[pairing]\n");
    let secret = InvitationSecret::from_bytes(SECRET);
    let (inviter, joiner) = (key(0x66), key(0x77));
    let binding = ChannelBinding {
        exporter: [0x88; 32],
        inviter_endpoint: [0x99; 32],
        joiner_endpoint: [0xaa; 32],
        negotiation: protocol::negotiation_digest(
            &protocol::encode_payload(&Message::Hello(Hello::new(Purpose::Pair))),
            &protocol::encode_payload(&Message::Hello(Hello::new(Purpose::Pair))),
        ),
    };
    let cx = ExchangeContext {
        binding: binding.clone(),
        kind: InvitationKind::Permanent,
        invitation: InvitationId::from_bytes([0x22; 16]),
        claim: ClaimId::from_bytes([0x33; 16]),
    };
    line(&mut out, "input.secret", hex(&SECRET));
    line(&mut out, "input.inviter_seed", hex(&[0x66; 32]));
    line(&mut out, "input.joiner_seed", hex(&[0x77; 32]));
    line(&mut out, "input.inviter_id", hex(inviter.id().as_bytes()));
    line(&mut out, "input.joiner_id", hex(joiner.id().as_bytes()));
    line(&mut out, "input.exporter", hex(&binding.exporter));
    line(
        &mut out,
        "input.inviter_endpoint",
        hex(&binding.inviter_endpoint),
    );
    line(
        &mut out,
        "input.joiner_endpoint",
        hex(&binding.joiner_endpoint),
    );
    line(&mut out, "input.negotiation", hex(&binding.negotiation));
    line(&mut out, "input.inviter_name", "inviter");
    line(&mut out, "input.joiner_name", "joiner");
    let name = |n: &str| PeerText::new(n.as_bytes().to_vec(), 64).unwrap();
    let mut rng = ChaCha20Rng::seed_from_u64(RNG_SEED);
    let (joiner_stream, transcript_cx) = (rng.clone(), cx.clone());
    let (j1, commit) =
        pairing::joiner_start(&secret, &joiner, &name("joiner"), cx.clone(), &mut rng).unwrap();
    let inviter_stream = rng.clone();
    frame(
        &mut out,
        "msg.1.commit",
        &Message::PairCommit(commit.clone()),
    );
    let commit_bytes = commit.0;
    let (i1, share) =
        pairing::inviter_on_commit(&secret, &inviter, &name("inviter"), cx, commit, &mut rng)
            .unwrap();
    frame(&mut out, "msg.2.share", &Message::PairShare(share.clone()));
    let (j2, reveal) = j1.on_share(share.clone()).unwrap();
    frame(
        &mut out,
        "msg.3.reveal",
        &Message::PairReveal(reveal.clone()),
    );
    let (i2, confirm_i) = i1.on_reveal(reveal.clone()).unwrap();
    frame(
        &mut out,
        "msg.4.confirm_inviter",
        &Message::PairConfirm(confirm_i.clone()),
    );
    let (j3, confirm_j) = j2.on_confirm(&confirm_i).unwrap();
    frame(
        &mut out,
        "msg.5.confirm_joiner",
        &Message::PairConfirm(confirm_j.clone()),
    );
    let (i3, proof_i) = i2.on_confirm(&confirm_j, &inviter).unwrap();
    frame(
        &mut out,
        "msg.6.identity_inviter",
        &Message::PairIdentity(proof_i.clone()),
    );
    let (joined, proof_j) = j3.on_identity(&proof_i, &joiner).unwrap();
    frame(
        &mut out,
        "msg.7.identity_joiner",
        &Message::PairIdentity(proof_j.clone()),
    );
    let invited = i3.on_identity(&proof_j).unwrap();
    assert_eq!(invited.verifier(), joined.verifier());
    line(
        &mut out,
        "verifier.bytes",
        hex(invited.verifier().as_bytes()),
    );
    line(&mut out, "verifier.words", invited.verifier().words());
    frame(
        &mut out,
        "msg.8.decision_inviter_yes",
        &Message::PairDecision(invited.decide(true)),
    );
    frame(
        &mut out,
        "msg.8.decision_joiner_yes",
        &Message::PairDecision(joined.decide(true)),
    );
    frame(
        &mut out,
        "msg.8.decision_joiner_no",
        &Message::PairDecision(joined.decide(false)),
    );

    // What a second implementation needs to check its own SPAKE2 and key
    // schedule. Each value is checked here against the messages above.
    let w = password_scalar(&SECRET);
    let x = ephemeral_scalar(joiner_stream.clone());
    let y = ephemeral_scalar(inviter_stream.clone());
    assert_eq!(
        ED25519_BASEPOINT_POINT * x + point(&SPAKE_M) * w,
        point(&reveal.share[1..])
    );
    assert_eq!(
        ED25519_BASEPOINT_POINT * y + point(&SPAKE_N) * w,
        point(&share.share[1..])
    );
    let (spake, joiner_share) = Spake2::<Ed25519Group>::start_a_with_rng(
        &Password::new(SECRET),
        &Identity::new(SPAKE_JOINER),
        &Identity::new(SPAKE_INVITER),
        &mut joiner_stream.clone(),
    );
    assert_eq!(joiner_share, reveal.share);
    let k = spake.finish(&share.share).unwrap();
    let mut h = blake3::Hasher::new_derive_key("hyakume v1 pairing transcript");
    field(&mut h, &transcript_cx.binding.exporter);
    field(&mut h, &transcript_cx.binding.inviter_endpoint);
    field(&mut h, &transcript_cx.binding.joiner_endpoint);
    field(&mut h, &transcript_cx.binding.negotiation);
    field(&mut h, b"permanent");
    field(&mut h, transcript_cx.invitation.as_bytes());
    field(&mut h, transcript_cx.claim.as_bytes());
    field(&mut h, &commit_bytes);
    contribution(&mut h, &share);
    contribution(&mut h, &reveal);
    let t = *h.finalize().as_bytes();
    let confirm_key = derive("hyakume v1 pairing confirm inviter", &k, &t);
    assert_eq!(
        *blake3::keyed_hash(&confirm_key, &t).as_bytes(),
        confirm_i.mac
    );
    line(
        &mut out,
        "spake2.id_a",
        String::from_utf8_lossy(SPAKE_JOINER),
    );
    line(
        &mut out,
        "spake2.id_b",
        String::from_utf8_lossy(SPAKE_INVITER),
    );
    line(&mut out, "spake2.m", hex(&SPAKE_M));
    line(&mut out, "spake2.n", hex(&SPAKE_N));
    line(&mut out, "spake2.password_scalar", hex(w.as_bytes()));
    line(&mut out, "spake2.joiner_scalar", hex(x.as_bytes()));
    line(&mut out, "spake2.inviter_scalar", hex(y.as_bytes()));
    line(&mut out, "pairing.k", hex(&k));
    line(&mut out, "pairing.transcript", hex(&t));

    out.push_str("\n[words]\n");
    line(&mut out, "verifier.word_list", WORDS.join(" "));

    out.push_str("\n[session]\n");
    let session_binding = SessionBinding {
        exporter: [0xbb; 32],
        dialer_endpoint: [0xcc; 32],
        listener_endpoint: [0xdd; 32],
        negotiation: protocol::negotiation_digest(
            &protocol::encode_payload(&Message::Hello(Hello::new(Purpose::Session))),
            &protocol::encode_payload(&Message::Hello(Hello::new(Purpose::Session))),
        ),
    };
    line(&mut out, "input.exporter", hex(&session_binding.exporter));
    line(
        &mut out,
        "input.dialer_endpoint",
        hex(&session_binding.dialer_endpoint),
    );
    line(
        &mut out,
        "input.listener_endpoint",
        hex(&session_binding.listener_endpoint),
    );
    line(
        &mut out,
        "input.negotiation",
        hex(&session_binding.negotiation),
    );
    let dialer_proof = session::prove(&joiner, &session_binding, Side::Dialer);
    let listener_proof = session::prove(&inviter, &session_binding, Side::Listener);
    frame(
        &mut out,
        "proof.dialer",
        &Message::SessionProof(dialer_proof),
    );
    frame(
        &mut out,
        "proof.listener",
        &Message::SessionProof(listener_proof),
    );

    out.push_str("\n[invalid]\n");
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty_payload", vec![]),
        ("unknown_tag", vec![0xc8]),
        ("truncated_chunk", hex_bytes("0d1111")),
        ("trailing_byte", {
            let mut p = protocol::encode_payload(&Message::ItemEnd { transfer, item: 0 });
            p.push(0);
            p
        }),
        ("unknown_error_code", vec![17, 0, 99]),
        ("decision_not_boolean", {
            let mut p = vec![8, 2];
            p.extend_from_slice(&[0; 32]);
            p
        }),
    ];
    for (name, payload) in cases {
        let result = protocol::decode_payload(&payload).expect_err("invalid case must fail");
        let shown = if payload.is_empty() {
            "(empty)".to_string()
        } else {
            hex(&payload)
        };
        line(
            &mut out,
            &format!("payload.{name}"),
            format!("{shown} -> {result:?}"),
        );
    }
    let oversized = ((1024 * 1024 - 3) as u32).to_be_bytes();
    line(
        &mut out,
        "frame.oversized_length",
        format!(
            "{} -> {:?}",
            hex(&oversized),
            protocol::decode_frame(&oversized).unwrap_err()
        ),
    );
    let newer = VersionRange { min: 2, max: 3 };
    line(
        &mut out,
        "negotiate.peer_2_to_3",
        format!("{:?}", protocol::negotiate(protocol::SUPPORTED, newer)),
    );
    let mut tampered = confirm_i;
    tampered.mac[0] ^= 1;
    line(
        &mut out,
        "pairing.tampered_confirm_inviter",
        hex(&tampered.mac),
    );
    out
}

fn hex_bytes(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn published_vectors_match_the_current_implementation() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vectors/protocol-v1.txt");
    let generated = generate();
    if std::env::var_os("HYAKUME_WRITE_VECTORS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &generated).unwrap();
    }
    let published = std::fs::read_to_string(&path)
        .expect("vectors/protocol-v1.txt is missing; set HYAKUME_WRITE_VECTORS=1 to create it");
    assert!(
        published == generated,
        "the implementation no longer matches the published vectors; review and regenerate"
    );
}
