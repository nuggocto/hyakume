//! `/v1/events`: one WebSocket per client, carrying change notices only.

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use hyakume_core::api::Event;
use hyakume_core::model::Identity;
use tokio::sync::broadcast::error::RecvError;

use crate::auth::Caller;
use crate::{AppState, Notice};

pub async fn subscribe(
    State(app): State<AppState>,
    Caller(me): Caller,
    upgrade: WebSocketUpgrade,
) -> Response {
    upgrade.on_upgrade(move |socket| stream(app, me, socket))
}

/// Subscribes to the broadcast before sending `Subscribed`, so every change
/// committed after the client's next fetch reaches this socket.
async fn stream(app: AppState, me: Identity, mut socket: WebSocket) {
    let mut notices = app.events.subscribe();
    let mut shutdown = app.shutdown.clone();
    if send(&mut socket, &Event::Subscribed).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            notice = notices.recv() => {
                let event = match notice {
                    Ok(notice) => match visible(&app, &me, &notice).await {
                        Ok(true) => notice.event,
                        Ok(false) => continue,
                        Err(err) => {
                            tracing::error!(error = %err, "checking event visibility");
                            continue;
                        }
                    },
                    Err(RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, identity = %me.name, "event subscriber lagged");
                        Event::Resync
                    }
                    Err(RecvError::Closed) => break,
                };
                if send(&mut socket, &event).await.is_err() {
                    break;
                }
            }
            incoming = socket.recv() => match incoming {
                // Clients send nothing meaningful; pings are answered by axum.
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
            _ = shutdown.changed() => break,
        }
    }
    let _ = socket.send(Message::Close(None)).await;
}

/// Checks access at delivery time, so role changes apply to open sockets.
async fn visible(
    app: &AppState,
    me: &Identity,
    notice: &Notice,
) -> Result<bool, hyakume_storage::StoreError> {
    if let Some(owner) = notice.only {
        return Ok(owner == me.id);
    }
    let Some(access) = app.store.access(notice.workspace, me.id).await? else {
        return Ok(false);
    };
    Ok(notice.project.is_none_or(|p| access.can_view_project(p)))
}

async fn send(socket: &mut WebSocket, event: &Event) -> Result<(), axum::Error> {
    let text = serde_json::to_string(event).expect("events always serialize");
    socket.send(Message::Text(text.into())).await
}
