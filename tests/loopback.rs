//! Journeys between two endpoints over real loopback QUIC connections.
//!
//! Each test owns two nodes with synthetic identities and in-memory state.
//! People's answers come from the test through the prompt channels. Tests
//! wait for observable state with deadlines. Only tests about the handshake
//! deadline sleep, because letting it pass is what they test.

// Test helpers may stop the test on unexpected failures.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use hyakume::identity::{DeviceId, DeviceKey};
use hyakume::ids::TransferId;
use hyakume::invitation::{InvitationToken, TOKEN_PREFIX};
use hyakume::limits::{Limits, MAX_CHUNK_BYTES, MAX_CLAIMANTS, MIN_BUFFER_BYTES, Usage};
use hyakume::node::{Node, NodeConfig, NodeError, PROMPT_QUEUE, Payload, Prompt, SendOptions};
use hyakume::pairing::{PairingError, Verifier};
use hyakume::protocol::{self, ErrorCode, Hello, Message, Purpose};
use hyakume::state::State;
use hyakume::transfer::{
    BatchOutcome, CancelCause, FailCause, ItemState, OfferState, RejectCause, Unsuccessful,
};
use hyakume::trust::TrustError;
use rand_core::OsRng;
use tokio::sync::{Semaphore, mpsc};
use tokio::time::{Instant, sleep_until, timeout};

const DEADLINE: Duration = Duration::from_secs(20);
const MIB: usize = 1024 * 1024;

struct Peer {
    node: Node,
    prompts: mpsc::Receiver<Prompt>,
}

async fn peer(name: &str) -> Peer {
    peer_with(name, Limits::COMPILED).await
}

async fn peer_with(name: &str, limits: Limits) -> Peer {
    peer_config(NodeConfig {
        name: name.into(),
        limits,
        ..NodeConfig::default()
    })
    .await
}

async fn peer_config(config: NodeConfig) -> Peer {
    let (node, prompts) = Node::bind(config, DeviceKey::generate(&mut OsRng))
        .await
        .unwrap();
    Peer { node, prompts }
}

/// Answers the next verifier prompt and returns the words shown.
async fn answer_verify(prompts: &mut mpsc::Receiver<Prompt>, accept: bool) -> (DeviceId, Verifier) {
    match timeout(DEADLINE, prompts.recv()).await.unwrap().unwrap() {
        Prompt::Verify {
            peer,
            verifier,
            answer,
            ..
        } => {
            answer.send(accept).unwrap();
            (peer, verifier)
        }
        Prompt::Offer { .. } => panic!("expected a verifier prompt"),
    }
}

/// Answers the next offer prompt and returns the rendered consent screen.
async fn answer_offer(prompts: &mut mpsc::Receiver<Prompt>, accept: bool) -> String {
    match timeout(DEADLINE, prompts.recv()).await.unwrap().unwrap() {
        Prompt::Offer { view, answer, .. } => {
            answer.send(accept).unwrap();
            view.render()
        }
        Prompt::Verify { .. } => panic!("expected an offer prompt"),
    }
}

/// Pairs `a` (inviting) and `b` (joining), both answering yes. Returns the
/// verifier each side showed.
async fn pair(a: &mut Peer, b: &mut Peer) -> (Verifier, Verifier) {
    let token = a.node.invite("b").unwrap();
    let join = b.node.join(&token, "a");
    let (joined, (_, va), (_, vb)) = timeout(DEADLINE, async {
        tokio::join!(
            join,
            answer_verify(&mut a.prompts, true),
            answer_verify(&mut b.prompts, true)
        )
    })
    .await
    .unwrap();
    assert_eq!(joined.unwrap(), a.node.device());
    a.node
        .wait_until(DEADLINE, |s| s.trust().authorize(&b.node.device()).is_ok())
        .await
        .unwrap();
    (va, vb)
}

async fn send_and_answer(
    from: &mut Peer,
    to: &mut Peer,
    payload: Payload,
    accept: bool,
) -> (hyakume::node::SendReport, String) {
    let send = from.node.send(
        to.node.device(),
        to.node.addr(),
        payload,
        SendOptions::default(),
    );
    let (report, screen) = timeout(DEADLINE, async {
        tokio::join!(send, answer_offer(&mut to.prompts, accept))
    })
    .await
    .unwrap();
    (report.unwrap(), screen)
}

/// Waits until the receiving node reports the same final outcome.
async fn settled(node: &Node, report: &hyakume::node::SendReport) {
    let transfer = report.transfer;
    node.wait_until(DEADLINE, |s| {
        s.transfer(&transfer).is_some_and(|t| t.is_terminal())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn pairing_shows_the_same_verifier_on_both_devices_and_saves_mutual_trust() {
    let (mut a, mut b) = (peer("alpha").await, peer("beta").await);
    let (va, vb) = pair(&mut a, &mut b).await;
    assert_eq!(va, vb);
    assert_eq!(a.node.trusted(), [b.node.device()]);
    assert_eq!(b.node.trusted(), [a.node.device()]);
}

#[tokio::test]
async fn people_who_answer_after_the_handshake_deadline_still_pair_and_receive() {
    let quick = Limits {
        handshake_deadline: Duration::from_secs(1),
        ..Limits::COMPILED
    };
    let late = quick.handshake_deadline + Duration::from_millis(500);
    let (mut a, mut b) = (
        peer_with("alpha", quick).await,
        peer_with("beta", quick).await,
    );
    let token = a.node.invite("b").unwrap();
    let join = b.node.join(&token, "a");
    let answer_late = async {
        let mut answers = Vec::new();
        for prompts in [&mut a.prompts, &mut b.prompts] {
            match timeout(DEADLINE, prompts.recv()).await.unwrap().unwrap() {
                Prompt::Verify { answer, .. } => answers.push(answer),
                Prompt::Offer { .. } => panic!("expected a verifier prompt"),
            }
        }
        sleep_until(Instant::now() + late).await;
        for answer in answers {
            answer.send(true).unwrap();
        }
    };
    let (joined, ()) = timeout(DEADLINE, async { tokio::join!(join, answer_late) })
        .await
        .unwrap();
    assert_eq!(joined.unwrap(), a.node.device());

    let send = b.node.send(
        a.node.device(),
        a.node.addr(),
        Payload::Text("slow yes".into()),
        SendOptions::default(),
    );
    let answer_late = async {
        match timeout(DEADLINE, a.prompts.recv()).await.unwrap().unwrap() {
            Prompt::Offer { answer, .. } => {
                sleep_until(Instant::now() + late).await;
                answer.send(true).unwrap();
            }
            Prompt::Verify { .. } => panic!("expected an offer prompt"),
        }
    };
    let (report, ()) = timeout(DEADLINE, async { tokio::join!(send, answer_late) })
        .await
        .unwrap();
    let report = report.unwrap();
    assert_eq!(report.outcome, BatchOutcome::Completed);
    settled(&a.node, &report).await;
    assert_eq!(a.node.received()[0].bytes, b"slow yes");
}

#[tokio::test]
async fn text_arrives_unchanged_over_a_reconnection_with_saved_identities() {
    let (mut a, mut b) = (peer("alpha").await, peer("beta").await);
    pair(&mut a, &mut b).await;
    let text = "remember the milk\nand the bread ✓".to_string();
    let (report, screen) = send_and_answer(&mut a, &mut b, Payload::Text(text.clone()), true).await;
    assert_eq!(report.outcome, BatchOutcome::Completed);
    assert!(screen.contains("remember the milk"));
    settled(&b.node, &report).await;
    let received = b.node.received();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].bytes, text.as_bytes());
    assert_eq!(received[0].from, a.node.device());
}

#[tokio::test]
async fn a_generated_file_batch_arrives_with_exact_length_bytes_and_digest() {
    let (mut a, mut b) = (peer("alpha").await, peer("beta").await);
    pair(&mut a, &mut b).await;
    // Several chunks, a partial last chunk, and an empty file.
    let big: Vec<u8> = (0..3 * 1024 * 1024 + 17)
        .map(|i| (i * 31 % 251) as u8)
        .collect();
    let files = vec![
        ("data.bin".to_string(), big.clone()),
        ("empty".to_string(), Vec::new()),
    ];
    let (report, screen) = send_and_answer(&mut b, &mut a, Payload::Files(files), true).await;
    assert_eq!(report.outcome, BatchOutcome::Completed);
    assert!(screen.contains("\"data.bin\""));
    settled(&a.node, &report).await;
    let received = a.node.received();
    assert_eq!(received.len(), 2);
    assert_eq!(received[0].bytes.len(), big.len());
    assert_eq!(blake3::hash(&received[0].bytes), blake3::hash(&big));
    assert!(received[1].bytes.is_empty());
}

#[tokio::test]
async fn a_rejected_offer_creates_no_received_payload() {
    let (mut a, mut b) = (peer("alpha").await, peer("beta").await);
    pair(&mut a, &mut b).await;
    let (report, _) = send_and_answer(
        &mut a,
        &mut b,
        Payload::Url("https://example.com".into()),
        false,
    )
    .await;
    assert_eq!(
        report.outcome,
        BatchOutcome::Unsuccessful(Unsuccessful {
            rejected: 1,
            ..Unsuccessful::default()
        })
    );
    settled(&b.node, &report).await;
    assert!(b.node.received().is_empty());
    assert_eq!(b.node.held_bytes(), 0);
}

/// The one transfer on `node` that has not finished.
fn open_transfer(node: &Node) -> TransferId {
    let open: Vec<_> = node
        .transfers()
        .into_iter()
        .filter(|r| matches!(r.outcome, BatchOutcome::Offered | BatchOutcome::InProgress))
        .collect();
    assert_eq!(open.len(), 1, "{open:?}");
    open[0].transfer
}

fn mid_item(s: &State) -> bool {
    s.transfers()
        .any(|t| t.items().first() == Some(&ItemState::Transferring))
}

#[tokio::test]
async fn cancelling_mid_transfer_ends_both_sides_and_releases_capacity() {
    let (mut a, mut b) = (peer("alpha").await, peer("beta").await);
    pair(&mut a, &mut b).await;
    let options = SendOptions {
        chunk_bytes: 1024,
        gate: Some(Arc::new(Semaphore::new(1))),
    };
    let payload = Payload::Files(vec![("big".into(), vec![9u8; 64 * 1024])]);
    let send = a
        .node
        .send(b.node.device(), b.node.addr(), payload, options);
    let drive = async {
        answer_offer(&mut b.prompts, true).await;
        // One chunk was allowed through; the receiver is now mid-item.
        b.node.wait_until(DEADLINE, mid_item).await.unwrap();
        assert!(a.node.cancel(open_transfer(&a.node)));
    };
    let (report, ()) = timeout(DEADLINE, async { tokio::join!(send, drive) })
        .await
        .unwrap();
    let report = report.unwrap();
    assert_eq!(report.items, [ItemState::Cancelled(CancelCause::Sender)]);
    settled(&b.node, &report).await;
    assert_eq!(
        b.node.report(report.transfer).unwrap().items,
        [ItemState::Cancelled(CancelCause::Sender)]
    );
    assert!(b.node.received().is_empty());
    assert_eq!(b.node.held_bytes(), 0);
    assert_eq!(a.node.usage(), Usage::default());
    b.node.wait_idle(DEADLINE).await.unwrap();
}

#[tokio::test]
async fn the_receiver_can_cancel_mid_transfer_too() {
    let (mut a, mut b) = (peer("alpha").await, peer("beta").await);
    pair(&mut a, &mut b).await;
    let options = SendOptions {
        chunk_bytes: 1024,
        gate: Some(Arc::new(Semaphore::new(1))),
    };
    let payload = Payload::Files(vec![("big".into(), vec![9u8; 64 * 1024])]);
    let send = a
        .node
        .send(b.node.device(), b.node.addr(), payload, options);
    let drive = async {
        answer_offer(&mut b.prompts, true).await;
        b.node.wait_until(DEADLINE, mid_item).await.unwrap();
        assert!(b.node.cancel(open_transfer(&b.node)));
    };
    let (report, ()) = timeout(DEADLINE, async { tokio::join!(send, drive) })
        .await
        .unwrap();
    let report = report.unwrap();
    assert_eq!(report.items, [ItemState::Cancelled(CancelCause::Receiver)]);
    settled(&b.node, &report).await;
    assert_eq!(
        b.node.report(report.transfer).unwrap().items,
        [ItemState::Cancelled(CancelCause::Receiver)]
    );
    assert!(b.node.received().is_empty());
    b.node.wait_idle(DEADLINE).await.unwrap();
    assert_eq!(b.node.held_bytes(), 0);
}

#[tokio::test]
async fn revoking_the_sender_mid_transfer_publishes_nothing_and_refuses_later_sessions() {
    let (mut a, mut b) = (peer("alpha").await, peer("beta").await);
    pair(&mut a, &mut b).await;
    let gate = Arc::new(Semaphore::new(1));
    let options = SendOptions {
        chunk_bytes: 1024,
        gate: Some(gate.clone()),
    };
    let payload = Payload::Files(vec![("big".into(), vec![1u8; 32 * 1024])]);
    let send = a
        .node
        .send(b.node.device(), b.node.addr(), payload, options);
    let drive = async {
        answer_offer(&mut b.prompts, true).await;
        b.node
            .wait_until(DEADLINE, |s| s.held_bytes() > 0)
            .await
            .unwrap();
        b.node.revoke(a.node.device()).unwrap();
    };
    let (report, ()) = timeout(DEADLINE, async { tokio::join!(send, drive) })
        .await
        .unwrap();
    let report = report.unwrap();
    assert_eq!(report.items, [ItemState::Cancelled(CancelCause::Revoked)]);
    settled(&b.node, &report).await;
    assert_eq!(
        b.node.report(report.transfer).unwrap().items,
        [ItemState::Cancelled(CancelCause::Revoked)]
    );
    assert!(b.node.received().is_empty());

    // The revoked device still trusts b, but b refuses the session.
    let retry = a
        .node
        .send(
            b.node.device(),
            b.node.addr(),
            Payload::Text("again".into()),
            SendOptions::default(),
        )
        .await;
    assert_eq!(retry.err(), Some(NodeError::Peer(ErrorCode::NotTrusted)));
    // b can no longer send to a either.
    let back = b
        .node
        .send(
            a.node.device(),
            a.node.addr(),
            Payload::Text("x".into()),
            SendOptions::default(),
        )
        .await;
    assert_eq!(back.err(), Some(NodeError::Trust(TrustError::Revoked)));
    assert!(b.node.received().is_empty());
}

#[tokio::test]
async fn fresh_pairing_after_revocation_allows_new_work_without_reviving_old_work() {
    let (mut a, mut b) = (peer("alpha").await, peer("beta").await);
    pair(&mut a, &mut b).await;
    let gate = Arc::new(Semaphore::new(0));
    let options = SendOptions {
        chunk_bytes: 1024,
        gate: Some(gate),
    };
    let payload = Payload::Files(vec![("old".into(), vec![2u8; 8 * 1024])]);
    let send = a
        .node
        .send(b.node.device(), b.node.addr(), payload, options);
    let drive = async {
        answer_offer(&mut b.prompts, true).await;
        b.node
            .wait_until(DEADLINE, |s| s.held_bytes() > 0)
            .await
            .unwrap();
        b.node.revoke(a.node.device()).unwrap();
    };
    let (old, ()) = timeout(DEADLINE, async { tokio::join!(send, drive) })
        .await
        .unwrap();
    let old = old.unwrap();
    settled(&b.node, &old).await;

    // Pair again: b invites this time, and both sides trust anew.
    let token = b.node.invite("a2").unwrap();
    let join = a.node.join(&token, "b2");
    let (joined, _, _) = timeout(DEADLINE, async {
        tokio::join!(
            join,
            answer_verify(&mut b.prompts, true),
            answer_verify(&mut a.prompts, true)
        )
    })
    .await
    .unwrap();
    joined.unwrap();
    b.node
        .wait_until(DEADLINE, |s| s.trust().authorize(&a.node.device()).is_ok())
        .await
        .unwrap();

    let (new, _) = send_and_answer(&mut a, &mut b, Payload::Text("new".into()), true).await;
    assert_eq!(new.outcome, BatchOutcome::Completed);
    settled(&b.node, &new).await;
    // The old transfer stays cancelled; only the new item was published.
    assert_eq!(
        b.node.report(old.transfer).unwrap().items,
        [ItemState::Cancelled(CancelCause::Revoked)]
    );
    let received = b.node.received();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].bytes, b"new");
}

#[tokio::test]
async fn a_wrong_secret_fails_pairing_and_trusts_no_one() {
    let (mut a, b) = (peer("alpha").await, peer("beta").await);
    let token = a.node.invite("b").unwrap();
    let mut parsed = InvitationToken::parse(&token).unwrap();
    let mut wrong = parsed.to_bytes().unwrap().to_vec();
    // Flip one bit of the secret (bytes 18..50 of the binary form).
    wrong[18] ^= 1;
    parsed = InvitationToken::from_bytes(&wrong).unwrap();
    let wrong_text = parsed.to_text().unwrap();
    assert!(wrong_text.starts_with(TOKEN_PREFIX));
    let result = timeout(DEADLINE, b.node.join(&wrong_text, "a"))
        .await
        .unwrap();
    assert_eq!(
        result.err(),
        Some(NodeError::Pairing(PairingError::ConfirmationFailed))
    );
    // Nobody was asked to compare words, and nobody trusts anyone.
    assert!(a.prompts.try_recv().is_err());
    assert!(a.node.trusted().is_empty());
    assert!(b.node.trusted().is_empty());
}

#[tokio::test]
async fn a_declined_verifier_saves_no_trust_and_closes_the_invitation() {
    let (mut a, mut b) = (peer("alpha").await, peer("beta").await);
    let token = a.node.invite("b").unwrap();
    let join = b.node.join(&token, "a");
    let (joined, _, _) = timeout(DEADLINE, async {
        tokio::join!(
            join,
            answer_verify(&mut a.prompts, true),
            answer_verify(&mut b.prompts, false)
        )
    })
    .await
    .unwrap();
    assert_eq!(
        joined.err(),
        Some(NodeError::Pairing(PairingError::Declined))
    );
    a.node
        .wait_until(DEADLINE, |s| s.trust().trusted().count() == 0)
        .await
        .unwrap();
    assert!(a.node.trusted().is_empty());
    assert!(b.node.trusted().is_empty());

    // The same invitation cannot be tried again.
    let again = timeout(DEADLINE, b.node.join(&token, "a")).await.unwrap();
    assert_eq!(
        again.err(),
        Some(NodeError::Peer(ErrorCode::InvitationUnavailable))
    );
}

#[tokio::test]
async fn a_used_invitation_cannot_pair_another_device() {
    let (mut a, mut b) = (peer("alpha").await, peer("beta").await);
    let token = a.node.invite("b").unwrap();
    let join = b.node.join(&token, "a");
    let (joined, _, _) = timeout(DEADLINE, async {
        tokio::join!(
            join,
            answer_verify(&mut a.prompts, true),
            answer_verify(&mut b.prompts, true)
        )
    })
    .await
    .unwrap();
    joined.unwrap();
    let c = peer("gamma").await;
    let reuse = timeout(DEADLINE, c.node.join(&token, "a")).await.unwrap();
    assert_eq!(
        reuse.err(),
        Some(NodeError::Peer(ErrorCode::InvitationUnavailable))
    );
    assert!(c.node.trusted().is_empty());
}

#[tokio::test]
async fn an_unanswered_verifier_times_out_without_trust() {
    let short = Limits {
        idle_timeout: Duration::from_millis(500),
        ..Limits::COMPILED
    };
    let (mut a, mut b) = (
        peer_with("alpha", short).await,
        peer_with("beta", short).await,
    );
    let token = a.node.invite("b").unwrap();
    let join = b.node.join(&token, "a");
    // a answers; b's person never does.
    let (joined, _) = timeout(DEADLINE, async {
        tokio::join!(join, answer_verify(&mut a.prompts, true))
    })
    .await
    .unwrap();
    assert!(joined.is_err());
    let _unanswered = b.prompts.recv().await;
    a.node
        .wait_until(DEADLINE, |s| s.trust().trusted().count() == 0)
        .await
        .unwrap();
    assert!(a.node.trusted().is_empty());
    assert!(b.node.trusted().is_empty());
}

#[tokio::test]
async fn an_unpaired_device_cannot_start_a_session() {
    let (a, b) = (peer("alpha").await, peer("beta").await);
    let result = a
        .node
        .send(
            b.node.device(),
            b.node.addr(),
            Payload::Text("hi".into()),
            SendOptions::default(),
        )
        .await;
    assert_eq!(result.err(), Some(NodeError::Trust(TrustError::NotTrusted)));
}

#[tokio::test]
async fn shutdown_stops_the_endpoint_within_its_deadline() {
    let quick = Limits {
        handshake_deadline: Duration::from_secs(2),
        ..Limits::COMPILED
    };
    let (mut a, mut b) = (peer("alpha").await, peer_with("beta", quick).await);
    pair(&mut a, &mut b).await;
    let (device, addr) = (a.node.device(), a.node.addr());
    timeout(DEADLINE, a.node.shutdown()).await.unwrap().unwrap();
    let after = timeout(
        DEADLINE,
        b.node.send(
            device,
            addr,
            Payload::Text("x".into()),
            SendOptions::default(),
        ),
    )
    .await
    .unwrap();
    assert!(after.is_err());
    assert!(b.node.received().is_empty());
}

#[tokio::test]
async fn connections_past_the_limit_are_refused_before_any_handshake() {
    let one_session = Limits {
        sessions: 1,
        handshake_deadline: Duration::from_secs(10),
        ..Limits::COMPILED
    };
    let (mut target, mut other) = (peer_with("target", one_session).await, peer("other").await);
    pair(&mut target, &mut other).await;
    target.node.wait_idle(DEADLINE).await.unwrap();
    // The target's own session holds a connection too. It waits for an
    // answer that never comes.
    let holding = target.node.send(
        other.node.device(),
        other.node.addr(),
        Payload::Text("hold".into()),
        SendOptions::default(),
    );
    tokio::pin!(holding);
    let _unanswered = timeout(DEADLINE, async {
        tokio::select! {
            _ = &mut holding => panic!("the held send ended"),
            prompt = other.prompts.recv() => prompt,
        }
    })
    .await
    .unwrap();
    let client = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .relay_mode(iroh::RelayMode::Disabled)
        .clear_ip_transports()
        .bind_addr("127.0.0.1:0")
        .unwrap()
        .bind()
        .await
        .unwrap();
    // Connections that have not proved who they are get one place per
    // possible claimant, whatever the sessions hold.
    let mut held = Vec::new();
    for _ in 0..MAX_CLAIMANTS {
        let conn = timeout(
            DEADLINE,
            client.connect(target.node.addr(), hyakume::node::ALPN),
        )
        .await
        .unwrap()
        .unwrap();
        held.push(conn);
    }
    let extra = timeout(
        DEADLINE,
        client.connect(target.node.addr(), hyakume::node::ALPN),
    )
    .await
    .unwrap();
    assert!(extra.is_err(), "the connection past the limit was accepted");
    assert_eq!(target.node.usage().connections, 1 + MAX_CLAIMANTS);
    drop(held);
}

#[tokio::test]
async fn a_peer_that_proves_nothing_gets_one_handshake_deadline_however_slowly_it_writes() {
    let quick = Limits {
        handshake_deadline: Duration::from_secs(2),
        ..Limits::COMPILED
    };
    let target = peer_with("target", quick).await;
    let client = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .relay_mode(iroh::RelayMode::Disabled)
        .clear_ip_transports()
        .bind_addr("127.0.0.1:0")
        .unwrap()
        .bind()
        .await
        .unwrap();
    let conn = timeout(
        DEADLINE,
        client.connect(target.node.addr(), hyakume::node::ALPN),
    )
    .await
    .unwrap()
    .unwrap();
    let start = Instant::now();
    let hello = protocol::encode_payload(&Message::Hello(Hello::new(Purpose::Session)));
    // Each step comes within 2 s of the one before, but the Hello is only
    // whole after 2.8 s.
    sleep_until(start + Duration::from_millis(1200)).await;
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(&u32::try_from(hello.len()).unwrap().to_be_bytes())
        .await
        .unwrap();
    sleep_until(start + Duration::from_millis(2800)).await;
    let _ = send.write_all(&hello).await;
    let reply = timeout(DEADLINE, async {
        let mut len = [0u8; 4];
        recv.read_exact(&mut len).await.ok()?;
        let mut payload = vec![0u8; usize::try_from(u32::from_be_bytes(len)).ok()?];
        recv.read_exact(&mut payload).await.ok()?;
        protocol::decode_payload(&payload).ok()
    })
    .await
    .unwrap();
    assert!(
        !matches!(reply, Some(Message::Hello(_))),
        "the target answered a Hello that came after its handshake deadline"
    );
    target.node.wait_idle(DEADLINE).await.unwrap();
    assert_eq!(target.node.usage(), Usage::default());
}

#[tokio::test]
async fn pairing_that_cannot_be_saved_on_one_side_is_saved_on_neither() {
    let (mut a, mut b, mut c) = (peer("alpha").await, peer("beta").await, peer("gamma").await);
    // Two invitations name their device "phone"; only one device can have it.
    let for_b = a.node.invite("phone").unwrap();
    let for_c = a.node.invite("phone").unwrap();
    let (b_device, c_device) = (b.node.device(), c.node.device());
    let b_join = b.node.join(&for_b, "a");
    let drive = async {
        // Let b's exchange reach the verifier on both sides, but hold a's answer.
        let held = match timeout(DEADLINE, a.prompts.recv()).await.unwrap().unwrap() {
            Prompt::Verify { peer, answer, .. } => {
                assert_eq!(peer, b_device);
                answer
            }
            Prompt::Offer { .. } => panic!("expected a verifier prompt"),
        };
        answer_verify(&mut b.prompts, true).await;
        // Meanwhile c pairs completely and takes the alias.
        let c_join = c.node.join(&for_c, "a");
        let (joined, (peer, _), _) = tokio::join!(
            c_join,
            answer_verify(&mut a.prompts, true),
            answer_verify(&mut c.prompts, true)
        );
        joined.unwrap();
        assert_eq!(peer, c_device);
        // Now a's person says yes to b, but a can no longer save b as "phone".
        held.send(true).unwrap();
    };
    let (b_result, ()) = timeout(DEADLINE, async { tokio::join!(b_join, drive) })
        .await
        .unwrap();
    assert_eq!(
        b_result.err(),
        Some(NodeError::Pairing(PairingError::Declined))
    );
    assert_eq!(a.node.trusted(), [c_device]);
    assert!(
        b.node.trusted().is_empty(),
        "b trusts a device that does not trust it"
    );
}

#[tokio::test]
async fn an_alias_held_by_a_pairing_that_answered_yes_cannot_be_taken() {
    let (mut a, mut b, mut c) = (peer("alpha").await, peer("beta").await, peer("gamma").await);
    let for_b = a.node.invite("phone").unwrap();
    let for_c = a.node.invite("phone").unwrap();
    let (b_device, c_device) = (b.node.device(), c.node.device());
    let b_join = b.node.join(&for_b, "a");
    let drive = async {
        // a's person says yes to b; b's person has not answered yet.
        let (peer, _) = answer_verify(&mut a.prompts, true).await;
        assert_eq!(peer, b_device);
        let held = match timeout(DEADLINE, b.prompts.recv()).await.unwrap().unwrap() {
            Prompt::Verify { answer, .. } => answer,
            Prompt::Offer { .. } => panic!("expected a verifier prompt"),
        };
        // Meanwhile c wants the same alias. Both people say yes, but a
        // already promised "phone" to b.
        let c_join = c.node.join(&for_c, "a");
        let (joined, (peer, _), _) = tokio::join!(
            c_join,
            answer_verify(&mut a.prompts, true),
            answer_verify(&mut c.prompts, true)
        );
        assert_eq!(peer, c_device);
        assert_eq!(
            joined.err(),
            Some(NodeError::Pairing(PairingError::Declined))
        );
        held.send(true).unwrap();
    };
    let (b_result, ()) = timeout(DEADLINE, async { tokio::join!(b_join, drive) })
        .await
        .unwrap();
    assert_eq!(b_result.unwrap(), a.node.device());
    a.node
        .wait_until(DEADLINE, |s| s.trust().authorize(&b_device).is_ok())
        .await
        .unwrap();
    assert_eq!(a.node.trusted(), [b_device]);
    assert!(c.node.trusted().is_empty());
}

#[tokio::test]
async fn abandoning_a_pairing_after_the_verifier_is_shown_closes_the_invitation() {
    let (mut a, mut b) = (peer("alpha").await, peer("beta").await);
    let token = a.node.invite("b").unwrap();
    let join = b.node.join(&token, "a");
    let shown = async {
        let on_a = timeout(DEADLINE, a.prompts.recv()).await.unwrap().unwrap();
        let on_b = timeout(DEADLINE, b.prompts.recv()).await.unwrap().unwrap();
        (on_a, on_b)
    };
    // Both screens show the words, nobody answers, and b gives up: the
    // join is dropped, which closes its connection.
    let unanswered = tokio::select! {
        _ = join => panic!("the join ended before anyone answered"),
        prompts = shown => prompts,
    };
    a.node.wait_idle(DEADLINE).await.unwrap();
    drop(unanswered);

    let again = timeout(DEADLINE, b.node.join(&token, "a")).await.unwrap();
    assert_eq!(
        again.err(),
        Some(NodeError::Peer(ErrorCode::InvitationUnavailable))
    );
    assert!(a.node.trusted().is_empty());
}

#[tokio::test]
async fn a_sender_that_disappears_mid_transfer_leaves_nothing_unfinished() {
    let (mut a, mut b) = (peer("alpha").await, peer("beta").await);
    pair(&mut a, &mut b).await;
    let options = SendOptions {
        chunk_bytes: 1024,
        gate: Some(Arc::new(Semaphore::new(1))),
    };
    let payload = Payload::Files(vec![("big".into(), vec![3u8; 64 * 1024])]);
    let send = a
        .node
        .send(b.node.device(), b.node.addr(), payload, options);
    let mid_transfer = async {
        answer_offer(&mut b.prompts, true).await;
        b.node.wait_until(DEADLINE, mid_item).await.unwrap();
    };
    // The caller abandons the send halfway: its future is dropped.
    tokio::select! {
        _ = send => panic!("the gated send cannot finish"),
        () = mid_transfer => {}
    }
    let transfer = a.node.transfers()[0].transfer;
    assert_eq!(
        a.node.report(transfer).unwrap().items,
        [ItemState::Cancelled(CancelCause::Sender)]
    );
    b.node
        .wait_until(DEADLINE, |s| {
            s.transfer(&transfer).is_some_and(|t| t.is_terminal())
        })
        .await
        .unwrap();
    assert!(b.node.received().is_empty());
    b.node.wait_idle(DEADLINE).await.unwrap();
    assert_eq!(b.node.held_bytes(), 0);
    assert_eq!(a.node.usage(), Usage::default());
}

#[tokio::test]
async fn a_receiver_that_stops_reading_cannot_hold_a_send_past_its_write_deadline() {
    let quick = Limits {
        idle_timeout: Duration::from_secs(2),
        ..Limits::COMPILED
    };
    let reads = Arc::new(Semaphore::new(Semaphore::MAX_PERMITS));
    let (mut a, mut b) = (
        peer_with("alpha", quick).await,
        peer_config(NodeConfig {
            name: "beta".into(),
            read_gate: Some(reads.clone()),
            ..NodeConfig::default()
        })
        .await,
    );
    pair(&mut a, &mut b).await;
    // b reads the session's Hello, proof, and offer, then nothing more, so
    // a's writes stall once QUIC's windows are full.
    reads.forget_permits(reads.available_permits());
    reads.add_permits(3);
    let payload = Payload::Files(vec![("big".into(), vec![5u8; 3 * 1024 * 1024])]);
    let send = a.node.send(
        b.node.device(),
        b.node.addr(),
        payload,
        SendOptions::default(),
    );
    let (report, _) = timeout(DEADLINE, async {
        tokio::join!(send, answer_offer(&mut b.prompts, true))
    })
    .await
    .expect("a stalled write held the send");
    assert_eq!(
        report.unwrap().items,
        [ItemState::Failed(FailCause::Connection)]
    );
    assert_eq!(a.node.usage(), Usage::default());
    b.node.wait_idle(DEADLINE).await.unwrap();
    assert!(b.node.received().is_empty());
    assert_eq!(b.node.held_bytes(), 0);
}

#[tokio::test]
async fn an_unanswered_offer_times_out_even_while_the_receiver_is_busy_elsewhere() {
    let quick = Limits {
        idle_timeout: Duration::from_secs(2),
        ..Limits::COMPILED
    };
    let (mut a, mut b) = (peer("alpha").await, peer_with("beta", quick).await);
    pair(&mut a, &mut b).await;
    let send = a.node.send(
        b.node.device(),
        b.node.addr(),
        Payload::Text("hi".into()),
        SendOptions::default(),
    );
    tokio::pin!(send);
    let _unanswered = timeout(DEADLINE, async {
        tokio::select! {
            _ = &mut send => panic!("the send ended before the offer was shown"),
            prompt = b.prompts.recv() => prompt,
        }
    })
    .await
    .unwrap();
    // Work that has nothing to do with this session changes b's state
    // every 100 ms, far more often than its 2 s idle timeout.
    let busy = async {
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        loop {
            tick.tick().await;
            b.node.take_received();
        }
    };
    let report = timeout(DEADLINE, async {
        tokio::select! {
            report = &mut send => report,
            () = busy => unreachable!(),
        }
    })
    .await
    .expect("other work kept an unanswered offer open past the idle timeout")
    .unwrap();
    b.node.wait_idle(DEADLINE).await.unwrap();
    assert_eq!(b.node.usage(), Usage::default());
    assert!(b.node.received().is_empty());
    assert_eq!(report.items, [ItemState::Rejected]);
}

#[tokio::test]
async fn a_sender_whose_reader_is_stuck_still_ends_when_the_receiver_closes() {
    let reads = Arc::new(Semaphore::new(Semaphore::MAX_PERMITS));
    let (mut a, mut b) = (
        peer_config(NodeConfig {
            name: "alpha".into(),
            read_gate: Some(reads.clone()),
            ..NodeConfig::default()
        })
        .await,
        peer("beta").await,
    );
    pair(&mut a, &mut b).await;
    a.node.wait_idle(DEADLINE).await.unwrap();
    // a reads the session's Hello and proof, then nothing more, the way a
    // reader waiting for buffer room would.
    reads.forget_permits(reads.available_permits());
    reads.add_permits(2);
    let send = a.node.send(
        b.node.device(),
        b.node.addr(),
        Payload::Text("hi".into()),
        SendOptions::default(),
    );
    tokio::pin!(send);
    let _unanswered = timeout(DEADLINE, async {
        tokio::select! {
            _ = &mut send => panic!("the send ended before the offer was shown"),
            prompt = b.prompts.recv() => prompt,
        }
    })
    .await
    .unwrap();
    timeout(DEADLINE, b.node.shutdown()).await.unwrap().unwrap();
    // The read gate stays shut: only the close can end the send.
    let report = timeout(DEADLINE, send)
        .await
        .expect("the send waited on its reader after the receiver closed")
        .unwrap();
    // a never read b's answer, so the offer ends withdrawn.
    a.node
        .wait_until(DEADLINE, |s| {
            s.transfer(&report.transfer)
                .is_some_and(|t| t.offer_state() == OfferState::Rejected(RejectCause::Withdrawn))
        })
        .await
        .unwrap();
    assert_eq!(a.node.usage(), Usage::default());
}

#[tokio::test]
async fn cancelling_a_send_that_waits_for_buffer_room_ends_it_at_once() {
    let smallest = Limits {
        buffer_bytes: MIN_BUFFER_BYTES,
        ..Limits::COMPILED
    };
    let (mut a, mut b) = (peer_with("alpha", smallest).await, peer("beta").await);
    pair(&mut a, &mut b).await;
    // The first send holds its chunk's buffer lease while it waits at a
    // closed gate. The second needs a full chunk, which takes all the room
    // this budget has for sending, so it waits.
    let gate = Arc::new(Semaphore::new(0));
    let first = a.node.send(
        b.node.device(),
        b.node.addr(),
        Payload::Text("x".into()),
        SendOptions {
            gate: Some(gate.clone()),
            ..SendOptions::default()
        },
    );
    tokio::pin!(first);
    // The first send takes its lease in the same poll that records the
    // acceptance, so it holds the lease before the second send starts.
    let accepted = |items: usize| {
        move |s: &State| {
            s.transfers()
                .any(|t| t.items().len() == items && t.offer_state() == OfferState::Accepted)
        }
    };
    timeout(DEADLINE, async {
        tokio::select! {
            _ = &mut first => panic!("the gated send cannot finish"),
            () = async {
                answer_offer(&mut b.prompts, true).await;
                a.node.wait_until(DEADLINE, accepted(1)).await.unwrap();
            } => {}
        }
    })
    .await
    .unwrap();
    let batch = vec![
        ("a".into(), vec![1u8; MAX_CHUNK_BYTES]),
        ("b".into(), vec![2u8; 4]),
    ];
    let second = a.node.send(
        b.node.device(),
        b.node.addr(),
        Payload::Files(batch),
        SendOptions::default(),
    );
    let drive = async {
        answer_offer(&mut b.prompts, true).await;
        a.node.wait_until(DEADLINE, accepted(2)).await.unwrap();
        let transfer = a.node.transfers().into_iter().find(|r| r.items.len() == 2);
        assert!(a.node.cancel(transfer.unwrap().transfer));
    };
    let (report, ()) = timeout(DEADLINE, async {
        tokio::select! {
            _ = &mut first => panic!("the gated send cannot finish"),
            both = async { tokio::join!(second, drive) } => both,
        }
    })
    .await
    .expect("a cancelled send kept waiting for buffer room");
    assert_eq!(
        report.unwrap().items,
        [
            ItemState::Cancelled(CancelCause::Sender),
            ItemState::Cancelled(CancelCause::Sender)
        ]
    );
    gate.add_permits(16);
    let first = timeout(DEADLINE, first).await.unwrap().unwrap();
    assert_eq!(first.outcome, BatchOutcome::Completed);
    assert_eq!(a.node.usage(), Usage::default());
}

#[tokio::test]
async fn a_receiver_with_a_lowered_budget_refuses_what_it_cannot_stage_and_takes_what_it_can() {
    // No room to stage a 60 KiB file next to one chunk sent and one frame
    // read: refused before the person is asked.
    let tiny = Limits {
        buffer_bytes: MIN_BUFFER_BYTES,
        ..Limits::COMPILED
    };
    let (mut a, mut b) = (peer("alpha").await, peer_with("beta", tiny).await);
    pair(&mut a, &mut b).await;
    let file = vec![4u8; 60 * 1024];
    let report = timeout(
        DEADLINE,
        a.node.send(
            b.node.device(),
            b.node.addr(),
            Payload::Files(vec![("f".into(), file.clone())]),
            SendOptions::default(),
        ),
    )
    .await
    .expect("the offer reached the person instead of being refused")
    .unwrap();
    a.node
        .wait_until(DEADLINE, |s| {
            s.transfer(&report.transfer)
                .is_some_and(|t| t.offer_state() == OfferState::Rejected(RejectCause::TooLarge))
        })
        .await
        .unwrap();
    assert!(b.prompts.try_recv().is_err());

    // Room for the file on top of that: it arrives.
    let roomy = Limits {
        buffer_bytes: MIN_BUFFER_BYTES + 64 * 1024,
        ..Limits::COMPILED
    };
    let (mut a, mut b) = (peer("alpha").await, peer_with("beta", roomy).await);
    pair(&mut a, &mut b).await;
    let (report, _) = send_and_answer(
        &mut a,
        &mut b,
        Payload::Files(vec![("f".into(), file.clone())]),
        true,
    )
    .await;
    assert_eq!(report.outcome, BatchOutcome::Completed);
    settled(&b.node, &report).await;
    assert_eq!(b.node.received()[0].bytes, file);
}

#[tokio::test]
async fn a_full_question_queue_refuses_new_offers_without_blocking_revocation() {
    let (mut a, mut b) = (peer("alpha").await, peer("beta").await);
    pair(&mut a, &mut b).await;
    let sender = Arc::new(a.node);
    let (to, at) = (b.node.device(), b.node.addr());
    // b never reads its questions, so they fill its queue.
    let mut waiting = tokio::task::JoinSet::new();
    for n in 0..PROMPT_QUEUE {
        let (sender, at) = (sender.clone(), at.clone());
        waiting.spawn(async move {
            sender
                .send(
                    to,
                    at,
                    Payload::Text(format!("note {n}")),
                    SendOptions::default(),
                )
                .await
        });
    }
    b.node
        .wait_until(DEADLINE, |s| s.transfers().count() == PROMPT_QUEUE)
        .await
        .unwrap();

    let refused = timeout(
        DEADLINE,
        sender.send(
            to,
            at,
            Payload::Text("one more".into()),
            SendOptions::default(),
        ),
    )
    .await
    .expect("an offer waited for room in the question queue")
    .unwrap();
    sender
        .wait_until(DEADLINE, |s| {
            s.transfer(&refused.transfer)
                .is_some_and(|t| t.offer_state() == OfferState::Rejected(RejectCause::Busy))
        })
        .await
        .unwrap();

    // Revocation ends every waiting session and frees its capacity.
    b.node.revoke(sender.device()).unwrap();
    b.node.wait_idle(DEADLINE).await.unwrap();
    let revoked = |s: &State| {
        s.transfers()
            .filter(|t| t.offer_state() == OfferState::Rejected(RejectCause::Revoked))
            .count()
    };
    b.node
        .wait_until(DEADLINE, |s| revoked(s) == PROMPT_QUEUE)
        .await
        .unwrap();
    while let Some(done) = timeout(DEADLINE, waiting.join_next()).await.unwrap() {
        assert_eq!(done.unwrap().unwrap().items, [ItemState::Rejected]);
    }
    assert!(b.node.received().is_empty());
    timeout(DEADLINE, b.node.shutdown()).await.unwrap().unwrap();
}

#[tokio::test]
async fn a_budget_set_below_the_minimum_still_sends_more_than_its_size() {
    let tiny = Limits {
        buffer_bytes: 64 * 1024,
        ..Limits::COMPILED
    };
    let (mut a, mut b) = (peer_with("alpha", tiny).await, peer("beta").await);
    pair(&mut a, &mut b).await;
    let (report, _) = send_and_answer(&mut a, &mut b, Payload::Text("x".into()), true).await;
    assert_eq!(report.outcome, BatchOutcome::Completed);
    let file = vec![7u8; MIN_BUFFER_BYTES as usize + MIB];
    let (report, _) = send_and_answer(
        &mut a,
        &mut b,
        Payload::Files(vec![("f".into(), file.clone())]),
        true,
    )
    .await;
    assert_eq!(report.outcome, BatchOutcome::Completed);
    settled(&b.node, &report).await;
    assert_eq!(b.node.received()[1].bytes, file);
}

/// `a` and `b` each send the other a file of `size` bytes at the same time.
/// Both people answer yes when `answer` is set.
async fn send_both_ways(
    a: &mut Peer,
    b: &mut Peer,
    size: usize,
    answer: bool,
) -> (hyakume::node::SendReport, hyakume::node::SendReport) {
    let file = |byte| Payload::Files(vec![("f".into(), vec![byte; size])]);
    let ab = a.node.send(
        b.node.device(),
        b.node.addr(),
        file(1),
        SendOptions::default(),
    );
    let ba = b.node.send(
        a.node.device(),
        a.node.addr(),
        file(2),
        SendOptions::default(),
    );
    let answers = async {
        if answer {
            tokio::join!(
                answer_offer(&mut a.prompts, true),
                answer_offer(&mut b.prompts, true)
            );
        }
    };
    let (ab, ba, ()) = timeout(DEADLINE, async { tokio::join!(ab, ba, answers) })
        .await
        .expect("sending both ways at once stalled");
    (ab.unwrap(), ba.unwrap())
}

#[tokio::test]
async fn sending_both_ways_at_once_on_lowered_budgets_completes_or_is_refused() {
    // Each node has room to stage 1 MiB next to one chunk sent and one
    // frame read.
    let lowered = Limits {
        buffer_bytes: MIN_BUFFER_BYTES + MIB as u64,
        ..Limits::COMPILED
    };
    let (mut a, mut b) = (
        peer_with("alpha", lowered).await,
        peer_with("beta", lowered).await,
    );
    pair(&mut a, &mut b).await;
    // Both fit: both arrive.
    let (ab, ba) = send_both_ways(&mut a, &mut b, MIB, true).await;
    assert_eq!(ab.outcome, BatchOutcome::Completed);
    assert_eq!(ba.outcome, BatchOutcome::Completed);
    settled(&b.node, &ab).await;
    settled(&a.node, &ba).await;
    assert_eq!(b.node.received()[0].bytes, vec![1u8; MIB]);
    assert_eq!(a.node.received()[0].bytes, vec![2u8; MIB]);

    // Staging either one would leave too little to send the other: both
    // are refused before anyone is asked.
    let (ab, ba) = send_both_ways(&mut a, &mut b, 3 * MIB, false).await;
    for (from, report) in [(&a.node, &ab), (&b.node, &ba)] {
        assert_eq!(report.items, [ItemState::Rejected]);
        from.wait_until(DEADLINE, |s| {
            s.transfer(&report.transfer)
                .is_some_and(|t| t.offer_state() == OfferState::Rejected(RejectCause::TooLarge))
        })
        .await
        .unwrap();
    }
    assert!(a.prompts.try_recv().is_err());
    assert!(b.prompts.try_recv().is_err());
}
