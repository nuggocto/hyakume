//! Binding the endpoint, dialing, and the accept loop. The accept loop
//! owns every connection and send task, so stopping it reaches them all.

use std::net::SocketAddr;
use std::sync::Arc;

use iroh::endpoint::{
    Connection, Incoming, PortmapperConfig, QuicTransportConfig, VarInt, presets,
};
use iroh::{Endpoint, EndpointAddr, NetReportConfig, RelayMode};
use rand_core::{OsRng, RngCore};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::{Instant, timeout_at};
use zeroize::Zeroizing;

use super::exchange::invite_exchange;
use super::receiving::receive_session;
use super::wire::Wire;
use super::{ALPN, NodeError, QUIC_IDLE_TIMEOUT, QUIC_WINDOW_BYTES, Shared, Task};
use crate::limits::ConnectionSlot;
use crate::protocol::{self, Hello, Message, Purpose};
use crate::tasks;

pub(super) async fn bind_endpoint(bind: SocketAddr) -> Result<Endpoint, NodeError> {
    let idle = QUIC_IDLE_TIMEOUT
        .try_into()
        .map_err(|_| NodeError::Bind("idle timeout out of range".into()))?;
    // One stream per connection, and windows that hold about one frame,
    // instead of QUIC's much larger defaults. See the resource estimate.
    let transport = QuicTransportConfig::builder()
        .max_idle_timeout(Some(idle))
        .max_concurrent_bidi_streams(VarInt::from_u32(1))
        .max_concurrent_uni_streams(VarInt::from_u32(0))
        .stream_receive_window(VarInt::from_u32(QUIC_WINDOW_BYTES))
        .receive_window(VarInt::from_u32(QUIC_WINDOW_BYTES))
        .send_window(u64::from(QUIC_WINDOW_BYTES))
        .build();
    let mut seed = Zeroizing::new([0u8; 32]);
    OsRng.fill_bytes(&mut *seed);
    // A fresh transport key every run, never stored. The long-term identity
    // is the separate device key.
    let transport_key = iroh::SecretKey::from_bytes(&seed);
    let mut net_report = NetReportConfig::minimal();
    net_report.https_probes = false;
    Endpoint::builder(presets::Minimal)
        .secret_key(transport_key)
        .transport_config(transport)
        .relay_mode(RelayMode::Disabled)
        .portmapper_config(PortmapperConfig::Disabled)
        .net_report_config(net_report)
        .clear_ip_transports()
        .bind_addr(bind)
        .map_err(|e| NodeError::Bind(e.to_string()))?
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .map_err(|e| NodeError::Bind(e.to_string()))
}

pub(super) fn exporter(conn: &Connection, label: &[u8]) -> Result<[u8; 32], NodeError> {
    let mut out = [0u8; 32];
    conn.export_keying_material(&mut out, label, b"")
        .map_err(|_| NodeError::ConnectionLost)?;
    Ok(out)
}

pub(super) async fn dial(
    shared: &Shared,
    endpoint: &Endpoint,
    addr: EndpointAddr,
    purpose: Purpose,
) -> Result<(Connection, Wire, [u8; 32]), NodeError> {
    let deadline = shared.limits().handshake_deadline;
    let slot = match purpose {
        Purpose::Pair => shared.budget.open_connection()?,
        Purpose::Session => shared.budget.open_session_connection()?,
    };

    let until = Instant::now() + deadline;
    let conn = timeout_at(until, endpoint.connect(addr, ALPN))
        .await
        .map_err(|_| NodeError::Timeout)?
        .map_err(|e| NodeError::Connect(e.to_string()))?;
    let (send, recv) = timeout_at(until, conn.open_bi())
        .await
        .map_err(|_| NodeError::Timeout)?
        .map_err(|_| NodeError::ConnectionLost)?;

    let mut wire = Wire::new(&conn, send, recv, shared, slot, until);
    let mine = protocol::encode_payload(&Message::Hello(Hello::new(purpose)));
    wire.write_payload(&mine).await?;

    let frame = wire.next(deadline).await?;
    let theirs = match protocol::decode_payload(&frame.payload)? {
        Message::Hello(h) if h.purpose == purpose => h,
        Message::Error(code) => return Err(NodeError::Peer(code)),
        _ => return Err(NodeError::Unexpected),
    };

    protocol::negotiate(protocol::SUPPORTED, theirs.versions)?;
    let negotiation = protocol::negotiation_digest(&mine, &frame.payload);
    Ok((conn, wire, negotiation))
}

pub(super) async fn accept_loop(
    shared: Arc<Shared>,
    endpoint: Endpoint,
    mut spawned: mpsc::Receiver<Task>,
) {
    let mut owned = JoinSet::new();
    loop {
        tokio::select! {
            incoming = endpoint.accept() => match incoming {
                // A connection that has not proved who it is costs a task
                // and QUIC windows until its handshake deadline, so their
                // number is bounded too.
                Some(incoming) => match shared.budget.open_connection() {
                    Ok(slot) => {
                        let (shared, endpoint) = (shared.clone(), endpoint.clone());
                        owned.spawn(serve(shared, endpoint, incoming, slot));
                    }
                    Err(_) => incoming.refuse(),
                },
                None => break,
            },
            Some(task) = spawned.recv() => {
                owned.spawn(task);
            }
            Some(done) = owned.join_next(), if !owned.is_empty() => {
                tasks::observe(done);
            }
        }
    }
    // No new sends start once the endpoint is closed. Those still queued
    // never run; dropping them ends their transfers.
    spawned.close();
    while spawned.try_recv().is_ok() {}
    while let Some(done) = owned.join_next().await {
        tasks::observe(done);
    }
}

pub(super) async fn serve(
    shared: Arc<Shared>,
    endpoint: Endpoint,
    incoming: Incoming,
    slot: ConnectionSlot,
) {
    let deadline = shared.limits().handshake_deadline;
    // One deadline for everything before the peer proves who it is, so a
    // slow peer cannot stretch it step by step.
    let until = Instant::now() + deadline;
    let accepted = timeout_at(until, async {
        let conn = incoming.accept().ok()?.await.ok()?;
        let (send, recv) = conn.accept_bi().await.ok()?;
        Some((conn, send, recv))
    })
    .await;

    let Ok(Some((conn, send, recv))) = accepted else {
        drop(slot);
        shared.cell.notify();
        return;
    };

    let mut wire = Wire::new(&conn, send, recv, &shared, slot, until);
    let result = serve_connection(&shared, &endpoint, &conn, &mut wire).await;
    // The session's capacity was released when its state machine dropped.
    shared.cell.notify();
    if let Err(e) = &result {
        wire.send_error(e).await;
    }
    // Finishing consumes the wire, which releases the connection's slot.
    wire.finish_listener(&conn, deadline).await;
    shared.cell.notify();
}

pub(super) async fn serve_connection(
    shared: &Arc<Shared>,
    endpoint: &Endpoint,
    conn: &Connection,
    wire: &mut Wire,
) -> Result<(), NodeError> {
    let deadline = shared.limits().handshake_deadline;
    let frame = wire.next(deadline).await?;
    let hello = match protocol::decode_payload(&frame.payload)? {
        Message::Hello(h) => h,
        _ => return Err(NodeError::Unexpected),
    };
    protocol::negotiate(protocol::SUPPORTED, hello.versions)?;

    let mine = protocol::encode_payload(&Message::Hello(Hello::new(hello.purpose)));
    wire.write_payload(&mine).await?;
    let negotiation = protocol::negotiation_digest(&frame.payload, &mine);
    // The session outlives the Hello, so its buffer lease goes now.
    drop(frame);

    match hello.purpose {
        Purpose::Pair => invite_exchange(shared, endpoint, conn, wire, negotiation).await,
        Purpose::Session => receive_session(shared, endpoint, conn, wire, negotiation).await,
    }
}
