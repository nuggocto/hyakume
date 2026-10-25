use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::ClientError;

/// `client.toml`. Holds no secrets; session tokens live in the OS keyring
/// or, by explicit choice, the development credential file.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    /// Selector used when `--workspace` is not given.
    pub default_workspace: Option<String>,
    #[serde(default)]
    pub credential_store: CredentialStore,
    #[serde(default, rename = "server")]
    pub servers: Vec<ServerEntry>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialStore {
    #[default]
    Keyring,
    /// Plaintext token files, accepted only for servers running in dev mode.
    DevFile,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerEntry {
    pub name: String,
    pub url: String,
    /// The identity signed in on this server, if any.
    pub identity: Option<String>,
}

impl ClientConfig {
    pub fn load(path: &Path) -> Result<Self, ClientError> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| ClientError::Config(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ClientError::Config(format!("{}: {e}", path.display()))),
        }
    }

    /// Writes through a temporary file and rename, so a crash never leaves a
    /// half-written config.
    pub fn save(&self, path: &Path) -> Result<(), ClientError> {
        let fail =
            |e: &dyn std::fmt::Display| ClientError::Config(format!("{}: {e}", path.display()));
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| fail(&e))?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| fail(&e))?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(|e| fail(&e))?;
        std::fs::rename(&tmp, path).map_err(|e| fail(&e))
    }

    pub fn server(&self, name: &str) -> Result<&ServerEntry, ClientError> {
        self.servers
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| ClientError::NotFound(format!("server {name:?}")))
    }

    pub fn server_mut(&mut self, name: &str) -> Result<&mut ServerEntry, ClientError> {
        self.servers
            .iter_mut()
            .find(|s| s.name == name)
            .ok_or_else(|| ClientError::NotFound(format!("server {name:?}")))
    }
}
