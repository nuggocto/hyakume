//! Saved session tokens, one per server URL and identity. The OS keyring is
//! the default, and a keyring failure is reported, never silently bypassed.
//! Plaintext dev files need `credential_store = "dev-file"` and a dev-mode
//! server. These calls block on D-Bus, so keep them off UI threads.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use keyring_core::{Entry, Error as KeyringError};

use crate::config::CredentialStore;
use crate::{Client, ClientError};

const SERVICE: &str = "hyakume";

pub fn save(
    client: &Client,
    server_url: &str,
    identity: &str,
    token: &str,
    server_dev_mode: bool,
) -> Result<(), ClientError> {
    match client.config.credential_store {
        CredentialStore::Keyring => entry(server_url, identity)?
            .set_password(token)
            .map_err(|e| keyring_failure(client, &e)),
        CredentialStore::DevFile => {
            if !server_dev_mode {
                return Err(ClientError::Credentials(format!(
                    "credential_store = \"dev-file\" is only accepted for dev-mode servers, and \
                     {server_url} is not one. Use the OS keyring for real servers."
                )));
            }
            write_dev_file(&dev_file(client, server_url, identity), token)
        }
    }
}

pub fn load(
    client: &Client,
    server_url: &str,
    identity: &str,
) -> Result<Option<String>, ClientError> {
    match client.config.credential_store {
        CredentialStore::Keyring => match entry(server_url, identity)?.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(KeyringError::NoEntry) => Ok(None),
            Err(e) => Err(keyring_failure(client, &e)),
        },
        CredentialStore::DevFile => {
            match std::fs::read_to_string(dev_file(client, server_url, identity)) {
                Ok(token) => Ok(Some(token.trim().to_owned())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(ClientError::Credentials(e.to_string())),
            }
        }
    }
}

pub fn delete(client: &Client, server_url: &str, identity: &str) -> Result<(), ClientError> {
    match client.config.credential_store {
        CredentialStore::Keyring => match entry(server_url, identity)?.delete_credential() {
            Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
            Err(e) => Err(keyring_failure(client, &e)),
        },
        CredentialStore::DevFile => {
            match std::fs::remove_file(dev_file(client, server_url, identity)) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(ClientError::Credentials(e.to_string())),
            }
        }
    }
}

/// Connects to the Secret Service once per process.
fn entry(server_url: &str, identity: &str) -> Result<Entry, ClientError> {
    static STORE: OnceLock<Result<(), String>> = OnceLock::new();
    let ready = STORE.get_or_init(|| {
        zbus_secret_service_keyring_store::Store::new()
            .map(|store| keyring_core::set_default_store(store as Arc<_>))
            .map_err(|e| e.to_string())
    });
    if let Err(reason) = ready {
        return Err(ClientError::Credentials(format!(
            "the OS keyring is unavailable ({reason}). Start a Secret Service provider such as \
             gnome-keyring or KeePassXC. For development servers only, you can instead set \
             credential_store = \"dev-file\" in client.toml to store tokens in plaintext files."
        )));
    }
    Entry::new(SERVICE, &format!("{server_url}#{identity}"))
        .map_err(|e| ClientError::Credentials(e.to_string()))
}

fn keyring_failure(client: &Client, err: &KeyringError) -> ClientError {
    ClientError::Credentials(format!(
        "the OS keyring refused the request ({err}). It may be locked. For development servers \
         only, you can set credential_store = \"dev-file\" in {}.",
        client.data.config_file().display()
    ))
}

/// `credentials/<hex(url)>/<hex(identity)>.token`. Hex keeps every byte, so
/// two names can't land on the same file. Swapping odd characters for `_`
/// made `alice.one` and `alice_one` share a token.
fn dev_file(client: &Client, server_url: &str, identity: &str) -> PathBuf {
    client
        .data
        .credentials_dir()
        .join(hex(server_url))
        .join(format!("{}.token", hex(identity)))
}

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

fn write_dev_file(path: &std::path::Path, token: &str) -> Result<(), ClientError> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    let fail = |e: std::io::Error| ClientError::Credentials(format!("{}: {e}", path.display()));
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(fail)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(fail)?;
    file.write_all(token.as_bytes()).map_err(fail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DataDir;
    use crate::config::ClientConfig;

    #[test]
    fn lookalike_servers_and_identities_keep_separate_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let client = Client {
            data: DataDir::at(dir.path()),
            config: ClientConfig {
                credential_store: CredentialStore::DevFile,
                ..Default::default()
            },
        };
        let pairs = [
            ("http://a.b:1", "alice.one", "t1"),
            ("http://a.b:1", "alice_one", "t2"),
            ("http://a_b:1", "alice.one", "t3"),
        ];
        for (url, identity, token) in pairs {
            save(&client, url, identity, token, true).unwrap();
        }
        for (url, identity, token) in pairs {
            assert_eq!(
                load(&client, url, identity).unwrap().as_deref(),
                Some(token)
            );
        }
    }
}
