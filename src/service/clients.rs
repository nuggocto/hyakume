//! Connections on the local socket: one request at a time per connection,
//! the input frames it reads, and the prompts the client holds. A node
//! prompt goes only to the client whose request asked for it.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use rand_core::{OsRng, RngCore};
use tokio::net::UnixStream;
use tokio::sync::{Notify, OwnedSemaphorePermit, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout, timeout_at};
use zeroize::Zeroizing;

use super::{
    HELLO_DEADLINE, INPUT_QUEUE, SECRET_DEADLINE, Service, WRITER_DRAIN, failed, hex, lock, ok,
};
use crate::ids::TransferId;
use crate::ipc::{
    self, Body, ClientMessage, ErrorCode, Event, Failure, Frame, Reply, Request, ServerMessage,
    StoresView,
};
use crate::limits::MAX_NAME_COLUMNS;
use crate::node::{PROMPT_QUEUE, Prompt};
use crate::secrets::{self, Availability};
use crate::tasks;

/// What a running request reads besides its first message.
pub(super) enum Input {
    Data(Vec<u8>),
    Secret(Zeroizing<Vec<u8>>),
    End,
}

pub(super) struct PendingPrompt {
    context: String,
    answer: oneshot::Sender<bool>,
    transfer: Option<TransferId>,
}

/// One connected client: its event queue and the prompts it holds.
pub(super) struct Client {
    events: mpsc::Sender<ServerMessage>,
    prompts: Mutex<HashMap<u64, PendingPrompt>>,
    /// Its event queue filled up; the connection ends.
    overloaded: Notify,
    /// Offers this client accepted, reported when they end.
    pub(super) accepted: Mutex<Vec<TransferId>>,
}

impl Client {
    pub(super) fn send(&self, msg: ServerMessage) {
        if self.events.try_send(msg).is_err() {
            self.overloaded.notify_one();
        }
    }

    pub(super) fn event(&self, id: u64, event: Event) {
        self.send(ServerMessage::Event { id, event });
    }

    /// Hands a question to this client. If it already holds as many as
    /// the node may ask, the question is dropped unanswered, which the
    /// node takes as having nobody to ask.
    pub(super) fn ask(
        &self,
        id: u64,
        context: String,
        transfer: Option<TransferId>,
        answer: oneshot::Sender<bool>,
        event: impl FnOnce(u64) -> Event,
    ) {
        let prompt = OsRng.next_u64();
        {
            let mut prompts = lock(&self.prompts);
            // Questions the node stopped waiting for, such as a withdrawn
            // offer, hold no place.
            prompts.retain(|_, p| !p.answer.is_closed());
            if prompts.len() >= PROMPT_QUEUE {
                return;
            }
            prompts.insert(
                prompt,
                PendingPrompt {
                    context,
                    answer,
                    transfer,
                },
            );
        }
        self.event(id, event(prompt));
    }

    /// Delivers an answer. Only a prompt this client holds, with the same
    /// context, can be answered; any other answer approves nothing.
    pub(super) fn answer(&self, prompt: u64, context: &str, accept: bool) -> bool {
        let Some(pending) = lock(&self.prompts).remove(&prompt) else {
            return false;
        };
        if pending.context != context {
            // The prompt is used up as a no.
            return false;
        }
        if pending.answer.send(accept).is_err() {
            return false;
        }
        if accept && let Some(transfer) = pending.transfer {
            lock(&self.accepted).push(transfer);
        }
        true
    }

    /// The client is gone: every question it holds is answered no.
    pub(super) fn decline_all(&self) {
        for (_, pending) in lock(&self.prompts).drain() {
            let _ = pending.answer.send(false);
        }
    }
}

/// Tells a client past the limit that the service is busy, then closes.
pub(super) async fn refuse_client(mut stream: UnixStream) {
    let reply = ServerMessage::Reply {
        id: 0,
        reply: Reply::Failed {
            failure: Failure::new(ErrorCode::Busy, "too many clients are connected"),
        },
    };
    let _ = timeout(HELLO_DEADLINE, ipc::write_message(&mut stream, &reply)).await;
}

pub(super) async fn serve_client(
    svc: Arc<Service>,
    stream: UnixStream,
    _permit: OwnedSemaphorePermit,
) {
    // Only this user's processes may talk to the service.
    match stream.peer_cred() {
        Ok(cred) if cred.uid() == rustix::process::geteuid().as_raw() => {}
        _ => return,
    }

    let (rd, mut wr) = stream.into_split();
    let mut rd = ipc::FrameReader::new(rd);
    let (events, mut outgoing) = mpsc::channel::<ServerMessage>(ipc::EVENT_QUEUE);
    let mut writer = tokio::spawn(async move {
        while let Some(msg) = outgoing.recv().await {
            if ipc::write_message(&mut wr, &msg).await.is_err() {
                break;
            }
        }
    });

    let client = Arc::new(Client {
        events,
        prompts: Mutex::new(HashMap::new()),
        overloaded: Notify::new(),
        accepted: Mutex::new(Vec::new()),
    });

    let hello = timeout(HELLO_DEADLINE, rd.next()).await;
    let version = match hello {
        Ok(Ok(Frame::Message(body))) => match ipc::decode::<ClientMessage>(&body) {
            Ok(ClientMessage::Hello { version }) => version,
            _ => 0,
        },
        _ => 0,
    };
    client.send(ServerMessage::Hello {
        version: ipc::VERSION,
    });
    if version == ipc::VERSION {
        client_loop(&svc, &client, &mut rd).await;
    }

    // The client is gone: its prompts are answered no, and its running
    // request ended with the loop.
    client.decline_all();
    drop(client);
    let written = match timeout(WRITER_DRAIN, &mut writer).await {
        Ok(written) => written,
        Err(_) => {
            writer.abort();
            writer.await
        }
    };
    tasks::observe(written);
}

pub(super) async fn client_loop(
    svc: &Arc<Service>,
    client: &Arc<Client>,
    rd: &mut ipc::FrameReader<tokio::net::unix::OwnedReadHalf>,
) {
    let mut input_tx: Option<mpsc::Sender<Input>> = None;
    let mut closing = svc.closing.subscribe();

    // Owns the running request so it ends when the connection does. The
    // request returns its reply instead of sending it, and the loop sends
    // it once the request is off this guard: a client that asks again
    // after reading a reply never finds the last request still running.
    struct Running(Option<JoinHandle<ServerMessage>>);
    impl Drop for Running {
        fn drop(&mut self) {
            if let Some(h) = self.0.take() {
                h.abort();
            }
        }
    }
    let mut guard = Running(None);
    // The running request is `stop`. The service is asked to stop only
    // once its reply is queued, so shutdown cannot end this connection
    // before the reply is on its way.
    let mut stops = false;

    loop {
        let finished = async {
            match guard.0.as_mut() {
                Some(h) => tasks::observe(h.await),
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            frame = rd.next() => {
                let input = match frame {
                    Ok(Frame::Message(body)) => match ipc::decode::<ClientMessage>(&body) {
                        Ok(ClientMessage::Request { id, request }) => {
                            if guard.0.is_some() {
                                client.send(ServerMessage::Reply {
                                    id,
                                    reply: failed(ErrorCode::Busy, "a request is already running on this connection"),
                                });
                                continue;
                            }
                            stops = matches!(request, Request::Stop {});
                            let (tx, rx) = mpsc::channel(INPUT_QUEUE);
                            let (svc, client) = (svc.clone(), client.clone());
                            guard.0 = Some(tokio::spawn(async move {
                                let reply = handle(&svc, &client, id, request, rx).await;
                                ServerMessage::Reply { id, reply }
                            }));
                            input_tx = Some(tx);
                            continue;
                        }
                        Ok(ClientMessage::Answer { prompt, context, accept }) => {
                            if !client.answer(prompt, &context, accept) {
                                client.send(ServerMessage::AnswerRefused { prompt });
                            }
                            continue;
                        }
                        Ok(ClientMessage::StreamEnd) => Input::End,
                        Ok(ClientMessage::Hello { .. }) | Err(_) => break,
                    },
                    Ok(Frame::Data(data)) => Input::Data(data),
                    Ok(Frame::Secret(secret)) => Input::Secret(secret),
                    Err(_) => break,
                };
                // Input for the running request; waiting here stops reading
                // the socket, which pushes back on the client. Input that
                // arrives after its request ended, such as the rest of a
                // refused stream, is dropped so the client can read why.
                if let Some(tx) = input_tx.as_ref() {
                    let _ = tx.send(input).await;
                }
            }
            reply = finished => {
                guard.0 = None;
                input_tx = None;
                if let Some(reply) = reply {
                    reply_then_stop(svc, client, reply, std::mem::take(&mut stops));
                }
            }
            () = client.overloaded.notified() => break,
            () = async { drop(closing.wait_for(|closing| *closing).await) } => break,
        }
    }

    // The request ends with its connection; observing it lets a panic in it
    // stop the process. A reply it finished in time still goes out.
    if let Some(handle) = guard.0.take() {
        handle.abort();
        if let Some(reply) = tasks::observe(handle.await) {
            reply_then_stop(svc, client, reply, stops);
        }
    }
}

pub(super) fn reply_then_stop(svc: &Service, client: &Client, reply: ServerMessage, stops: bool) {
    client.send(reply);
    if stops {
        svc.stop.notify_one();
    }
}

pub(super) async fn handle(
    svc: &Arc<Service>,
    client: &Arc<Client>,
    id: u64,
    request: Request,
    mut inputs: mpsc::Receiver<Input>,
) -> Reply {
    if svc.stopping.load(Ordering::SeqCst) {
        return failed(ErrorCode::Stopping, "the service is stopping");
    }
    // A request reads input only while it needs it; past that, input is
    // refused at once instead of filling a queue nobody reads.
    let takes_input = matches!(
        request,
        Request::Init { .. }
            | Request::Unlock {}
            | Request::Join { .. }
            | Request::SendStream { .. }
    );
    if !takes_input {
        inputs.close();
    }
    let result = match request {
        Request::Status {} => Ok(Body::Status(svc.status())),
        Request::Stores {} => Ok(Body::Stores(StoresView {
            secret_service: match secrets::probe().await {
                Availability::Unlocked => "unlocked",
                Availability::Locked => "locked",
                Availability::Unavailable => "unavailable",
            }
            .into(),
            key_file: true,
        })),
        // A client that leaves during a lock-state transition ends its wait,
        // never the transition: a drain cut short would leave work running
        // that the next unlock could not see.
        Request::Init { store, name } => {
            let s = svc.clone();
            svc.own(async move { s.init(store, name, &mut inputs).await })
                .await
                .flatten()
        }
        Request::Unlock {} => {
            let s = svc.clone();
            svc.own(async move { s.unlock(&mut inputs).await })
                .await
                .flatten()
        }
        Request::Lock {} => {
            let s = svc.clone();
            svc.own(async move { s.lock().await }).await.flatten()
        }
        Request::Invite { alias } => svc.invite(client, id, alias).await,
        Request::Join { alias } => svc.join(client, id, alias, &mut inputs).await,
        Request::Devices {} => Ok(svc.devices()),
        Request::Remove { device } => svc.remove(&device),
        Request::Receive { dir } => svc.receive(client, id, dir).await,
        Request::SendFiles {
            device,
            route,
            paths,
        } => svc.send_files(client, id, &device, &route, paths).await,
        Request::SendStream {
            device,
            route,
            kind,
            name,
        } => {
            svc.send_stream(client, id, &device, &route, kind, name, &mut inputs)
                .await
        }
        Request::Cancel { transfer } => svc.cancel(&transfer).await,
        Request::Inspect { transfer } => svc
            .find_transfer(&transfer)
            .and_then(|t| svc.transfer_view(t))
            .map(Body::Transfer),
        Request::Transfers {} => Ok(svc.transfers()),
        Request::Cleanup {} => svc.cleanup().await,
        Request::Reset {} => {
            let s = svc.clone();
            svc.own(async move { s.reset().await }).await.flatten()
        }
        // The connection asks the service to stop once this reply is
        // queued; see `client_loop`.
        Request::Stop {} => Ok(Body::Done),
    };
    match result {
        Ok(body) => ok(body),
        Err(failure) => Reply::Failed { failure },
    }
}

/// Waits for the next piece of streamed input until `deadline`.
pub(super) async fn next_input(
    inputs: &mut mpsc::Receiver<Input>,
    deadline: Instant,
) -> Result<Input, Failure> {
    // A frame that is ready wins over a timer that has run out, so the
    // deadline is checked again: a backlog of input must not outlast it.
    match timeout_at(deadline, inputs.recv()).await {
        Ok(Some(input)) if Instant::now() < deadline => Ok(input),
        Ok(None) => Err(Failure::new(ErrorCode::Failed, "the input stopped")),
        _ => Err(Failure::new(
            ErrorCode::Refused,
            "the input did not end in time; nothing was offered",
        )),
    }
}

/// Waits for the credential a request needs.
pub(super) async fn secret_input(
    inputs: &mut mpsc::Receiver<Input>,
) -> Result<Zeroizing<Vec<u8>>, Failure> {
    let received = timeout(SECRET_DEADLINE, inputs.recv()).await;
    inputs.close();
    match received {
        Ok(Some(Input::Secret(secret))) => Ok(secret),
        Ok(Some(_)) => Err(Failure::new(ErrorCode::Malformed, "expected a credential")),
        _ => Err(Failure::new(ErrorCode::Malformed, "no credential arrived")),
    }
}

/// Hands each question from the node to the client that owns it. A
/// question nobody owns is answered no as it drops.
pub(super) async fn route_prompts(svc: Arc<Service>, mut prompts: mpsc::Receiver<Prompt>) {
    while let Some(prompt) = prompts.recv().await {
        match prompt {
            Prompt::Verify {
                invitation,
                peer,
                declared_name,
                verifier,
                answer,
            } => {
                let owner = lock(&svc.routes).pairing.get(&invitation).cloned();
                let Some((client, id)) = owner else {
                    continue;
                };
                let mut h = blake3::Hasher::new();
                h.update(invitation.as_bytes());
                h.update(peer.as_bytes());
                h.update(verifier.as_bytes());
                let context = hex(h.finalize().as_bytes());
                let shown_context = context.clone();
                client.ask(id, context, None, answer, |prompt| Event::Verify {
                    prompt,
                    context: shown_context,
                    fingerprint: peer.fingerprint(),
                    name: declared_name.line(MAX_NAME_COLUMNS).to_string(),
                    words: verifier.words(),
                });
            }
            Prompt::Offer {
                transfer,
                view,
                digest,
                answer,
                ..
            } => {
                let owner = lock(&svc.routes)
                    .receiver
                    .as_ref()
                    .map(|(c, id, _)| (c.clone(), *id));
                let Some((client, id)) = owner else {
                    continue;
                };
                let context = hex(&digest);
                let shown_context = context.clone();
                client.ask(id, context, Some(transfer), answer, |prompt| Event::Offer {
                    prompt,
                    context: shown_context,
                    transfer: hex(transfer.as_bytes()),
                    screen: view.render(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn input_still_queued_when_its_deadline_passes_is_refused() {
        let (tx, mut inputs) = mpsc::channel(INPUT_QUEUE);
        tx.send(Input::Data(b"more".to_vec())).await.unwrap();
        tx.send(Input::End).await.unwrap();

        let now = Instant::now();
        let Err(refused) = next_input(&mut inputs, now).await else {
            panic!("input was taken after its deadline");
        };
        assert_eq!(refused.code, ErrorCode::Refused);
        // Before the deadline, the same queue is read as usual.
        let later = now + Duration::from_secs(60);
        assert!(matches!(
            next_input(&mut inputs, later).await,
            Ok(Input::End)
        ));
    }

    fn client() -> (Client, mpsc::Receiver<ServerMessage>) {
        let (events, rx) = mpsc::channel(ipc::EVENT_QUEUE);
        let client = Client {
            events,
            prompts: Mutex::new(HashMap::new()),
            overloaded: Notify::new(),
            accepted: Mutex::new(Vec::new()),
        };
        (client, rx)
    }

    /// Hands `client` one prompt and returns its number and the node's side.
    fn prompt(
        client: &Client,
        rx: &mut mpsc::Receiver<ServerMessage>,
    ) -> (u64, oneshot::Receiver<bool>) {
        let (answer, node) = oneshot::channel();
        client.ask(1, "ctx".into(), None, answer, |prompt| Event::Offer {
            prompt,
            context: "ctx".into(),
            transfer: "00".into(),
            screen: String::new(),
        });
        match rx.try_recv().unwrap() {
            ServerMessage::Event {
                event: Event::Offer { prompt, .. },
                ..
            } => (prompt, node),
            other => panic!("expected a prompt, got {other:?}"),
        }
    }

    #[test]
    fn only_the_connection_holding_a_prompt_can_answer_it_with_its_context() {
        let (owner, mut events) = client();
        let (other, _) = client();
        let (number, mut node) = prompt(&owner, &mut events);
        // Another connection that learns the number approves nothing.
        assert!(!other.answer(number, "ctx", true));
        assert!(node.try_recv().is_err());
        assert!(owner.answer(number, "ctx", true));
        assert_eq!(node.try_recv(), Ok(true));
        // A prompt is answered once.
        assert!(!owner.answer(number, "ctx", true));
    }

    #[test]
    fn an_answer_with_the_wrong_context_uses_the_prompt_up_as_a_no() {
        let (owner, mut events) = client();
        let (number, mut node) = prompt(&owner, &mut events);
        assert!(!owner.answer(number, "another offer", true));
        // The node sees no answer, which counts as no.
        assert!(matches!(
            node.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
        assert!(!owner.answer(number, "ctx", true));
    }

    #[test]
    fn a_client_holding_as_many_prompts_as_the_node_may_ask_gets_no_more() {
        let (owner, mut events) = client();
        let held: Vec<_> = (0..PROMPT_QUEUE)
            .map(|_| prompt(&owner, &mut events))
            .collect();
        let (answer, mut node) = oneshot::channel();
        owner.ask(1, "ctx".into(), None, answer, |prompt| Event::Offer {
            prompt,
            context: "ctx".into(),
            transfer: "00".into(),
            screen: String::new(),
        });
        assert!(events.try_recv().is_err(), "the extra prompt was shown");
        assert!(matches!(
            node.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
        drop(held);
    }

    #[test]
    fn questions_the_node_stopped_waiting_for_hold_no_place() {
        let (owner, mut events) = client();
        let held: Vec<_> = (0..PROMPT_QUEUE)
            .map(|_| prompt(&owner, &mut events))
            .collect();
        // The node gave up on every one, as when the offers were withdrawn.
        drop(held);
        let (number, mut node) = prompt(&owner, &mut events);
        assert!(owner.answer(number, "ctx", true));
        assert_eq!(node.try_recv(), Ok(true));
    }
}
