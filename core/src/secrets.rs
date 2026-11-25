//! Where sign-in secrets are kept. The file at a store's path is one of:
//!
//! - an index of the accounts whose secrets are in the OS credential store
//!   (Windows Credential Manager, macOS Keychain, Linux Secret Service);
//! - the accounts sealed with DPAPI (Windows) or with a key derived from a
//!   passphrase by Argon2id and XChaCha20-Poly1305 (macOS, Linux);
//! - the accounts as plain JSON, only when the person chose that.
//!
//! Every secret is keyed by provider and account. Sources, verified facts and
//! limits: docs/evidence/account-pool.md.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

const FORMAT: &str = "inocode-credentials";
const VERSION: u32 = 2;
/// The service every OS credential store entry is filed under.
const SERVICE: &str = "inocode";
/// The account each provider's single sign-in is stored as.
pub const DEFAULT_ACCOUNT: &str = "default";
/// Bytes per OS credential store entry. Windows refuses a credential blob
/// above `CRED_MAX_CREDENTIAL_BLOB_SIZE` (2,560 bytes), which one OAuth
/// credential can exceed, so a secret is split over several entries.
const CHUNK: usize = 2048;

/// Secrets by provider, then account.
pub(crate) type Accounts = BTreeMap<String, BTreeMap<String, Value>>;

/// Where a store keeps its secrets: the `credentials.store` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StoreMode {
    /// The OS credential store, or an encrypted file when it cannot be used.
    #[default]
    Auto,
    /// The OS credential store only.
    Keyring,
    /// A file sealed with DPAPI on Windows, or with a passphrase elsewhere.
    EncryptedFile,
    /// Plain JSON.
    Plaintext,
}

impl StoreMode {
    /// Reads a `credentials.store` value.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim() {
            "auto" => Ok(Self::Auto),
            "keyring" => Ok(Self::Keyring),
            "encrypted-file" => Ok(Self::EncryptedFile),
            "plaintext" => Ok(Self::Plaintext),
            other => Err(format!(
                "unknown credentials.store \"{other}\"; expected auto, keyring, encrypted-file, or plaintext"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Keyring => "keyring",
            Self::EncryptedFile => "encrypted-file",
            Self::Plaintext => "plaintext",
        }
    }
}

static MODE: Mutex<Option<StoreMode>> = Mutex::new(None);

/// The core unit tests write throwaway stores in the temporary folder. Away
/// from Windows the default would wait for a passphrase, so they use plain
/// JSON there; the backends themselves are tested with explicit modes.
#[cfg(all(test, not(windows)))]
const DEFAULT_MODE: StoreMode = StoreMode::Plaintext;
#[cfg(not(all(test, not(windows))))]
const DEFAULT_MODE: StoreMode = StoreMode::Auto;

/// Sets the mode [`crate::oauth::Store::load`] uses in this process.
pub fn set_mode(mode: StoreMode) {
    *MODE.lock().unwrap_or_else(PoisonError::into_inner) = Some(mode);
}

/// The mode [`crate::oauth::Store::load`] uses: the one set, else auto.
pub fn mode() -> StoreMode {
    MODE.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .copied()
        .unwrap_or(DEFAULT_MODE)
}

/// The passphrase of passphrase-sealed stores and the keys derived from it,
/// kept only in memory.
struct Unlocked {
    passphrase: Zeroizing<String>,
    keys: Vec<(Kdf, Zeroizing<[u8; 32]>)>,
}

type Slot = Arc<Mutex<Option<Unlocked>>>;

static PASSPHRASE: LazyLock<Slot> = LazyLock::new(|| Arc::new(Mutex::new(None)));

/// Gives this process the passphrase of passphrase-sealed stores. It is never
/// written anywhere; a wrong one shows when a store is opened.
pub fn unlock(passphrase: &str) -> Result<(), String> {
    if passphrase.is_empty() {
        return Err("the passphrase is empty".to_string());
    }
    *PASSPHRASE.lock().unwrap_or_else(PoisonError::into_inner) = Some(Unlocked {
        passphrase: Zeroizing::new(passphrase.to_string()),
        keys: Vec::new(),
    });
    Ok(())
}

/// Forgets the passphrase and the keys derived from it.
pub fn lock() {
    *PASSPHRASE.lock().unwrap_or_else(PoisonError::into_inner) = None;
}

/// One OS credential store, addressed by entry name within [`SERVICE`].
pub(crate) trait Keyring: Send + Sync {
    fn get(&self, user: &str) -> Result<Option<Vec<u8>>, String>;
    fn set(&self, user: &str, secret: &[u8]) -> Result<(), String>;
    fn delete(&self, user: &str) -> Result<(), String>;
}

struct OsKeyring;

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
impl Keyring for OsKeyring {
    fn get(&self, user: &str) -> Result<Option<Vec<u8>>, String> {
        off_runtime(|| {
            match keyring::Entry::new(SERVICE, user).and_then(|entry| entry.get_secret()) {
                Ok(secret) => Ok(Some(secret)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(error) => Err(format!("the OS credential store failed: {error}")),
            }
        })
    }

    fn set(&self, user: &str, secret: &[u8]) -> Result<(), String> {
        off_runtime(|| {
            keyring::Entry::new(SERVICE, user)
                .and_then(|entry| entry.set_secret(secret))
                .map_err(|error| format!("the OS credential store failed: {error}"))
        })
    }

    fn delete(&self, user: &str) -> Result<(), String> {
        off_runtime(|| {
            match keyring::Entry::new(SERVICE, user).and_then(|entry| entry.delete_credential()) {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(format!("the OS credential store failed: {error}")),
            }
        })
    }
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
impl Keyring for OsKeyring {
    fn get(&self, _user: &str) -> Result<Option<Vec<u8>>, String> {
        Err("this system has no supported OS credential store".to_string())
    }

    fn set(&self, _user: &str, _secret: &[u8]) -> Result<(), String> {
        Err("this system has no supported OS credential store".to_string())
    }

    fn delete(&self, _user: &str) -> Result<(), String> {
        Err("this system has no supported OS credential store".to_string())
    }
}

/// The Secret Service client blocks on a Tokio runtime of its own, which
/// panics on a thread that is already driving one, so it runs on a thread
/// of its own.
#[cfg(target_os = "linux")]
fn off_runtime<T: Send>(work: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| match scope.spawn(work).join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    })
}

#[cfg(all(not(target_os = "linux"), any(windows, target_os = "macos")))]
fn off_runtime<T>(work: impl FnOnce() -> T) -> T {
    work()
}

/// Where one store's secrets are, as its file records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Stored {
    Missing,
    /// The original `auth.json`: plain JSON keyed by provider.
    Legacy,
    Keyring {
        prefix: String,
        names: BTreeMap<String, Vec<String>>,
    },
    Plaintext,
    Dpapi,
    Passphrase(Kdf),
}

impl Stored {
    fn backend(&self) -> Option<Backend> {
        match self {
            Self::Missing => None,
            Self::Legacy | Self::Plaintext => Some(Backend::Plaintext),
            Self::Keyring { .. } => Some(Backend::Keyring),
            Self::Dpapi => Some(Backend::Dpapi),
            Self::Passphrase(_) => Some(Backend::Passphrase),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
    Keyring,
    Dpapi,
    Passphrase,
    Plaintext,
}

impl Backend {
    fn describe(self) -> &'static str {
        match self {
            Self::Keyring => "the OS credential store",
            Self::Dpapi => "a DPAPI-sealed file",
            Self::Passphrase => "a passphrase-sealed file",
            Self::Plaintext => "a plain JSON file",
        }
    }
}

/// The encrypted file this platform uses when the OS credential store does not.
fn platform_sealed() -> Backend {
    if cfg!(windows) {
        Backend::Dpapi
    } else {
        Backend::Passphrase
    }
}

/// Argon2id cost of new passphrase-sealed files: the second recommended
/// option of RFC 9106 section 4 (64 MiB, 3 passes, 4 lanes).
#[derive(Debug, Clone, Copy)]
pub(crate) struct KdfCost {
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
}

const PRODUCTION_COST: KdfCost = KdfCost {
    memory_kib: 64 * 1024,
    iterations: 3,
    parallelism: 4,
};

/// The key derivation a passphrase-sealed file records in its header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Kdf {
    algorithm: String,
    version: u32,
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
    salt: String,
}

const CIPHER: &str = "xchacha20poly1305";

#[derive(Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "kebab-case")]
enum Envelope {
    Keyring {
        prefix: String,
        accounts: BTreeMap<String, Vec<String>>,
    },
    Plaintext {
        accounts: Accounts,
    },
    Dpapi {
        sealed: String,
    },
    Passphrase {
        kdf: Kdf,
        cipher: String,
        nonce: String,
        sealed: String,
    },
}

#[derive(Serialize, Deserialize)]
struct StoreFile {
    format: String,
    version: u32,
    #[serde(flatten)]
    envelope: Envelope,
}

/// What a store holds and where it was read from.
pub(crate) struct Contents {
    pub(crate) stored: Stored,
    pub(crate) accounts: Accounts,
}

/// One store: the file at `path` and, for the OS credential store, its
/// entries.
pub(crate) struct Vault {
    path: PathBuf,
    mode: StoreMode,
    /// Only the store at the default location may use the OS credential
    /// store in auto mode: its entries are shared by every store of this
    /// user and outlive a store's folder, so throwaway stores elsewhere
    /// would leave them behind.
    default_location: bool,
    keyring: Arc<dyn Keyring>,
    passphrase: Slot,
    sealed: Backend,
    cost: KdfCost,
}

impl Vault {
    pub(crate) fn new(path: PathBuf, mode: StoreMode, default_location: bool) -> Self {
        Self {
            path,
            mode,
            default_location,
            keyring: Arc::new(OsKeyring),
            passphrase: Arc::clone(&PASSPHRASE),
            sealed: platform_sealed(),
            cost: PRODUCTION_COST,
        }
    }

    /// Reads every account, after moving the store to the backend its mode
    /// selects when it is elsewhere (the original plain `auth.json` included).
    pub(crate) fn load(&self) -> Result<Accounts, String> {
        let contents = self.read()?;
        if self.move_to(&contents.stored).is_none() {
            return Ok(contents.accounts);
        }
        let _lock = lock_store(&self.path)?;
        Ok(self.load_locked()?.accounts)
    }

    /// [`Vault::load`] for a caller holding [`lock_store`].
    pub(crate) fn load_locked(&self) -> Result<Contents, String> {
        let contents = self.read()?;
        let Some(target) = self.move_to(&contents.stored) else {
            return Ok(contents);
        };
        let stored = self
            .write(target, &contents.accounts, None, &contents.stored)
            .map_err(|error| {
                format!(
                    "the credentials in {} were not moved to {}, and the file was left as it was: {error}",
                    self.path.display(),
                    target.describe()
                )
            })?;
        Ok(Contents {
            stored,
            accounts: contents.accounts,
        })
    }

    /// Stores `contents` after its `provider`/`account` secret changed. The
    /// caller holds [`lock_store`] and read `contents` with
    /// [`Vault::load_locked`].
    pub(crate) fn save_locked(
        &self,
        contents: &Contents,
        provider: &str,
        account: &str,
    ) -> Result<(), String> {
        let target = contents
            .stored
            .backend()
            .unwrap_or_else(|| self.desired(&contents.stored));
        self.write(
            target,
            &contents.accounts,
            Some((provider, account)),
            &contents.stored,
        )
        .map(|_| ())
    }

    /// The backend the mode selects for a store found as `stored`.
    fn desired(&self, stored: &Stored) -> Backend {
        match self.mode {
            StoreMode::Plaintext => Backend::Plaintext,
            StoreMode::Keyring => Backend::Keyring,
            StoreMode::EncryptedFile => self.sealed,
            StoreMode::Auto => match stored.backend() {
                Some(backend) if backend != Backend::Plaintext => backend,
                _ if self.default_location => Backend::Keyring,
                _ => self.sealed,
            },
        }
    }

    /// The backend an existing store must move to, if any.
    fn move_to(&self, stored: &Stored) -> Option<Backend> {
        let current = stored.backend()?;
        let desired = self.desired(stored);
        (desired != current || *stored == Stored::Legacy).then_some(desired)
    }

    /// Writes `accounts` to `target` (only `changed` when it is already
    /// there), falling back to the encrypted file in auto mode when the OS
    /// credential store fails. The file at the store's path is replaced only
    /// after the new copy reads back equal.
    fn write(
        &self,
        target: Backend,
        accounts: &Accounts,
        changed: Option<(&str, &str)>,
        previous: &Stored,
    ) -> Result<Stored, String> {
        match self.write_to(target, accounts, changed, previous) {
            Err(error) if target == Backend::Keyring && self.mode == StoreMode::Auto => self
                .write_to(self.sealed, accounts, None, previous)
                .map_err(|second| format!("{error}; and then {second}")),
            result => result,
        }
    }

    fn write_to(
        &self,
        target: Backend,
        accounts: &Accounts,
        changed: Option<(&str, &str)>,
        previous: &Stored,
    ) -> Result<Stored, String> {
        let stored = if target == Backend::Keyring {
            self.write_keyring(accounts, changed, previous)?
        } else {
            let (bytes, stored) = self.encode(target, accounts, previous)?;
            write_verified(&self.path, &bytes, |written| {
                if self.decode(written)?.accounts == *accounts {
                    Ok(())
                } else {
                    Err("the written copy reads back different".to_string())
                }
            })?;
            stored
        };
        if let (Stored::Keyring { prefix, names }, false) =
            (previous, matches!(stored, Stored::Keyring { .. }))
        {
            for (provider, accounts) in names {
                for account in accounts {
                    let _ = delete_secret(&*self.keyring, &entry_name(prefix, provider, account));
                }
            }
        }
        Ok(stored)
    }

    fn write_keyring(
        &self,
        accounts: &Accounts,
        changed: Option<(&str, &str)>,
        previous: &Stored,
    ) -> Result<Stored, String> {
        let (prefix, old_names) = match previous {
            Stored::Keyring { prefix, names } => (prefix.clone(), Some(names)),
            _ => (self.keyring_prefix(), None),
        };
        let names: BTreeMap<String, Vec<String>> = accounts
            .iter()
            .map(|(provider, accounts)| (provider.clone(), accounts.keys().cloned().collect()))
            .collect();
        let mut written = Vec::new();
        for (provider, accounts) in accounts {
            for (account, value) in accounts {
                if old_names.is_some()
                    && changed.is_some_and(|c| c != (provider.as_str(), account.as_str()))
                {
                    continue;
                }
                check_name(provider)?;
                check_name(account)?;
                let name = entry_name(&prefix, provider, account);
                let bytes = Zeroizing::new(serde_json::to_vec(value).map_err(|e| e.to_string())?);
                let result = put_secret(&*self.keyring, &name, &bytes).and_then(|()| {
                    match get_secret(&*self.keyring, &name)? {
                        Some(read) if *read == *bytes => Ok(()),
                        _ => Err("the OS credential store returned something else".to_string()),
                    }
                });
                if let Err(error) = result {
                    if old_names.is_none() {
                        for name in written.iter().chain([&name]) {
                            let _ = delete_secret(&*self.keyring, name);
                        }
                    }
                    return Err(error);
                }
                written.push(name);
            }
        }
        if old_names != Some(&names) {
            let index = StoreFile {
                format: FORMAT.to_string(),
                version: VERSION,
                envelope: Envelope::Keyring {
                    prefix: prefix.clone(),
                    accounts: names.clone(),
                },
            };
            let bytes = serde_json::to_vec_pretty(&index).map_err(|e| e.to_string())?;
            write_verified(&self.path, &bytes, |written| {
                if self.decode(written)?.accounts == *accounts {
                    Ok(())
                } else {
                    Err("the OS credential store reads back different".to_string())
                }
            })?;
        }
        Ok(Stored::Keyring { prefix, names })
    }

    /// Entry names start with this, so a store chosen to use the OS
    /// credential store outside the default location keeps its own entries.
    fn keyring_prefix(&self) -> String {
        if self.default_location {
            return String::new();
        }
        let digest = Sha256::digest(self.path.to_string_lossy().as_bytes());
        format!("{}/", hex(&digest[..8]))
    }

    fn read(&self) -> Result<Contents, String> {
        match std::fs::read(&self.path) {
            Ok(bytes) => self.decode(&bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Contents {
                stored: Stored::Missing,
                accounts: Accounts::new(),
            }),
            Err(error) => Err(format!(
                "cannot read the credential store {}: {error}",
                self.path.display()
            )),
        }
    }

    fn decode(&self, bytes: &[u8]) -> Result<Contents, String> {
        let invalid = |error: String| {
            format!(
                "the credential store {} is not valid: {error}",
                self.path.display()
            )
        };
        let value: Value = serde_json::from_slice(bytes).map_err(|e| invalid(e.to_string()))?;
        if value.get("format").is_none() {
            let legacy: BTreeMap<String, Value> =
                serde_json::from_value(value).map_err(|e| invalid(e.to_string()))?;
            let accounts = legacy
                .into_iter()
                .map(|(provider, secret)| {
                    (
                        provider,
                        BTreeMap::from([(DEFAULT_ACCOUNT.to_string(), secret)]),
                    )
                })
                .collect();
            return Ok(Contents {
                stored: Stored::Legacy,
                accounts,
            });
        }
        let file: StoreFile = serde_json::from_value(value).map_err(|e| invalid(e.to_string()))?;
        if file.format != FORMAT || file.version != VERSION {
            return Err(invalid(format!(
                "format {} version {} is not one this build reads",
                file.format, file.version
            )));
        }
        let (stored, accounts) = match file.envelope {
            Envelope::Plaintext { accounts } => (Stored::Plaintext, accounts),
            Envelope::Keyring { prefix, accounts } => {
                let mut read = Accounts::new();
                for (provider, names) in &accounts {
                    for account in names {
                        let secret = get_secret(
                            &*self.keyring,
                            &entry_name(&prefix, provider, account),
                        )?
                        .ok_or_else(|| {
                            format!("{provider}/{account} is missing from the OS credential store")
                        })?;
                        let value = serde_json::from_slice(&secret)
                            .map_err(|e| invalid(format!("{provider}/{account}: {e}")))?;
                        read.entry(provider.clone())
                            .or_default()
                            .insert(account.clone(), value);
                    }
                }
                (
                    Stored::Keyring {
                        prefix,
                        names: accounts,
                    },
                    read,
                )
            }
            Envelope::Dpapi { sealed } => {
                let sealed = STANDARD
                    .decode(sealed)
                    .map_err(|e| invalid(e.to_string()))?;
                let plain = dpapi::unprotect(&sealed)
                    .map_err(|error| format!("cannot decrypt {}: {error}", self.path.display()))?;
                let accounts =
                    serde_json::from_slice(&plain).map_err(|e| invalid(e.to_string()))?;
                (Stored::Dpapi, accounts)
            }
            Envelope::Passphrase {
                kdf,
                cipher,
                nonce,
                sealed,
            } => {
                if cipher != CIPHER {
                    return Err(invalid(format!("unknown cipher {cipher}")));
                }
                let plain = self.open_sealed(&kdf, &nonce, &sealed)?;
                let accounts =
                    serde_json::from_slice(&plain).map_err(|e| invalid(e.to_string()))?;
                (Stored::Passphrase(kdf), accounts)
            }
        };
        Ok(Contents { stored, accounts })
    }

    fn encode(
        &self,
        target: Backend,
        accounts: &Accounts,
        previous: &Stored,
    ) -> Result<(Vec<u8>, Stored), String> {
        let (envelope, stored) = match target {
            Backend::Plaintext => (
                Envelope::Plaintext {
                    accounts: accounts.clone(),
                },
                Stored::Plaintext,
            ),
            Backend::Dpapi => {
                let plain =
                    Zeroizing::new(serde_json::to_vec(accounts).map_err(|e| e.to_string())?);
                let sealed = dpapi::protect(&plain)?;
                (
                    Envelope::Dpapi {
                        sealed: STANDARD.encode(sealed),
                    },
                    Stored::Dpapi,
                )
            }
            Backend::Passphrase => {
                let kdf = match previous {
                    Stored::Passphrase(kdf) => kdf.clone(),
                    _ => Kdf {
                        algorithm: "argon2id".to_string(),
                        version: 0x13,
                        memory_kib: self.cost.memory_kib,
                        iterations: self.cost.iterations,
                        parallelism: self.cost.parallelism,
                        salt: STANDARD.encode(random::<16>()?),
                    },
                };
                let plain =
                    Zeroizing::new(serde_json::to_vec(accounts).map_err(|e| e.to_string())?);
                let nonce = STANDARD.encode(random::<24>()?);
                let sealed = self.seal(&kdf, &nonce, &plain)?;
                (
                    Envelope::Passphrase {
                        kdf: kdf.clone(),
                        cipher: CIPHER.to_string(),
                        nonce,
                        sealed,
                    },
                    Stored::Passphrase(kdf),
                )
            }
            Backend::Keyring => unreachable!("the OS credential store has no file encoding"),
        };
        let file = StoreFile {
            format: FORMAT.to_string(),
            version: VERSION,
            envelope,
        };
        Ok((
            serde_json::to_vec_pretty(&file).map_err(|e| e.to_string())?,
            stored,
        ))
    }

    fn seal(&self, kdf: &Kdf, nonce: &str, plain: &[u8]) -> Result<String, String> {
        use chacha20poly1305::aead::{Aead, KeyInit, Payload};
        let key = self.key(kdf)?;
        let nonce_bytes = STANDARD.decode(nonce).map_err(|e| e.to_string())?;
        let cipher = chacha20poly1305::XChaCha20Poly1305::new(key.as_ref().into());
        let sealed = cipher
            .encrypt(
                chacha20poly1305::XNonce::from_slice(&nonce_bytes),
                Payload {
                    msg: plain,
                    aad: &header_aad(kdf, nonce),
                },
            )
            .map_err(|_| "cannot encrypt the credential store".to_string())?;
        Ok(STANDARD.encode(sealed))
    }

    fn open_sealed(
        &self,
        kdf: &Kdf,
        nonce: &str,
        sealed: &str,
    ) -> Result<Zeroizing<Vec<u8>>, String> {
        use chacha20poly1305::aead::{Aead, KeyInit, Payload};
        let broken = || {
            format!(
                "cannot decrypt {}: the passphrase is wrong or the file was changed",
                self.path.display()
            )
        };
        let nonce_bytes = STANDARD.decode(nonce).map_err(|_| broken())?;
        let sealed = STANDARD.decode(sealed).map_err(|_| broken())?;
        if nonce_bytes.len() != 24 {
            return Err(broken());
        }
        let key = self.key(kdf)?;
        let cipher = chacha20poly1305::XChaCha20Poly1305::new(key.as_ref().into());
        cipher
            .decrypt(
                chacha20poly1305::XNonce::from_slice(&nonce_bytes),
                Payload {
                    msg: &sealed,
                    aad: &header_aad(kdf, nonce),
                },
            )
            .map(Zeroizing::new)
            .map_err(|_| broken())
    }

    /// The key `kdf` derives from the unlocked passphrase, derived once per
    /// header because Argon2id is deliberately slow.
    fn key(&self, kdf: &Kdf) -> Result<Zeroizing<[u8; 32]>, String> {
        let mut slot = self
            .passphrase
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let unlocked = slot.as_mut().ok_or_else(|| {
            format!(
                "the credential store {} is sealed with a passphrase and is locked; unlock it with the passphrase first",
                self.path.display()
            )
        })?;
        if let Some((_, key)) = unlocked.keys.iter().find(|(known, _)| known == kdf) {
            return Ok(key.clone());
        }
        let invalid = || {
            format!(
                "the credential store {} records a key derivation this build does not accept",
                self.path.display()
            )
        };
        // Bounds keep a changed header from demanding unbounded memory or time.
        if kdf.algorithm != "argon2id"
            || kdf.version != 0x13
            || kdf.memory_kib > 1024 * 1024
            || !(1..=64).contains(&kdf.iterations)
            || !(1..=16).contains(&kdf.parallelism)
        {
            return Err(invalid());
        }
        let salt = STANDARD.decode(&kdf.salt).map_err(|_| invalid())?;
        if salt.len() < 16 {
            return Err(invalid());
        }
        let params = argon2::Params::new(kdf.memory_kib, kdf.iterations, kdf.parallelism, Some(32))
            .map_err(|_| invalid())?;
        let mut key = Zeroizing::new([0u8; 32]);
        argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
            .hash_password_into(unlocked.passphrase.as_bytes(), &salt, key.as_mut())
            .map_err(|_| invalid())?;
        unlocked.keys.push((kdf.clone(), key.clone()));
        Ok(key)
    }
}

/// Bytes the cipher authenticates with the secrets, so a changed header
/// fails the decryption as a changed ciphertext does.
fn header_aad(kdf: &Kdf, nonce: &str) -> Vec<u8> {
    serde_json::to_vec(&(FORMAT, VERSION, "passphrase", kdf, CIPHER, nonce)).unwrap_or_default()
}

fn entry_name(prefix: &str, provider: &str, account: &str) -> String {
    format!("{prefix}{provider}/{account}")
}

/// Provider and account names must not contain the separators of entry names.
fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.contains(['/', '#']) || name.chars().any(char::is_whitespace) {
        return Err(format!("\"{name}\" cannot name a stored account"));
    }
    Ok(())
}

/// A secret is stored as a manifest entry, `inocode-secret-v1 <generation>
/// <parts> <sha256>`, and its parts at `<name>#<generation>.<i>`. New parts
/// are written under a new generation before the manifest points at them, so
/// a reader never mixes parts of two values.
fn put_secret(keyring: &dyn Keyring, name: &str, value: &[u8]) -> Result<(), String> {
    let previous = read_manifest(keyring, name).ok().flatten();
    let generation = hex(&random::<4>()?);
    let parts: Vec<&[u8]> = value.chunks(CHUNK).collect();
    for (index, part) in parts.iter().enumerate() {
        if let Err(error) = keyring.set(&part_name(name, &generation, index), part) {
            for written in 0..index {
                let _ = keyring.delete(&part_name(name, &generation, written));
            }
            return Err(error);
        }
    }
    let manifest = format!(
        "inocode-secret-v1 {generation} {} {}",
        parts.len(),
        hex(&Sha256::digest(value))
    );
    keyring.set(name, manifest.as_bytes())?;
    if let Some(old) = previous {
        for index in 0..old.parts {
            let _ = keyring.delete(&part_name(name, &old.generation, index));
        }
    }
    Ok(())
}

fn get_secret(keyring: &dyn Keyring, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
    // A writer replacing the value between reading the manifest and its parts
    // deletes the old parts; reading again finds the new ones.
    let mut last = String::new();
    for _ in 0..3 {
        let Some(manifest) = read_manifest(keyring, name)? else {
            return Ok(None);
        };
        let mut value = Zeroizing::new(Vec::new());
        let mut complete = true;
        for index in 0..manifest.parts {
            match keyring.get(&part_name(name, &manifest.generation, index))? {
                Some(part) => value.extend_from_slice(&Zeroizing::new(part)),
                None => {
                    complete = false;
                    break;
                }
            }
        }
        if complete && hex(&Sha256::digest(&value[..])) == manifest.sha256 {
            return Ok(Some(value));
        }
        last = format!("the OS credential store entry {name} is incomplete or changed");
    }
    Err(last)
}

fn delete_secret(keyring: &dyn Keyring, name: &str) -> Result<(), String> {
    if let Some(manifest) = read_manifest(keyring, name)? {
        for index in 0..manifest.parts {
            keyring.delete(&part_name(name, &manifest.generation, index))?;
        }
    }
    keyring.delete(name)
}

struct Manifest {
    generation: String,
    parts: usize,
    sha256: String,
}

fn read_manifest(keyring: &dyn Keyring, name: &str) -> Result<Option<Manifest>, String> {
    let Some(bytes) = keyring.get(name)? else {
        return Ok(None);
    };
    let text = String::from_utf8(bytes).unwrap_or_default();
    let mut fields = text.split(' ');
    match (
        fields.next(),
        fields.next(),
        fields.next().and_then(|n| n.parse().ok()),
        fields.next(),
        fields.next(),
    ) {
        (Some("inocode-secret-v1"), Some(generation), Some(parts), Some(sha256), None) => {
            Ok(Some(Manifest {
                generation: generation.to_string(),
                parts,
                sha256: sha256.to_string(),
            }))
        }
        _ => Err(format!(
            "the OS credential store entry {name} was not written by inocode"
        )),
    }
}

fn part_name(name: &str, generation: &str, index: usize) -> String {
    format!("{name}#{generation}.{index}")
}

fn random<const N: usize>() -> Result<[u8; N], String> {
    let mut bytes = [0u8; N];
    getrandom::getrandom(&mut bytes).map_err(|e| format!("no random numbers: {e}"))?;
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Whether `a` and `b` name the same file, also when spelled differently.
pub(crate) fn same_location(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    let folder = |path: &Path| path.parent().and_then(|parent| parent.canonicalize().ok());
    match (folder(a), folder(b)) {
        (Some(x), Some(y)) => x == y && a.file_name() == b.file_name(),
        _ => false,
    }
}

/// `path` with `suffix` appended to its file name.
pub(crate) fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

pub(crate) fn open_lock_file(path: &Path) -> Result<std::fs::File, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|e| format!("cannot open the lock file {}: {e}", path.display()))
}

/// Takes the OS's exclusive lock on `path`; dropping the file releases it,
/// and so does the process ending, so a crash never leaves a stale lock.
pub(crate) fn lock_file(path: &Path) -> Result<std::fs::File, String> {
    let file = open_lock_file(path)?;
    file.lock()
        .map_err(|e| format!("cannot lock {}: {e}", path.display()))?;
    Ok(file)
}

/// The lock held from reading a store to writing it back, so two processes
/// saving at once cannot both start from the same contents and drop each
/// other's change (reference `auth.store.lock`).
pub(crate) fn lock_store(path: &Path) -> Result<std::fs::File, String> {
    lock_file(&sibling(path, ".lock"))
}

/// Replaces `path` with `bytes` once a sibling copy, created owner-only where
/// the platform supports it, passes `verify`. A crash never leaves a
/// truncated store, and a copy that fails `verify` never replaces it.
fn write_verified(
    path: &Path,
    bytes: &[u8],
    verify: impl FnOnce(&[u8]) -> Result<(), String>,
) -> Result<(), String> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let temp = sibling(path, &format!(".tmp-{}", std::process::id()));
    // A leftover from an interrupted save may carry other permissions; the
    // file must be created fresh for the owner-only mode to apply.
    let _ = std::fs::remove_file(&temp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let failed = |error: String| {
        format!(
            "cannot write the credential store {}: {error}",
            path.display()
        )
    };
    let result = options
        .open(&temp)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .map_err(|e| failed(e.to_string()))
        .and_then(|()| std::fs::read(&temp).map_err(|e| failed(e.to_string())))
        .and_then(|written| verify(&written).map_err(failed))
        .and_then(|()| std::fs::rename(&temp, path).map_err(|e| failed(e.to_string())));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(windows)]
mod dpapi {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    use zeroize::Zeroizing;

    /// Passed as DPAPI's optional entropy, so a program that calls
    /// `CryptUnprotectData` on the blob without it gets nothing.
    const ENTROPY: &[u8] = b"inocode-credentials";

    fn blob(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: bytes.len() as u32,
            pbData: bytes.as_ptr().cast_mut(),
        }
    }

    /// Copies DPAPI's output and frees it with `LocalFree`, as the API requires.
    unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Zeroizing<Vec<u8>> {
        let bytes =
            Zeroizing::new(std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec());
        std::ptr::write_bytes(out.pbData, 0, out.cbData as usize);
        LocalFree(out.pbData.cast());
        bytes
    }

    pub(super) fn protect(plain: &[u8]) -> Result<Vec<u8>, String> {
        let input = blob(plain);
        let entropy = blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        // SAFETY: the input blobs point at live slices for the call, and the
        // output is owned by the call's LocalAlloc until `take` frees it.
        unsafe {
            if CryptProtectData(
                &input,
                std::ptr::null(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            ) == 0
            {
                return Err(format!(
                    "DPAPI refused to encrypt: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(take(out).to_vec())
        }
    }

    pub(super) fn unprotect(sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, String> {
        let input = blob(sealed);
        let entropy = blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        // SAFETY: as in `protect`.
        unsafe {
            if CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            ) == 0
            {
                return Err(format!(
                    "DPAPI refused to decrypt (another Windows user, another machine, or a changed file): {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(take(out))
        }
    }
}

#[cfg(not(windows))]
mod dpapi {
    use zeroize::Zeroizing;

    pub(super) fn protect(_plain: &[u8]) -> Result<Vec<u8>, String> {
        Err("DPAPI exists only on Windows".to_string())
    }

    pub(super) fn unprotect(_sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, String> {
        Err(
            "the file was sealed with Windows DPAPI and opens only for the same Windows user"
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// An in-memory credential store with the Windows blob limit.
    #[derive(Default)]
    struct MemoryKeyring {
        entries: Mutex<HashMap<String, Vec<u8>>>,
        calls: AtomicUsize,
        /// Returns this instead of what was stored, to fail verification.
        corrupt: bool,
    }

    impl Keyring for MemoryKeyring {
        fn get(&self, user: &str) -> Result<Option<Vec<u8>>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let found = self.entries.lock().unwrap().get(user).cloned();
            Ok(match found {
                Some(_) if self.corrupt && user.contains('#') => Some(b"other".to_vec()),
                found => found,
            })
        }

        fn set(&self, user: &str, secret: &[u8]) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if secret.len() > 2560 {
                return Err("TooLong".to_string());
            }
            self.entries
                .lock()
                .unwrap()
                .insert(user.to_string(), secret.to_vec());
            Ok(())
        }

        fn delete(&self, user: &str) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entries.lock().unwrap().remove(user);
            Ok(())
        }
    }

    struct FailingKeyring;

    impl Keyring for FailingKeyring {
        fn get(&self, _user: &str) -> Result<Option<Vec<u8>>, String> {
            Err("no secret service".to_string())
        }

        fn set(&self, _user: &str, _secret: &[u8]) -> Result<(), String> {
            Err("no secret service".to_string())
        }

        fn delete(&self, _user: &str) -> Result<(), String> {
            Err("no secret service".to_string())
        }
    }

    const TEST_COST: KdfCost = KdfCost {
        memory_kib: 64,
        iterations: 1,
        parallelism: 1,
    };

    fn vault(
        path: &Path,
        mode: StoreMode,
        default_location: bool,
        keyring: Arc<dyn Keyring>,
        sealed: Backend,
        passphrase: Option<&str>,
    ) -> Vault {
        Vault {
            path: path.to_path_buf(),
            mode,
            default_location,
            keyring,
            passphrase: Arc::new(Mutex::new(passphrase.map(|p| Unlocked {
                passphrase: Zeroizing::new(p.to_string()),
                keys: Vec::new(),
            }))),
            sealed,
            cost: TEST_COST,
        }
    }

    fn sample(secret: &str) -> Accounts {
        BTreeMap::from([(
            "gpt-oauth".to_string(),
            BTreeMap::from([(
                DEFAULT_ACCOUNT.to_string(),
                serde_json::json!({ "access": secret, "refresh": "rt" }),
            )]),
        )])
    }

    fn save(vault: &Vault, accounts: Accounts) -> Result<(), String> {
        let _lock = lock_store(&vault.path)?;
        let mut contents = vault.load_locked()?;
        contents.accounts = accounts;
        vault.save_locked(&contents, "gpt-oauth", DEFAULT_ACCOUNT)
    }

    #[test]
    fn modes_parse_from_the_setting() {
        for mode in [
            StoreMode::Auto,
            StoreMode::Keyring,
            StoreMode::EncryptedFile,
            StoreMode::Plaintext,
        ] {
            assert_eq!(StoreMode::parse(mode.as_str()), Ok(mode));
        }
        assert!(StoreMode::parse("file").is_err());
    }

    #[test]
    fn passphrase_file_round_trips_without_the_secret_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let keyring = Arc::new(MemoryKeyring::default());
        let store = vault(
            &path,
            StoreMode::EncryptedFile,
            true,
            keyring.clone(),
            Backend::Passphrase,
            Some("correct horse"),
        );
        save(&store, sample("very-secret-access")).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("very-secret-access"));
        assert_eq!(store.load().unwrap(), sample("very-secret-access"));
        assert_eq!(keyring.calls.load(Ordering::SeqCst), 0);
        // A second process with the same passphrase reads it.
        let other = vault(
            &path,
            StoreMode::EncryptedFile,
            true,
            keyring,
            Backend::Passphrase,
            Some("correct horse"),
        );
        assert_eq!(other.load().unwrap(), sample("very-secret-access"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn wrong_or_missing_passphrase_is_an_error_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let keyring: Arc<dyn Keyring> = Arc::new(MemoryKeyring::default());
        let make = |passphrase| {
            vault(
                &path,
                StoreMode::EncryptedFile,
                true,
                Arc::clone(&keyring),
                Backend::Passphrase,
                passphrase,
            )
        };
        save(&make(Some("right")), sample("a")).unwrap();
        let before = std::fs::read(&path).unwrap();
        let wrong = make(Some("wrong"));
        let error = wrong.load().unwrap_err();
        assert!(error.contains("passphrase is wrong"), "{error}");
        assert!(save(&wrong, sample("b")).is_err());
        let error = make(None).load().unwrap_err();
        assert!(error.contains("locked"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn tampered_passphrase_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let store = vault(
            &path,
            StoreMode::EncryptedFile,
            true,
            Arc::new(MemoryKeyring::default()),
            Backend::Passphrase,
            Some("pass"),
        );
        save(&store, sample("a")).unwrap();
        let original: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();

        let mut flipped = original.clone();
        let mut sealed = STANDARD
            .decode(flipped["sealed"].as_str().unwrap())
            .unwrap();
        sealed[0] ^= 1;
        flipped["sealed"] = Value::String(STANDARD.encode(sealed));
        std::fs::write(&path, serde_json::to_vec(&flipped).unwrap()).unwrap();
        assert!(store.load().unwrap_err().contains("changed"));

        // A changed header is authenticated too, even when the key still
        // matches (the key is cached for the original header only).
        let mut renonced = original.clone();
        let mut nonce = STANDARD
            .decode(renonced["nonce"].as_str().unwrap())
            .unwrap();
        nonce[0] ^= 1;
        renonced["nonce"] = Value::String(STANDARD.encode(nonce));
        std::fs::write(&path, serde_json::to_vec(&renonced).unwrap()).unwrap();
        assert!(store.load().unwrap_err().contains("changed"));

        let mut costly = original;
        costly["kdf"]["memory_kib"] = Value::from(u32::MAX);
        std::fs::write(&path, serde_json::to_vec(&costly).unwrap()).unwrap();
        assert!(store.load().unwrap_err().contains("does not accept"));
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_file_round_trips_and_refuses_a_changed_blob() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let store = vault(
            &path,
            StoreMode::EncryptedFile,
            true,
            Arc::new(MemoryKeyring::default()),
            Backend::Dpapi,
            None,
        );
        save(&store, sample("dpapi-secret")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("dpapi-secret"));
        assert!(text.contains("\"backend\": \"dpapi\""));
        assert_eq!(store.load().unwrap(), sample("dpapi-secret"));
        let mut file: Value = serde_json::from_str(&text).unwrap();
        let mut sealed = STANDARD.decode(file["sealed"].as_str().unwrap()).unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 1;
        file["sealed"] = Value::String(STANDARD.encode(sealed));
        std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
        assert!(store.load().unwrap_err().contains("cannot decrypt"));
    }

    #[test]
    fn large_secrets_are_split_and_old_parts_removed() {
        let keyring = MemoryKeyring::default();
        let large: Vec<u8> = (0..7000u32).map(|i| (i % 251) as u8).collect();
        put_secret(&keyring, "gpt-oauth/default", &large).unwrap();
        assert_eq!(keyring.entries.lock().unwrap().len(), 1 + 4);
        assert_eq!(
            get_secret(&keyring, "gpt-oauth/default")
                .unwrap()
                .unwrap()
                .to_vec(),
            large
        );
        put_secret(&keyring, "gpt-oauth/default", b"small").unwrap();
        assert_eq!(keyring.entries.lock().unwrap().len(), 1 + 1);
        assert_eq!(
            get_secret(&keyring, "gpt-oauth/default")
                .unwrap()
                .unwrap()
                .to_vec(),
            b"small"
        );
        let part = keyring
            .entries
            .lock()
            .unwrap()
            .keys()
            .find(|name| name.contains('#'))
            .cloned()
            .unwrap();
        keyring.entries.lock().unwrap().remove(&part);
        assert!(get_secret(&keyring, "gpt-oauth/default").is_err());
        delete_secret(&keyring, "gpt-oauth/default").unwrap();
        assert!(keyring.entries.lock().unwrap().is_empty());
    }

    fn write_legacy(path: &Path, secret: &str) -> Vec<u8> {
        let legacy = serde_json::to_vec_pretty(&serde_json::json!({
            "gpt-oauth": { "access": secret, "refresh": "rt" }
        }))
        .unwrap();
        std::fs::write(path, &legacy).unwrap();
        legacy
    }

    #[test]
    fn legacy_plaintext_moves_to_the_keyring_and_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        write_legacy(&path, "legacy-secret");
        let keyring = Arc::new(MemoryKeyring::default());
        let store = vault(
            &path,
            StoreMode::Auto,
            true,
            keyring.clone(),
            Backend::Passphrase,
            None,
        );
        assert_eq!(store.load().unwrap(), sample("legacy-secret"));
        let index = std::fs::read_to_string(&path).unwrap();
        assert!(!index.contains("legacy-secret"), "{index}");
        assert!(index.contains("\"backend\": \"keyring\""));
        assert!(keyring
            .entries
            .lock()
            .unwrap()
            .contains_key("gpt-oauth/default"));
        assert_eq!(store.load().unwrap(), sample("legacy-secret"));
        let leftovers = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn auto_falls_back_to_the_encrypted_file_when_the_keyring_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        write_legacy(&path, "legacy-secret");
        let store = vault(
            &path,
            StoreMode::Auto,
            true,
            Arc::new(FailingKeyring),
            Backend::Passphrase,
            Some("pass"),
        );
        assert_eq!(store.load().unwrap(), sample("legacy-secret"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"backend\": \"passphrase\""));
        assert!(!text.contains("legacy-secret"));
        save(&store, sample("next")).unwrap();
        assert_eq!(store.load().unwrap(), sample("next"));
    }

    #[test]
    fn explicit_keyring_mode_does_not_fall_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let legacy = write_legacy(&path, "legacy-secret");
        let store = vault(
            &path,
            StoreMode::Keyring,
            true,
            Arc::new(FailingKeyring),
            Backend::Passphrase,
            Some("pass"),
        );
        let error = store.load().unwrap_err();
        assert!(error.contains("left as it was"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), legacy);
    }

    #[test]
    fn a_move_that_does_not_read_back_keeps_the_plaintext_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let legacy = write_legacy(&path, "legacy-secret");
        let keyring = Arc::new(MemoryKeyring {
            corrupt: true,
            ..MemoryKeyring::default()
        });
        let store = vault(
            &path,
            StoreMode::Keyring,
            true,
            keyring.clone(),
            Backend::Passphrase,
            None,
        );
        assert!(store.load().unwrap_err().contains("left as it was"));
        assert_eq!(std::fs::read(&path).unwrap(), legacy);
        assert!(keyring.entries.lock().unwrap().is_empty());
    }

    #[test]
    fn auto_away_from_the_default_location_never_uses_the_keyring() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let keyring = Arc::new(MemoryKeyring::default());
        let store = vault(
            &path,
            StoreMode::Auto,
            false,
            keyring.clone(),
            Backend::Passphrase,
            Some("pass"),
        );
        save(&store, sample("a")).unwrap();
        assert_eq!(keyring.calls.load(Ordering::SeqCst), 0);
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("\"backend\": \"passphrase\""));
    }

    #[test]
    fn plaintext_only_when_chosen_and_auto_moves_it_away() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let keyring = Arc::new(MemoryKeyring::default());
        let plain = vault(
            &path,
            StoreMode::Plaintext,
            true,
            keyring.clone(),
            Backend::Passphrase,
            None,
        );
        save(&plain, sample("visible")).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("visible"));
        let auto = vault(
            &path,
            StoreMode::Auto,
            true,
            keyring.clone(),
            Backend::Passphrase,
            None,
        );
        assert_eq!(auto.load().unwrap(), sample("visible"));
        assert!(!std::fs::read_to_string(&path).unwrap().contains("visible"));
        // Choosing an encrypted file moves it out of the keyring and removes
        // the entries there.
        let sealed = vault(
            &path,
            StoreMode::EncryptedFile,
            true,
            keyring.clone(),
            Backend::Passphrase,
            Some("pass"),
        );
        assert_eq!(sealed.load().unwrap(), sample("visible"));
        assert!(keyring.entries.lock().unwrap().is_empty());
    }

    #[test]
    fn keyring_saves_touch_only_the_changed_account() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let keyring = Arc::new(MemoryKeyring::default());
        let store = vault(
            &path,
            StoreMode::Keyring,
            false,
            keyring.clone(),
            Backend::Passphrase,
            None,
        );
        let mut accounts = sample("one");
        accounts.insert(
            "claude-oauth".to_string(),
            BTreeMap::from([(
                DEFAULT_ACCOUNT.to_string(),
                serde_json::json!({"access": "c"}),
            )]),
        );
        {
            let _lock = lock_store(&path).unwrap();
            let mut contents = store.load_locked().unwrap();
            contents.accounts = accounts.clone();
            store
                .save_locked(&contents, "claude-oauth", DEFAULT_ACCOUNT)
                .unwrap();
        }
        assert_eq!(store.load().unwrap(), accounts);
        // Away from the default location, entries carry a prefix of their own.
        assert!(keyring
            .entries
            .lock()
            .unwrap()
            .keys()
            .all(|name| name.len() > 17 && name.as_bytes()[16] == b'/'));
        save(&store, {
            let mut changed = accounts.clone();
            changed.insert("gpt-oauth".to_string(), sample("two")["gpt-oauth"].clone());
            changed
        })
        .unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded["gpt-oauth"], sample("two")["gpt-oauth"]);
        assert_eq!(loaded["claude-oauth"], accounts["claude-oauth"]);
    }

    #[test]
    fn a_file_from_a_newer_format_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(&path, br#"{"format":"inocode-credentials","version":3}"#).unwrap();
        let store = vault(
            &path,
            StoreMode::Auto,
            true,
            Arc::new(MemoryKeyring::default()),
            Backend::Passphrase,
            None,
        );
        assert!(store.load().is_err());
        assert!(save(&store, sample("a")).is_err());
    }

    /// Stores, reads and deletes a dummy secret larger than one Windows
    /// credential blob in this machine's real OS credential store.
    #[test]
    #[ignore = "writes to the real OS credential store"]
    fn round_trips_a_dummy_secret_in_the_os_store_here() {
        let name = format!("selftest-{}/default", std::process::id());
        let dummy: Vec<u8> = (0..6000u32).map(|i| b'a' + (i % 26) as u8).collect();
        put_secret(&OsKeyring, &name, &dummy).expect("store");
        let read = get_secret(&OsKeyring, &name)
            .expect("read")
            .expect("present");
        assert_eq!(read.to_vec(), dummy);
        delete_secret(&OsKeyring, &name).expect("delete");
        assert!(get_secret(&OsKeyring, &name).expect("read").is_none());
        println!(
            "OS credential store: {} bytes stored in {} parts, read back and deleted",
            dummy.len(),
            dummy.len().div_ceil(CHUNK)
        );
    }
}
