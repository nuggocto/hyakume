//! Client-side access to local personal work and to shared workspaces on
//! self-hosted servers. The CLI and desktop app both go through this crate.

pub mod config;
pub mod credentials;
mod local;
pub mod remote;
mod workspace;

use std::path::{Path, PathBuf};

use hyakume_core::api::{ApiError, ErrorCode};
use hyakume_core::model::{Task, Workspace};
use hyakume_storage::{Store, StoreError};

use crate::config::{ClientConfig, CredentialStore, ServerEntry};
use crate::remote::Remote;

pub use workspace::OpenWorkspace;

/// Selector for the local personal workspace.
pub const LOCAL: &str = "local";

/// Where this client profile keeps its files. `HYAKUME_DATA_DIR` overrides
/// the default, so several profiles (Alice and Bob) can share one machine.
#[derive(Clone, Debug)]
pub struct DataDir(PathBuf);

impl DataDir {
    pub fn resolve() -> Result<Self, ClientError> {
        if let Some(dir) = std::env::var_os("HYAKUME_DATA_DIR") {
            return Ok(Self(dir.into()));
        }
        let base = directories::BaseDirs::new().ok_or_else(|| {
            ClientError::Config("cannot find a home directory; set HYAKUME_DATA_DIR".into())
        })?;
        Ok(Self(base.data_dir().join("hyakume")))
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn config_file(&self) -> PathBuf {
        self.0.join("client.toml")
    }

    pub fn local_db(&self) -> PathBuf {
        self.0.join("local.db")
    }

    /// Copies of shared workspaces for showing them offline. Never local work.
    pub fn cache_db(&self) -> PathBuf {
        self.0.join("cache.db")
    }

    pub fn credentials_dir(&self) -> PathBuf {
        self.0.join("credentials")
    }
}

/// A client profile: its data directory and saved configuration.
pub struct Client {
    pub data: DataDir,
    pub config: ClientConfig,
}

/// One workspace as listed across every source.
pub struct Listing {
    /// What to pass to `--workspace`: `local` or `SERVER/SLUG`.
    pub selector: String,
    pub workspace: Workspace,
}

impl Client {
    pub fn load(data: DataDir) -> Result<Self, ClientError> {
        let config = ClientConfig::load(&data.config_file())?;
        Ok(Self { data, config })
    }

    pub fn save_config(&self) -> Result<(), ClientError> {
        self.config.save(&self.data.config_file())
    }

    pub async fn open_local(&self) -> Result<OpenWorkspace, ClientError> {
        std::fs::create_dir_all(self.data.path()).map_err(|e| {
            ClientError::Config(format!("creating {}: {e}", self.data.path().display()))
        })?;
        let store = Store::open(&self.data.local_db()).await?;
        local::open(store).await
    }

    /// A client for a configured server, with its saved session if there is one.
    /// A plaintext dev-file session is used only while the server still
    /// reports dev mode, as the dev-file store promises.
    pub async fn server(&self, name: &str) -> Result<Remote, ClientError> {
        let entry = self.config.server(name)?;
        let token = match &entry.identity {
            Some(identity) => credentials::load(self, &entry.url, identity)?,
            None => None,
        };
        let uses_dev_file = self.config.credential_store == CredentialStore::DevFile;
        if uses_dev_file
            && token.is_some()
            && !Remote::new(&entry.url, None)?.info().await?.dev_mode
        {
            return Err(ClientError::Credentials(format!(
                "{name} is no longer in dev mode, so its saved dev-file session is not used. \
                 Switch credential_store to \"keyring\" and sign in again."
            )));
        }
        Remote::new(&entry.url, token)
    }

    /// Opens a workspace by selector, or the saved default, or local work.
    pub async fn open(&self, selector: Option<&str>) -> Result<OpenWorkspace, ClientError> {
        let selector = selector
            .or(self.config.default_workspace.as_deref())
            .unwrap_or(LOCAL);
        if selector == LOCAL {
            return self.open_local().await;
        }
        let (server, slug) = selector.split_once('/').ok_or_else(|| {
            ClientError::Invalid(format!(
                "workspace {selector:?} must be `local` or SERVER/WORKSPACE"
            ))
        })?;
        let remote = self.server(server).await?;
        let listing = remote
            .workspaces()
            .await?
            .into_iter()
            .find(|w| w.workspace.slug == slug)
            .ok_or_else(|| ClientError::NotFound(format!("workspace {selector}")))?;
        Ok(OpenWorkspace::remote(
            selector.to_owned(),
            listing.workspace,
            remote,
        ))
    }

    /// Every workspace across local work and configured servers. A server
    /// that cannot be reached is reported next to the others, not as a failure.
    pub async fn list(&self) -> (Vec<Listing>, Vec<(String, ClientError)>) {
        let mut listings = Vec::new();
        let mut failures = Vec::new();
        match self.open_local().await {
            Ok(local) => listings.push(Listing {
                selector: LOCAL.into(),
                workspace: local.workspace.clone(),
            }),
            Err(err) => failures.push((LOCAL.to_owned(), err)),
        }
        for ServerEntry { name, .. } in &self.config.servers {
            let result = match self.server(name).await {
                Ok(remote) => remote.workspaces().await,
                Err(err) => Err(err),
            };
            match result {
                Ok(found) => listings.extend(found.into_iter().map(|w| Listing {
                    selector: format!("{name}/{}", w.workspace.slug),
                    workspace: w.workspace,
                })),
                Err(err) => failures.push((name.clone(), err)),
            }
        }
        (listings, failures)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("{0} not found")]
    NotFound(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("{message}")]
    Conflict { message: String, latest: Box<Task> },
    #[error("{0}")]
    Duplicate(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Unauthorized(String),
    #[error("cannot reach {server}: {reason}")]
    Unreachable { server: String, reason: String },
    #[error("credential storage: {0}")]
    Credentials(String),
    #[error("configuration: {0}")]
    Config(String),
    #[error("{0}")]
    Internal(String),
}

impl From<ApiError> for ClientError {
    fn from(err: ApiError) -> Self {
        match err.code {
            ErrorCode::NotFound => Self::NotFound(
                err.message
                    .strip_suffix(" not found")
                    .unwrap_or(&err.message)
                    .to_owned(),
            ),
            ErrorCode::Forbidden => Self::Forbidden(err.message),
            ErrorCode::Conflict => match err.latest {
                Some(latest) => Self::Conflict {
                    message: err.message,
                    latest,
                },
                None => Self::Internal(err.message),
            },
            ErrorCode::Duplicate => Self::Duplicate(err.message),
            ErrorCode::Invalid => Self::Invalid(err.message),
            ErrorCode::Unauthorized => Self::Unauthorized(err.message),
            ErrorCode::Internal => Self::Internal(err.message),
        }
    }
}

impl From<StoreError> for ClientError {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::NotFound(what) => Self::NotFound(what.to_owned()),
            StoreError::Conflict { expected, latest } => Self::Conflict {
                message: format!(
                    "the task changed since version {expected}; latest is version {}",
                    latest.version
                ),
                latest,
            },
            StoreError::Duplicate(message) => Self::Duplicate(message),
            StoreError::Invalid(err) => Self::Invalid(err.to_string()),
            other => Self::Internal(other.to_string()),
        }
    }
}
