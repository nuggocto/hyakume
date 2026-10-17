//! Receiving, sending, and cancelling. Streamed input is spooled and
//! sealed before anything is offered, and its bytes stay charged until the
//! file ledger owns the file.

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::sync::Arc;

use rand_core::OsRng;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, timeout_at};

use super::clients::{Client, Input, next_input};
use super::{
    CANCEL_WAIT, Phase, STDIN_DEADLINE, Service, hex, lock, maintain, node_failure, shown,
};
use crate::files::destination::Destination;
use crate::files::names::OutputName;
use crate::files::source::{PreparedItem, Sealed, Source, Spool, SpoolError, prepare_file};
use crate::identity::DeviceId;
use crate::ids::TransferId;
use crate::invitation::Route;
use crate::ipc::{self, Body, ErrorCode, Event, Failure, StreamKind};
use crate::limits::{
    MAX_BATCH_ITEMS, MAX_PREVIEW_BYTES, MAX_SPOOL_BYTES, MAX_SPOOL_TOTAL, MAX_TEXT_BYTES,
    MAX_URL_BYTES,
};
use crate::node::{self, NodeError, Prepared, SendOptions};
use crate::offer::PayloadKind;
use crate::state::{State, is_paused};
use crate::storage::{FileKind, NewFile, StoreError};

impl Service {
    pub(super) async fn receive(
        self: &Arc<Self>,
        client: &Arc<Client>,
        id: u64,
        dir: Option<Vec<u8>>,
    ) -> Result<Body, Failure> {
        self.with_node(|_| ())?;
        let path = match dir {
            Some(bytes) => PathBuf::from(OsString::from_vec(bytes)),
            None => self.config.locations.app_storage(),
        };

        let shown_path = shown(path.as_os_str().as_bytes());
        let qualify = path.clone();
        let dest = self
            .blocking(move || Destination::qualify(&qualify))
            .await?
            .map_err(|e| {
                Failure::new(
                    ErrorCode::Destination,
                    format!("cannot receive into {shown_path}: {e}"),
                )
            })?;
        let dest = Arc::new(dest);

        {
            let mut routes = lock(&self.routes);
            if routes.receiver.is_some() {
                return Err(Failure::new(
                    ErrorCode::Busy,
                    "another client is already receiving",
                ));
            }
            routes.receiver = Some((client.clone(), id, dest.clone()));
            // While locked, the next unlock applies the registered
            // destination.
            let _ = self.with_node(|n| n.set_destination(Some(dest.clone())));
        }
        let _registered = ReceiverRoute { svc: self.clone() };
        client.event(id, Event::Receiving { dir: shown_path });
        let mut changes = self.cell.subscribe();

        loop {
            let ended: Vec<TransferId> = {
                let s = self.cell.lock();
                let mut accepted = lock(&client.accepted);
                let ended = accepted
                    .iter()
                    .copied()
                    .filter(|t| {
                        s.transfer(t)
                            .is_none_or(|t| t.is_terminal() || is_paused(t))
                    })
                    .collect();
                accepted.retain(|t| {
                    s.transfer(t)
                        .is_some_and(|t| !t.is_terminal() && !is_paused(t))
                });
                ended
            };
            for transfer in ended {
                if let Ok(view) = self.transfer_view(transfer) {
                    client.event(id, Event::Received(view));
                }
            }
            if changes.changed().await.is_err() {
                return Ok(Body::Done);
            }
        }
    }

    pub(super) fn send_target(
        &self,
        device: &str,
        route: &str,
    ) -> Result<(DeviceId, iroh::EndpointAddr), Failure> {
        self.with_node(|_| ())?;
        let device = self.find_device(device)?;
        let route = Route::parse(route.trim())
            .map_err(|e| Failure::new(ErrorCode::Invalid, format!("the route: {e}")))?;
        let addr = node::route_addr(&route).map_err(node_failure)?;
        Ok((device, addr))
    }

    pub(super) async fn send_files(
        self: &Arc<Self>,
        client: &Arc<Client>,
        id: u64,
        device: &str,
        route: &str,
        paths: Vec<Vec<u8>>,
    ) -> Result<Body, Failure> {
        let (device, addr) = self.send_target(device, route)?;
        if paths.is_empty() || paths.len() > MAX_BATCH_ITEMS {
            return Err(Failure::new(
                ErrorCode::Invalid,
                "send 1 to 256 files at once",
            ));
        }

        let mut items = Vec::with_capacity(paths.len());
        for raw in &paths {
            if raw.len() > ipc::MAX_PATH_BYTES {
                return Err(Failure::new(ErrorCode::Invalid, "a path is too long"));
            }
            let path = PathBuf::from(OsString::from_vec(raw.clone()));
            if !path.is_absolute() {
                return Err(Failure::new(ErrorCode::Invalid, "paths must be absolute"));
            }
            let item = self
                .blocking(move || prepare_file(&path))
                .await?
                .map_err(|e| Failure::new(ErrorCode::Invalid, shown(e.to_string().as_bytes())))?;
            if items.iter().any(|i: &PreparedItem| i.name == item.name) {
                return Err(Failure::new(
                    ErrorCode::Invalid,
                    "two files have the same name; one batch needs distinct names",
                ));
            }
            items.push(item);
        }

        let prepared = Prepared {
            transfer: TransferId::random(&mut OsRng),
            kind: PayloadKind::Files,
            items,
            preview: Vec::new(),
            paths: paths.into_iter().map(Some).collect(),
        };

        let started = self.start_send(device, addr, prepared)?;
        self.follow_send(client, id, started).await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn send_stream(
        self: &Arc<Self>,
        client: &Arc<Client>,
        id: u64,
        device: &str,
        route: &str,
        kind: StreamKind,
        name: Option<String>,
        inputs: &mut mpsc::Receiver<Input>,
    ) -> Result<Body, Failure> {
        let (device, addr) = self.send_target(device, route)?;
        let transfer = TransferId::random(&mut OsRng);
        let name = match (kind, name) {
            (StreamKind::Stdin, Some(name)) => OutputName::new(name.as_bytes())
                .map_err(|e| Failure::new(ErrorCode::Invalid, format!("the name: {e}")))?
                .as_str()
                .to_owned(),
            (StreamKind::Stdin, None) => format!("stdin-{}", &hex(transfer.as_bytes())[..8]),
            (_, Some(_)) => {
                return Err(Failure::new(
                    ErrorCode::Invalid,
                    "only standard input takes a name",
                ));
            }
            (_, None) => String::new(),
        };

        let (limit, keep, payload) = match kind {
            StreamKind::Text => (MAX_TEXT_BYTES, MAX_TEXT_BYTES as usize, PayloadKind::Text),
            StreamKind::Url => (MAX_URL_BYTES, MAX_URL_BYTES as usize, PayloadKind::Url),
            StreamKind::Stdin => (MAX_SPOOL_BYTES, 0, PayloadKind::Files),
        };
        let spool = Spool::create(&self.config.locations.spool(), transfer, limit, keep)
            .map_err(|e| Failure::new(ErrorCode::Storage, e.to_string()))?;
        let mut input = InputSpool {
            spool,
            charge: SpoolCharge {
                svc: self.clone(),
                bytes: 0,
            },
            transfer,
        };

        let deadline = Instant::now() + self.config.stdin_deadline.min(STDIN_DEADLINE);
        loop {
            match next_input(inputs, deadline).await? {
                Input::End => {
                    inputs.close();
                    break;
                }
                Input::Secret(_) => {
                    return Err(Failure::new(ErrorCode::Malformed, "unexpected credential"));
                }
                Input::Data(data) => {
                    input.charge.add(data.len() as u64)?;
                    let (back, written) = self
                        .blocking(move || {
                            let written = input.spool.write(&data);
                            (input, written)
                        })
                        .await?;
                    input = back;
                    written.map_err(|e| {
                        let code = match e {
                            SpoolError::TooLarge(_) => ErrorCode::TooLarge,
                            _ => ErrorCode::Storage,
                        };
                        Failure::new(code, format!("{e}; nothing was offered"))
                    })?;
                }
            }
        }

        // Text and links fit whole in the head.
        if payload != PayloadKind::Files && std::str::from_utf8(input.spool.head()).is_err() {
            return Err(Failure::new(
                ErrorCode::Invalid,
                "text and links must be UTF-8",
            ));
        }
        // From the end of input on, the offer is the service's to make, so a
        // client that leaves now cannot strand a sealed spool.
        let svc = self.clone();
        let started = self
            .own(async move { svc.offer_input(input, device, addr, payload, name).await })
            .await
            .flatten()?;
        self.follow_send(client, id, started).await
    }

    /// Seals finished input, hands its spool to the ledger, and starts its
    /// send. A spool no transfer took is deleted.
    pub(super) async fn offer_input(
        &self,
        input: InputSpool,
        device: DeviceId,
        addr: iroh::EndpointAddr,
        kind: PayloadKind,
        name: String,
    ) -> Result<Started, Failure> {
        let (mut input, sealed) = self
            .blocking(move || {
                let mut input = input;
                let sealed = input.spool.seal();
                (input, sealed)
            })
            .await?;
        let sealed: Sealed = sealed.map_err(|e| Failure::new(ErrorCode::Storage, e.to_string()))?;
        let Source::File { identity, .. } = &sealed.source else {
            return Err(Failure::new(ErrorCode::Failed, "a spool is a file"));
        };

        // The ledger charges the sealed spool from now until it is deleted.
        let transfer = input.transfer;
        input
            .keep(&NewFile {
                kind: FileKind::Spool,
                transfer,
                item: 0,
                dir: &self.spool,
                name: sealed.name.as_bytes(),
                identity: Some(identity.key()),
                bytes: sealed.size,
            })
            .map_err(|e| Failure::new(ErrorCode::Storage, format!("{e}; nothing was offered")))?;

        let preview = match kind {
            PayloadKind::Files => Vec::new(),
            _ => sealed.head[..sealed.head.len().min(MAX_PREVIEW_BYTES)].to_vec(),
        };

        let prepared = Prepared {
            transfer,
            kind,
            items: vec![PreparedItem {
                name: name.into_bytes(),
                size: sealed.size,
                digest: sealed.digest,
                source: sealed.source,
            }],
            preview,
            paths: vec![None],
        };
        let started = self.start_send(device, addr, prepared);
        if started.is_err() && self.cell.lock().transfer(&transfer).is_none() {
            // No transfer took the spool, so it goes.
            let marked = self.cell.change(|s| {
                let now = s.now_ms();
                s.store().delete_files_of(transfer, now)
            });
            if let Err(e) = marked {
                tracing::error!(error = %e, "could not mark an unused spool for deletion");
            }
        }
        started
    }

    pub(super) fn start_send(
        &self,
        device: DeviceId,
        addr: iroh::EndpointAddr,
        prepared: Prepared,
    ) -> Result<Started, Failure> {
        self.with_node(|n| n.start_send(device, addr, prepared, SendOptions::default()))?
            .map_err(node_failure)
    }

    /// Tells the client the send started and waits for its end. The send
    /// belongs to the node, so a client that stops waiting does not stop it.
    pub(super) async fn follow_send(
        &self,
        client: &Client,
        id: u64,
        (transfer, done): Started,
    ) -> Result<Body, Failure> {
        client.event(
            id,
            Event::Started {
                transfer: hex(transfer.as_bytes()),
            },
        );
        if let Ok(Err(e)) = done.await {
            return Err(node_failure(e));
        }
        self.transfer_view(transfer).map(Body::Transfer)
    }

    pub(super) async fn cancel(&self, text: &str) -> Result<Body, Failure> {
        let id = self.find_transfer(text)?;
        self.cell.change(|s| s.request_cancel(id));
        let mut changes = self.cell.subscribe();
        let deadline = Instant::now() + CANCEL_WAIT;
        loop {
            let settled = self
                .cell
                .lock()
                .transfer(&id)
                .is_none_or(|t| t.is_terminal() || is_paused(t));
            if settled {
                break;
            }
            if timeout_at(deadline, changes.changed()).await.is_err() {
                break;
            }
        }
        self.transfer_view(id).map(Body::Transfer)
    }

    pub(super) async fn cleanup(self: &Arc<Self>) -> Result<Body, Failure> {
        let retried = self
            .cell
            .change(|s| {
                let now = s.now_ms();
                s.store().retry_retained(now)
            })
            .map_err(|e| Failure::new(ErrorCode::Storage, e.to_string()))?;
        maintain(self.clone()).await;
        Ok(Body::Cleanup { retried })
    }
}

/// Bytes of one input being spooled, counted in `spooling` until sealed,
/// then in the ledger. Admission and the handoff both lock `spooling`
/// first, then the state, so no byte is ever counted twice or not at all.
pub(super) struct SpoolCharge {
    svc: Arc<Service>,
    bytes: u64,
}

impl SpoolCharge {
    pub(super) fn add(&mut self, more: u64) -> Result<(), Failure> {
        let mut spooling = lock(&self.svc.spooling);
        let charged = self
            .svc
            .cell
            .lock()
            .store()
            .charged()
            .map(|(spools, _)| spools)
            .map_err(|e| Failure::new(ErrorCode::Storage, e.to_string()))?;
        let total = charged
            .checked_add(*spooling)
            .and_then(|t| t.checked_add(more));
        if total.is_none_or(|t| t > MAX_SPOOL_TOTAL) {
            return Err(Failure::new(
                ErrorCode::TooLarge,
                "spool storage is full; nothing was offered",
            ));
        }
        *spooling += more;
        self.bytes += more;
        Ok(())
    }

    /// Moves the bytes counted while the spool was written into the ledger
    /// row `record` adds, as one step under `spooling`.
    pub(super) fn hand_off(
        &mut self,
        record: impl FnOnce(&mut State) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let mut spooling = lock(&self.svc.spooling);
        self.svc.cell.change(record)?;
        *spooling = spooling.saturating_sub(self.bytes);
        self.bytes = 0;
        Ok(())
    }

    /// Leaves the bytes counted after this charge ends, for a spool that
    /// could be neither recorded nor deleted. The next start removes the
    /// spool, and the count starts again from the ledger.
    pub(super) fn keep(&mut self) {
        self.bytes = 0;
    }
}

impl Drop for SpoolCharge {
    fn drop(&mut self) {
        let mut spooling = lock(&self.svc.spooling);
        *spooling = spooling.saturating_sub(self.bytes);
    }
}

/// A send that started: its transfer, and the node's word on how it ended.
pub(super) type Started = (TransferId, oneshot::Receiver<Result<(), NodeError>>);

/// Input being spooled, with the bytes counted for it. Its bytes stay
/// counted until its file is gone or the ledger has it: dropped before
/// [`InputSpool::keep`], it deletes the file, and a file that cannot be
/// deleted goes to the ledger, still charged, for cleanup to retry.
pub(super) struct InputSpool {
    spool: Spool,
    charge: SpoolCharge,
    transfer: TransferId,
}

impl InputSpool {
    /// Hands the sealed spool to the ledger, which charges it from now on.
    pub(super) fn keep(&mut self, file: &NewFile<'_>) -> Result<(), StoreError> {
        self.charge.hand_off(|s| s.add_file(file).map(drop))?;
        self.spool.keep();
        Ok(())
    }
}

impl Drop for InputSpool {
    fn drop(&mut self) {
        let Err((e, left)) = self.spool.discard() else {
            return;
        };
        tracing::warn!(error = %e, "could not delete unfinished input; cleanup retries");
        let svc = self.charge.svc.clone();
        let file = NewFile {
            kind: FileKind::Spool,
            transfer: self.transfer,
            item: 0,
            dir: &svc.spool,
            name: left.name.as_bytes(),
            identity: left.identity,
            bytes: left.bytes,
        };
        if self
            .charge
            .hand_off(|s| s.add_file_to_delete(&file))
            .is_err()
        {
            tracing::error!("unfinished input could be neither deleted nor recorded");
            self.charge.keep();
        }
    }
}

/// Unregisters the receiving client and stops taking offers when its
/// request ends. The node's destination only changes with the routes
/// locked, so a receiver leaving cannot clear the one that replaced it.
struct ReceiverRoute {
    svc: Arc<Service>,
}

impl Drop for ReceiverRoute {
    fn drop(&mut self) {
        let mut routes = lock(&self.svc.routes);
        routes.receiver = None;
        if let Phase::Unlocked { node, .. } = &*lock(&self.svc.phase) {
            node.set_destination(None);
        }
    }
}
