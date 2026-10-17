//! Init, unlock, lock, and reset. Each runs as a service task under the
//! transition lock, so a client that leaves cannot cut a drain short.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use rand_core::{OsRng, RngCore};
use tokio::sync::mpsc;

use super::clients::{Input, route_prompts, secret_input};
use super::{
    Phase, Service, delete_key, key_not_deleted, lock, node_failure, secret_failure, state_failure,
};
use crate::identity::DeviceKey;
use crate::ipc::{Body, ErrorCode, Failure};
use crate::limits::Limits;
use crate::node::{Node, NodeConfig, NodeError};
use crate::secrets::{self, Availability, KeyBundle, MIN_PASSPHRASE_BYTES, SecretError};
use crate::storage::{Installation, StoreKind};
use crate::tasks;
use crate::trust::LocalAlias;

impl Service {
    pub(super) async fn init(
        self: &Arc<Self>,
        store: StoreKind,
        name: String,
        inputs: &mut mpsc::Receiver<Input>,
    ) -> Result<Body, Failure> {
        let _t = self.transition().await?;
        if !matches!(&*lock(&self.phase), Phase::Uninitialized) {
            return Err(Failure::new(
                ErrorCode::AlreadyInitialized,
                "this device already has an identity; hyakume reset forgets it",
            ));
        }

        let earlier = self
            .cell
            .change(|s| s.store().installation())
            .map_err(|e| Failure::new(ErrorCode::Storage, e.to_string()))?;
        if earlier.as_ref().is_some_and(|i| i.ready) {
            return Err(Failure::new(
                ErrorCode::AlreadyInitialized,
                "this device already has an identity; hyakume reset forgets it",
            ));
        }

        // Only an initialization that never finished is undone.
        if let Some(old) = earlier {
            delete_key(&self.config.locations, &old)
                .await
                .map_err(|e| key_not_deleted(&e))?;
            self.cell
                .change(|s| s.forget_all())
                .map_err(state_failure)?;
        }

        let name = LocalAlias::new(&name).map_err(|_| {
            Failure::new(
                ErrorCode::Invalid,
                "a device name must be 1 to 64 characters without control characters",
            )
        })?;

        let passphrase = match store {
            StoreKind::KeyFile => {
                let pass = secret_input(inputs).await?;
                if pass.len() < MIN_PASSPHRASE_BYTES {
                    return Err(secret_failure(SecretError::WeakPassphrase));
                }
                Some(pass)
            }
            StoreKind::SecretService => {
                if secrets::probe().await != Availability::Unlocked {
                    return Err(Failure::new(
                        ErrorCode::StoreUnavailable,
                        "the Secret Service is unavailable or locked; choose the key file instead",
                    ));
                }
                None
            }
        };

        let mut id = [0u8; 16];
        OsRng.fill_bytes(&mut id);
        let key = DeviceKey::generate(&mut OsRng);
        let installation = Installation {
            id,
            device: key.id(),
            store,
            name: name.as_str().to_owned(),
            ready: false,
        };

        self.cell
            .change(|s| s.store().begin_installation(&installation))
            .map_err(|e| Failure::new(ErrorCode::Storage, e.to_string()))?;

        let bundle = KeyBundle {
            installation: id,
            seed: key.seed(),
        };
        drop(key);

        let written = match (store, passphrase) {
            (StoreKind::KeyFile, Some(pass)) => {
                let path = self.config.locations.key_file();
                let seed = bundle.seed.clone();
                self.blocking(move || {
                    let bundle = KeyBundle {
                        installation: id,
                        seed,
                    };
                    secrets::write_key_file(&path, &bundle, &pass)
                })
                .await
                .unwrap_or_else(|_| {
                    Err(SecretError::Io(std::io::Error::other(
                        "the key writer stopped",
                    )))
                })
            }
            _ => secrets::store(&bundle).await,
        };

        if let Err(e) = written {
            // Undo what was written. If that fails too, the unfinished
            // record stays so the next init or start can finish the job.
            if delete_key(&self.config.locations, &installation)
                .await
                .is_ok()
            {
                let _ = self.cell.change(|s| s.forget_all());
            }
            return Err(secret_failure(e));
        }

        self.cell
            .change(|s| s.store().finish_installation())
            .map_err(|e| Failure::new(ErrorCode::Storage, e.to_string()))?;
        let installation = Installation {
            ready: true,
            ..installation
        };

        // The identity is saved. If the endpoint cannot start, the service
        // keeps it, locked, and unlock tries again.
        if let Err(failure) = self.start(installation.clone(), bundle).await {
            *lock(&self.phase) = Phase::Locked(installation);
            return Err(Failure::new(
                failure.code,
                format!(
                    "{}; the identity is saved, run hyakume unlock to try again",
                    failure.message
                ),
            ));
        }
        Ok(Body::Status(self.status()))
    }

    pub(super) async fn unlock(
        self: &Arc<Self>,
        inputs: &mut mpsc::Receiver<Input>,
    ) -> Result<Body, Failure> {
        let _t = self.transition().await?;

        // The phase lock is released before anything below locks it again.
        let locked = match &*lock(&self.phase) {
            Phase::Locked(i) => Some(i.clone()),
            Phase::Unlocked { .. } | Phase::Uninitialized => None,
        };
        let Some(installation) = locked else {
            self.with_node(|_| ())?;
            return Ok(Body::Status(self.status()));
        };

        let bundle = match installation.store {
            StoreKind::KeyFile => {
                let pass = secret_input(inputs).await?;
                let path = self.config.locations.key_file();
                let id = installation.id;
                self.blocking(move || secrets::read_key_file(&path, &pass, &id))
                    .await?
            }
            StoreKind::SecretService => secrets::load(&installation.id).await,
        }
        .map_err(secret_failure)?;

        self.start(installation, bundle).await?;
        Ok(Body::Status(self.status()))
    }

    /// Starts the node with the unlocked key. The key must be the one this
    /// installation recorded; a different key is refused, never adopted.
    pub(super) async fn start(
        self: &Arc<Self>,
        installation: Installation,
        bundle: KeyBundle,
    ) -> Result<(), Failure> {
        let key = DeviceKey::from_seed(&bundle.seed);
        if key.id() != installation.device {
            return Err(Failure::new(
                ErrorCode::StoreUnavailable,
                "the stored key is not this installation's device key; hyakume reset starts over",
            ));
        }

        let config = NodeConfig {
            name: installation.name.clone(),
            limits: Limits::COMPILED,
            bind: self.config.listen,
            read_gate: None,
            state: Some(self.cell.clone()),
        };

        let (node, prompts) = Node::bind(config, key).await.map_err(node_failure)?;
        let node = Arc::new(node);
        let router = tokio::spawn(route_prompts(self.clone(), prompts));

        // The receiver cannot change until the node is in place, so the
        // node never keeps the destination of a receiver that left.
        let routes = lock(&self.routes);
        if let Some((_, _, dest)) = &routes.receiver {
            node.set_destination(Some(dest.clone()));
        }
        *lock(&self.phase) = Phase::Unlocked {
            installation,
            node,
            router,
        };
        drop(routes);
        Ok(())
    }

    pub(super) async fn lock(self: &Arc<Self>) -> Result<Body, Failure> {
        let _t = self.transition().await?;
        self.stop_node_or_service().await?;
        Ok(Body::Status(self.status()))
    }

    /// Stops the node for lock or reset. If its work does not stop in
    /// time, the whole service stops: a new node must not share the state
    /// with work that still runs, and the next start recovers it.
    pub(super) async fn stop_node_or_service(&self) -> Result<(), Failure> {
        self.stop_node().await.map_err(|e| {
            // New requests are refused from now on, before the stop begins.
            self.stopping.store(true, Ordering::SeqCst);
            self.drain_failed.store(true, Ordering::SeqCst);
            self.stop.notify_one();
            Failure::new(ErrorCode::Stopping, format!("{e}; the service is stopping"))
        })
    }

    /// Stops the node, if running, pausing its work. The key goes with it.
    pub(super) async fn stop_node(&self) -> Result<(), NodeError> {
        let running = {
            let mut phase = lock(&self.phase);
            match std::mem::replace(&mut *phase, Phase::Uninitialized) {
                Phase::Unlocked {
                    installation,
                    node,
                    router,
                } => {
                    *phase = Phase::Locked(installation);
                    Some((node, router))
                }
                other => {
                    *phase = other;
                    None
                }
            }
        };

        let Some((node, router)) = running else {
            return Ok(());
        };

        let stopped = node.shutdown().await;
        router.abort();
        tasks::observe(router.await);
        stopped
    }

    pub(super) async fn reset(self: &Arc<Self>) -> Result<Body, Failure> {
        let _t = self.transition().await?;
        self.stop_node_or_service().await?;

        let installation = self
            .cell
            .change(|s| s.store().installation())
            .map_err(|e| Failure::new(ErrorCode::Storage, e.to_string()))?;
        if let Some(i) = installation {
            // The old key must be gone before the identity is forgotten:
            // the installation record is what finds the key to delete.
            delete_key(&self.config.locations, &i)
                .await
                .map_err(|e| key_not_deleted(&e))?;
            self.cell
                .change(|s| s.forget_all())
                .map_err(state_failure)?;
        }

        *lock(&self.phase) = Phase::Uninitialized;
        Ok(Body::Status(self.status()))
    }
}
