//! The two Linux stores for the device key: the Secret Service, or an age
//! file under a passphrase. Both hold a [`KeyBundle`] naming its
//! installation, so a key from elsewhere is never used. Hyakume never runs
//! a Secret Service prompt, to unlock or to confirm: one can open a window
//! nobody on SSH can answer. Every call has [`SECRET_SERVICE_TIMEOUT`].

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

use aes::cipher::block_padding::Pkcs7;
use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use age::secrecy::SecretString;
use crypto_bigint::modular::runtime_mod::{DynResidue, DynResidueParams};
use crypto_bigint::{Encoding, U1024};
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use rustix::fs::{Mode, OFlags};
use sha2::Sha256;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zeroize::{Zeroize, Zeroizing};

/// How long one Secret Service call may take.
pub const SECRET_SERVICE_TIMEOUT: Duration = Duration::from_secs(3);
/// scrypt work factor of the key file: N = 2^17, 128 MiB for about half a
/// second, which stays inside the service's memory budget.
pub const KEY_FILE_WORK_FACTOR: u8 = 17;
/// Longest passphrase or other credential read from a client.
pub const MAX_CREDENTIAL_BYTES: usize = 4096;
/// Shortest passphrase accepted for a new key file.
pub const MIN_PASSPHRASE_BYTES: usize = 8;
/// Largest key file read; a real one is a few hundred bytes.
const MAX_KEY_FILE_BYTES: u64 = 16 * 1024;

const BUNDLE_MAGIC: &[u8; 12] = b"hyakume-key\x01";
const BUNDLE_BYTES: usize = 12 + 16 + 32;
const APPLICATION: &str = "hyakume";

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("the Secret Service is not available: {0}")]
    Unavailable(String),
    #[error("the Secret Service is locked")]
    Locked,
    #[error("the Secret Service did not answer in time")]
    Timeout,
    #[error(
        "the Secret Service wants someone to confirm this, and Hyakume never shows its prompts; allow it without confirmation or use the key file"
    )]
    Confirmation,
    #[error("the passphrase is wrong")]
    WrongPassphrase,
    #[error("a passphrase must be UTF-8 and at least {MIN_PASSPHRASE_BYTES} bytes")]
    WeakPassphrase,
    #[error("no key is stored for this installation")]
    Missing,
    #[error("the stored key is damaged or belongs to another installation: {0}")]
    Corrupt(&'static str),
    #[error("{0}")]
    Io(#[from] io::Error),
}

/// The device key's seed and the installation it belongs to.
pub struct KeyBundle {
    pub installation: [u8; 16],
    pub seed: Zeroizing<[u8; 32]>,
}

impl KeyBundle {
    fn encode(&self) -> Zeroizing<Vec<u8>> {
        let mut out = Zeroizing::new(Vec::with_capacity(BUNDLE_BYTES));
        out.extend_from_slice(BUNDLE_MAGIC);
        out.extend_from_slice(&self.installation);
        out.extend_from_slice(&*self.seed);
        out
    }

    /// Accepts exactly one complete bundle for `installation`.
    fn decode(bytes: &[u8], installation: &[u8; 16]) -> Result<Self, SecretError> {
        if bytes.len() != BUNDLE_BYTES || !bytes.starts_with(BUNDLE_MAGIC) {
            return Err(SecretError::Corrupt("not a Hyakume key"));
        }
        let (id, seed) = bytes[BUNDLE_MAGIC.len()..].split_at(16);
        if id != installation {
            return Err(SecretError::Corrupt("another installation's key"));
        }
        let mut out = Zeroizing::new([0u8; 32]);
        out.copy_from_slice(seed);
        Ok(Self {
            installation: *installation,
            seed: out,
        })
    }
}

impl std::fmt::Debug for KeyBundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeyBundle([redacted])")
    }
}

fn passphrase(bytes: &[u8]) -> Result<SecretString, SecretError> {
    let text = std::str::from_utf8(bytes).map_err(|_| SecretError::WeakPassphrase)?;
    Ok(SecretString::from(text.to_owned()))
}

// ---- Key file ----

/// Writes `bundle` to `path` encrypted with `passphrase`, replacing the file
/// atomically. Blocking: scrypt takes most of a second.
pub fn write_key_file(path: &Path, bundle: &KeyBundle, pass: &[u8]) -> Result<(), SecretError> {
    if pass.len() < MIN_PASSPHRASE_BYTES {
        return Err(SecretError::WeakPassphrase);
    }
    let mut recipient = age::scrypt::Recipient::new(passphrase(pass)?);
    recipient.set_work_factor(KEY_FILE_WORK_FACTOR);
    let encryptor = age::Encryptor::with_recipients(std::iter::once(&recipient as _))
        .map_err(|e| io::Error::other(e.to_string()))?;

    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("the key file has no directory"))?;
    let tmp = path.with_extension("age.new");
    // A leftover from an interrupted write is never a usable key.
    let _ = std::fs::remove_file(&tmp);

    let fd = rustix::fs::open(
        &tmp,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(io::Error::from)?;
    let mut file = File::from(fd);
    let mut writer = encryptor.wrap_output(&mut file)?;
    writer.write_all(&bundle.encode())?;
    writer.finish()?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// Reads and decrypts the key file. Blocking.
pub fn read_key_file(
    path: &Path,
    pass: &[u8],
    installation: &[u8; 16],
) -> Result<KeyBundle, SecretError> {
    let fd = match rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Err(SecretError::Missing),
        Err(e) => return Err(io::Error::from(e).into()),
    };
    let mut encrypted = Vec::new();
    File::from(fd)
        .take(MAX_KEY_FILE_BYTES + 1)
        .read_to_end(&mut encrypted)?;
    if encrypted.len() as u64 > MAX_KEY_FILE_BYTES {
        return Err(SecretError::Corrupt("the key file is too large"));
    }
    let mut identity = age::scrypt::Identity::new(passphrase(pass)?);
    identity.set_max_work_factor(KEY_FILE_WORK_FACTOR);
    let decryptor = age::Decryptor::new(encrypted.as_slice()).map_err(decrypt_error)?;
    let mut reader = decryptor
        .decrypt(std::iter::once(&identity as _))
        .map_err(decrypt_error)?;
    let mut plain = Zeroizing::new(Vec::with_capacity(BUNDLE_BYTES));
    // A complete bundle is a fixed size; anything longer is refused. Reading
    // to the end checks the last chunk's authentication tag.
    (&mut reader)
        .take(BUNDLE_BYTES as u64 + 1)
        .read_to_end(&mut plain)
        .map_err(|_| SecretError::Corrupt("the key file does not decrypt completely"))?;
    KeyBundle::decode(&plain, installation)
}

fn decrypt_error(e: age::DecryptError) -> SecretError {
    match e {
        age::DecryptError::DecryptionFailed
        | age::DecryptError::NoMatchingKeys
        | age::DecryptError::KeyDecryptionFailed => SecretError::WrongPassphrase,
        age::DecryptError::ExcessiveWork { .. } => {
            SecretError::Corrupt("the key file asks for more work than allowed")
        }
        age::DecryptError::Io(e) => SecretError::Io(e),
        _ => SecretError::Corrupt("not an age key file"),
    }
}

/// Removes the key file, if any. Blocking.
pub fn delete_key_file(path: &Path) -> Result<(), SecretError> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    if let Some(dir) = path.parent() {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

// ---- Secret Service ----

const SERVICE: &str = "org.freedesktop.secrets";
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SERVICE_INTERFACE: &str = "org.freedesktop.Secret.Service";
const COLLECTION_INTERFACE: &str = "org.freedesktop.Secret.Collection";
const ITEM_INTERFACE: &str = "org.freedesktop.Secret.Item";
const PROMPT_INTERFACE: &str = "org.freedesktop.Secret.Prompt";
const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";
const IS_LOCKED: &str = "org.freedesktop.Secret.Error.IsLocked";
const ALGORITHM: &str = "dh-ietf1024-sha256-aes128-cbc-pkcs7";
/// The 1024-bit prime of RFC 2409's second Oakley group, which the
/// Secret Service's key exchange uses with generator 2.
const MODP_1024: U1024 = U1024::from_be_hex(concat!(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74",
    "020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F1437",
    "4FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED",
    "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE65381FFFFFFFFFFFFFFFF",
));

/// A secret as the Secret Service sends it: its session, the cipher's
/// IV, the encrypted value, and its content type.
type Secret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

/// What the Secret Service can do right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    Unlocked,
    Locked,
    Unavailable,
}

async fn bounded<T>(call: impl Future<Output = zbus::Result<T>>) -> Result<T, SecretError> {
    match tokio::time::timeout(SECRET_SERVICE_TIMEOUT, call).await {
        Err(_) => Err(SecretError::Timeout),
        Ok(Err(zbus::Error::MethodError(name, ..))) if name.as_str() == IS_LOCKED => {
            Err(SecretError::Locked)
        }
        Ok(Err(e)) => Err(SecretError::Unavailable(e.to_string())),
        Ok(Ok(v)) => Ok(v),
    }
}

fn unusable(what: &str) -> SecretError {
    SecretError::Unavailable(format!("the Secret Service sent {what}"))
}

/// One Secret Service connection with an encrypted session. It calls the
/// D-Bus methods itself and never runs a prompt: a reply that needs one
/// is refused and its prompt dismissed unseen.
struct Keyring {
    conn: zbus::Connection,
    session: OwnedObjectPath,
    key: Zeroizing<[u8; 16]>,
}

impl Keyring {
    async fn connect() -> Result<Self, SecretError> {
        Self::open(bounded(zbus::Connection::session()).await?).await
    }

    async fn open(conn: zbus::Connection) -> Result<Self, SecretError> {
        let ours = Exchange::new();
        let input = Value::from(ours.public());
        let (output, session): (OwnedValue, OwnedObjectPath) = call(
            &conn,
            SERVICE_PATH,
            SERVICE_INTERFACE,
            "OpenSession",
            &(ALGORITHM, input),
        )
        .await?;
        let theirs =
            Vec::<u8>::try_from(output).map_err(|_| unusable("a malformed session key"))?;
        let key = ours
            .key(&theirs)
            .ok_or_else(|| unusable("an unusable session key"))?;
        Ok(Self { conn, session, key })
    }

    async fn call<B, R>(
        &self,
        path: &str,
        interface: &str,
        method: &str,
        body: &B,
    ) -> Result<R, SecretError>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
        R: zbus::zvariant::Type + serde::de::DeserializeOwned,
    {
        call(&self.conn, path, interface, method, body).await
    }

    async fn default_collection(&self) -> Result<OwnedObjectPath, SecretError> {
        let path: OwnedObjectPath = self
            .call(SERVICE_PATH, SERVICE_INTERFACE, "ReadAlias", &("default",))
            .await?;
        if path.as_str() == "/" {
            return Err(SecretError::Unavailable(
                "the Secret Service has no default collection".to_owned(),
            ));
        }
        Ok(path)
    }

    async fn is_locked(&self, collection: &str) -> Result<bool, SecretError> {
        let locked: OwnedValue = self
            .call(
                collection,
                PROPERTIES_INTERFACE,
                "Get",
                &(COLLECTION_INTERFACE, "Locked"),
            )
            .await?;
        bool::try_from(locked).map_err(|_| unusable("a malformed lock state"))
    }

    /// The items holding `installation`'s key: unlocked, then locked.
    async fn search(
        &self,
        installation: &[u8; 16],
    ) -> Result<(Vec<OwnedObjectPath>, Vec<OwnedObjectPath>), SecretError> {
        let (app, id) = attributes(installation);
        let attrs = HashMap::from([("application", app.as_str()), ("installation", id.as_str())]);
        self.call(SERVICE_PATH, SERVICE_INTERFACE, "SearchItems", &(attrs,))
            .await
    }

    /// A reply that names a prompt asks for someone to confirm. The
    /// prompt is dismissed without being shown, and the call refused.
    async fn refuse_prompt(&self, prompt: &OwnedObjectPath) -> Result<(), SecretError> {
        if prompt.as_str() == "/" {
            return Ok(());
        }
        let dismissed: Result<(), _> = self
            .call(prompt.as_str(), PROMPT_INTERFACE, "Dismiss", &())
            .await;
        if let Err(e) = dismissed {
            tracing::debug!(error = %e, "could not dismiss a Secret Service prompt");
        }
        Err(SecretError::Confirmation)
    }

    async fn store(&self, bundle: &KeyBundle) -> Result<(), SecretError> {
        let collection = self.default_collection().await?;
        if self.is_locked(collection.as_str()).await? {
            return Err(SecretError::Locked);
        }
        let (app, id) = attributes(&bundle.installation);
        let attrs = HashMap::from([("application", app.as_str()), ("installation", id.as_str())]);
        let properties = HashMap::from([
            (
                "org.freedesktop.Secret.Item.Label",
                Value::from("Hyakume device key"),
            ),
            ("org.freedesktop.Secret.Item.Attributes", Value::from(attrs)),
        ]);
        let mut iv = [0u8; 16];
        OsRng.fill_bytes(&mut iv);
        let value = cbc::Encryptor::<aes::Aes128>::new(&(*self.key).into(), &iv.into())
            .encrypt_padded_vec_mut::<Pkcs7>(&bundle.encode());
        let secret = (
            self.session.clone(),
            iv.to_vec(),
            value,
            "application/octet-stream",
        );
        let (_item, prompt): (OwnedObjectPath, OwnedObjectPath) = self
            .call(
                collection.as_str(),
                COLLECTION_INTERFACE,
                "CreateItem",
                &(properties, secret, true),
            )
            .await?;
        self.refuse_prompt(&prompt).await
    }

    async fn load(&self, installation: &[u8; 16]) -> Result<KeyBundle, SecretError> {
        let (unlocked, locked) = self.search(installation).await?;
        let item = match (unlocked.as_slice(), locked.is_empty()) {
            ([item], _) => item,
            ([], false) => return Err(SecretError::Locked),
            ([], true) => return Err(SecretError::Missing),
            _ => {
                return Err(SecretError::Corrupt(
                    "more than one key for this installation",
                ));
            }
        };
        let (_, iv, value, _): Secret = self
            .call(
                item.as_str(),
                ITEM_INTERFACE,
                "GetSecret",
                &(&self.session,),
            )
            .await?;
        let iv: [u8; 16] = iv.try_into().map_err(|_| unusable("a malformed secret"))?;
        let plain = cbc::Decryptor::<aes::Aes128>::new(&(*self.key).into(), &iv.into())
            .decrypt_padded_vec_mut::<Pkcs7>(&value)
            .map_err(|_| unusable("a secret that does not decrypt"))?;
        KeyBundle::decode(&Zeroizing::new(plain), installation)
    }

    async fn delete(&self, installation: &[u8; 16]) -> Result<(), SecretError> {
        let (unlocked, locked) = self.search(installation).await?;
        if !locked.is_empty() {
            return Err(SecretError::Locked);
        }
        for item in &unlocked {
            let prompt: OwnedObjectPath = self
                .call(item.as_str(), ITEM_INTERFACE, "Delete", &())
                .await?;
            self.refuse_prompt(&prompt).await?;
        }
        Ok(())
    }
}

async fn call<B, R>(
    conn: &zbus::Connection,
    path: &str,
    interface: &str,
    method: &str,
    body: &B,
) -> Result<R, SecretError>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
    R: zbus::zvariant::Type + serde::de::DeserializeOwned,
{
    let reply =
        bounded(conn.call_method(Some(SERVICE), path, Some(interface), method, body)).await?;
    reply
        .body()
        .deserialize()
        .map_err(|e| SecretError::Unavailable(e.to_string()))
}

/// The Secret Service's key exchange: Diffie-Hellman in [`MODP_1024`],
/// then HKDF-SHA256 of the shared value, padded to the prime's length,
/// into an AES-128 key.
struct Exchange {
    secret: U1024,
}

impl Drop for Exchange {
    fn drop(&mut self) {
        self.secret.zeroize();
    }
}

impl Exchange {
    fn new() -> Self {
        let mut bytes = Zeroizing::new([0u8; 128]);
        OsRng.fill_bytes(&mut *bytes);
        Self {
            secret: U1024::from_be_slice(&*bytes),
        }
    }

    fn power(base: &U1024, exponent: &U1024) -> U1024 {
        let params = DynResidueParams::new(&MODP_1024);
        DynResidue::new(base, params).pow(exponent).retrieve()
    }

    /// The public value, big-endian without leading zeros, as other
    /// Secret Service clients send it.
    fn public(&self) -> Vec<u8> {
        let public = Self::power(&U1024::from_u8(2), &self.secret).to_be_bytes();
        let start = public
            .iter()
            .position(|b| *b != 0)
            .unwrap_or(public.len() - 1);
        public[start..].to_vec()
    }

    /// The session key shared with the holder of `theirs`, or `None` if
    /// `theirs` is not a usable public value.
    fn key(&self, theirs: &[u8]) -> Option<Zeroizing<[u8; 16]>> {
        let len = theirs.len();
        if len > 128 {
            return None;
        }
        let mut padded = [0u8; 128];
        padded[128 - len..].copy_from_slice(theirs);
        let theirs = U1024::from_be_slice(&padded);
        // 0, 1, and p - 1 would make the shared value one anyone knows.
        if theirs <= U1024::ONE || theirs >= MODP_1024.wrapping_sub(&U1024::ONE) {
            return None;
        }
        let shared = Zeroizing::new(Self::power(&theirs, &self.secret).to_be_bytes());
        let mut key = Zeroizing::new([0u8; 16]);
        Hkdf::<Sha256>::new(None, &*shared)
            .expand(&[], &mut *key)
            .ok()?;
        Some(key)
    }
}

fn attributes(installation: &[u8; 16]) -> (String, String) {
    (
        APPLICATION.to_owned(),
        data_encoding::HEXLOWER.encode(installation),
    )
}

/// Whether the default collection is reachable and unlocked, without
/// unlocking it.
pub async fn probe() -> Availability {
    let Ok(keyring) = Keyring::connect().await else {
        return Availability::Unavailable;
    };
    let Ok(collection) = keyring.default_collection().await else {
        return Availability::Unavailable;
    };
    match keyring.is_locked(collection.as_str()).await {
        Ok(false) => Availability::Unlocked,
        Ok(true) => Availability::Locked,
        Err(_) => Availability::Unavailable,
    }
}

/// Stores `bundle` in the default collection, replacing an earlier key of
/// the same installation. Refuses a locked collection.
pub async fn store(bundle: &KeyBundle) -> Result<(), SecretError> {
    Keyring::connect().await?.store(bundle).await
}

/// Loads this installation's key. A locked item reads as locked.
pub async fn load(installation: &[u8; 16]) -> Result<KeyBundle, SecretError> {
    Keyring::connect().await?.load(installation).await
}

/// Deletes this installation's key, if the Secret Service holds one.
pub async fn delete(installation: &[u8; 16]) -> Result<(), SecretError> {
    Keyring::connect().await?.delete(installation).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use zbus::zvariant::ObjectPath;

    fn temp() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hyakume-secrets-{}-{:x}",
            std::process::id(),
            OsRng.next_u64()
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    fn bundle() -> KeyBundle {
        KeyBundle {
            installation: [5; 16],
            seed: Zeroizing::new([9; 32]),
        }
    }

    #[test]
    fn a_key_file_opens_only_with_its_passphrase_and_its_installation() {
        let dir = temp();
        let path = dir.join("identity.age");
        write_key_file(&path, &bundle(), b"correct horse").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // The seed does not appear in the file.
        let raw = std::fs::read(&path).unwrap();
        assert!(!raw.windows(32).any(|w| w == [9; 32]));

        let loaded = read_key_file(&path, b"correct horse", &[5; 16]).unwrap();
        assert_eq!(*loaded.seed, [9; 32]);
        assert!(matches!(
            read_key_file(&path, b"wrong horse!", &[5; 16]),
            Err(SecretError::WrongPassphrase)
        ));
        assert!(matches!(
            read_key_file(&path, b"correct horse", &[6; 16]),
            Err(SecretError::Corrupt(_))
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_damaged_or_truncated_key_file_is_refused() {
        let dir = temp();
        let path = dir.join("identity.age");
        write_key_file(&path, &bundle(), b"correct horse").unwrap();
        let raw = std::fs::read(&path).unwrap();
        std::fs::write(&path, &raw[..raw.len() - 1]).unwrap();
        assert!(read_key_file(&path, b"correct horse", &[5; 16]).is_err());
        let mut flipped = raw.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 1;
        std::fs::write(&path, &flipped).unwrap();
        assert!(read_key_file(&path, b"correct horse", &[5; 16]).is_err());
        assert!(matches!(
            read_key_file(&dir.join("absent.age"), b"correct horse", &[5; 16]),
            Err(SecretError::Missing)
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Plays a Secret Service that wants someone to confirm every item it
    /// creates or deletes, and counts the prompts it is asked to show.
    async fn confirm_everything(conn: zbus::Connection, shown: Arc<AtomicUsize>) {
        use futures_util::StreamExt;
        let path = |p: &'static str| ObjectPath::from_static_str_unchecked(p);
        let ours = Exchange::new();
        let mut calls = zbus::MessageStream::from(&conn);
        while let Some(Ok(call)) = calls.next().await {
            let header = call.header();
            let replied = match header.member().map(|m| m.as_str()) {
                Some("OpenSession") => {
                    let public = Value::from(ours.public());
                    conn.reply(&header, &(public, path("/session"))).await
                }
                Some("ReadAlias") => conn.reply(&header, &path("/collection")).await,
                Some("Get") => conn.reply(&header, &Value::from(false)).await,
                Some("SearchItems") => {
                    let none: Vec<ObjectPath<'_>> = Vec::new();
                    conn.reply(&header, &(vec![path("/item")], none)).await
                }
                Some("CreateItem") => conn.reply(&header, &(path("/"), path("/prompt"))).await,
                Some("Delete") => conn.reply(&header, &path("/prompt")).await,
                Some("Prompt") => {
                    shown.fetch_add(1, Ordering::SeqCst);
                    conn.reply(&header, &()).await
                }
                _ => conn.reply(&header, &()).await,
            };
            replied.unwrap();
        }
    }

    #[tokio::test]
    async fn a_secret_service_that_wants_confirmation_never_gets_to_ask() {
        let (ours, theirs) = tokio::net::UnixStream::pair().unwrap();
        let theirs = zbus::connection::Builder::unix_stream(theirs)
            .server(zbus::Guid::generate())
            .unwrap()
            .p2p()
            .build();
        let ours = zbus::connection::Builder::unix_stream(ours).p2p().build();
        let (theirs, ours) = tokio::try_join!(theirs, ours).unwrap();
        let shown = Arc::new(AtomicUsize::new(0));
        tokio::spawn(confirm_everything(theirs, shown.clone()));

        let keyring = Keyring::open(ours).await.unwrap();
        assert!(matches!(
            keyring.store(&bundle()).await,
            Err(SecretError::Confirmation)
        ));
        assert!(matches!(
            keyring.delete(&[5; 16]).await,
            Err(SecretError::Confirmation)
        ));
        assert_eq!(shown.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn both_sides_of_the_key_exchange_agree_and_trivial_values_are_refused() {
        let (a, b) = (Exchange::new(), Exchange::new());
        let key = a.key(&b.public()).unwrap();
        assert_eq!(*key, *b.key(&a.public()).unwrap());
        assert_ne!(*key, *Exchange::new().key(&a.public()).unwrap());

        // 0, 1, and p - 1 would make the shared value one anyone knows.
        let last = MODP_1024.wrapping_sub(&U1024::ONE).to_be_bytes();
        for theirs in [&[0u8][..], &[1], &last, &MODP_1024.to_be_bytes(), &[1; 129]] {
            assert!(a.key(theirs).is_none());
        }
    }

    #[test]
    fn short_passphrases_are_refused_for_a_new_key_file() {
        let dir = temp();
        assert!(matches!(
            write_key_file(&dir.join("k.age"), &bundle(), b"short"),
            Err(SecretError::WeakPassphrase)
        ));
        assert!(!dir.join("k.age").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
