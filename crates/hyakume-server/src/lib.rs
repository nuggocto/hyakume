//! `hyakume-server`: the authoritative store for shared workspaces.

mod auth;
pub mod config;
mod error;
mod events;
mod routes;
mod seed;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::http::StatusCode;
use hyakume_core::api::Event;
use hyakume_core::ids::{IdentityId, ProjectId, WorkspaceId};
use hyakume_storage::Store;
use tokio::sync::{broadcast, watch};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use crate::config::Config;

/// A committed change, fanned out to WebSocket connections that may see it.
#[derive(Clone, Debug)]
pub struct Notice {
    pub workspace: WorkspaceId,
    /// Set when only viewers of this project should hear about it.
    pub project: Option<ProjectId>,
    /// Set for personal data, such as layouts, that only its owner may see.
    pub only: Option<IdentityId>,
    pub event: Event,
}

#[derive(Clone)]
pub struct AppState {
    store: Store,
    events: broadcast::Sender<Notice>,
    /// Identity names that may use dev login. Empty unless dev mode is on.
    dev_identities: Arc<HashSet<String>>,
    session_idle_ms: i64,
    shutdown: watch::Receiver<bool>,
}

impl AppState {
    /// Returns the state and the sender that tells open WebSockets to close.
    pub fn new(store: Store, config: &Config) -> (Self, watch::Sender<bool>) {
        let (shutdown_tx, shutdown) = watch::channel(false);
        let dev_identities = if config.dev_mode {
            config
                .dev
                .identities
                .iter()
                .map(|i| i.name.clone())
                .collect()
        } else {
            HashSet::new()
        };
        // Slow sockets that fall this far behind get a Resync instead.
        let (events, _) = broadcast::channel(1024);
        let state = Self {
            store,
            events,
            dev_identities: Arc::new(dev_identities),
            session_idle_ms: config.session_idle_ms(),
            shutdown,
        };
        (state, shutdown_tx)
    }

    fn dev_mode(&self) -> bool {
        !self.dev_identities.is_empty()
    }

    /// Call only after the change is committed, so a client that refetches in
    /// response always sees it.
    fn notify(&self, notice: Notice) {
        // An error only means nobody is subscribed right now.
        let _ = self.events.send(notice);
    }
}

pub fn router(state: AppState) -> Router {
    routes::v1()
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(30),
        ))
        .layer(RequestBodyLimitLayer::new(1024 * 1024))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Opens and migrates the database, then seeds dev data when dev mode is on.
/// Migrations finish before any listener exists, so a failed upgrade can
/// never serve requests against the wrong schema.
pub async fn prepare(config: &Config) -> anyhow::Result<Store> {
    if let Some(dir) = config.database.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating database directory {}", dir.display()))?;
    }
    let store = Store::open(&config.database)
        .await
        .with_context(|| format!("opening database {}", config.database.display()))?;
    if config.dev_mode {
        seed::apply(&store, &config.dev)
            .await
            .context("seeding dev data")?;
    }
    Ok(store)
}

/// Prepares the database and serves until SIGINT or SIGTERM.
pub async fn run(config: Config) -> anyhow::Result<()> {
    let store = prepare(&config).await?;
    if config.dev_mode && !config.bind.ip().is_loopback() {
        tracing::warn!(
            bind = %config.bind,
            "dev mode is reachable from other machines; anyone who can reach this port can sign in as a test identity"
        );
    }

    let (state, shutdown_tx) = AppState::new(store.clone(), &config);
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("binding {}", config.bind))?;
    tracing::info!(addr = %config.bind, dev_mode = config.dev_mode, "listening");
    serve(listener, state, async move {
        shutdown_signal().await;
        tracing::info!("shutting down");
        let _ = shutdown_tx.send(true);
    })
    .await?;
    store.close().await;
    Ok(())
}

/// Serves until `shutdown` completes. Send `true` on the state's shutdown
/// channel as part of it, so open WebSockets close and the drain can finish.
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: AppState,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
        .context("serving")
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!(%err, "cannot listen for Ctrl+C");
            std::future::pending::<()>().await;
        }
    };
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(err) => {
                tracing::error!(%err, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
