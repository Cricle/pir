//! Credential storage backed by `~/.pi/agent/auth.json`.
//!
//! Rust equivalent of upstream `packages/coding-agent/src/core/auth-storage.ts`
//! (shape and semantics) using pir's own locking: one type-tagged credential
//! per provider, every write a serialized read-modify-write under an
//! in-process mutex plus a `<path>.lock` file (bounded acquire, stale-lock
//! takeover), and `0o600`/`0o700` permissions applied on creation on Unix.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::oauth::OAuthCredential;
use crate::ai::auth::CredentialStore;
use crate::ai::error::{AiError, AiResult};
use crate::ai::types::{ApiKeyCredential, Credential};

/// The stored credential union — the JSON shape of `auth.json`
/// (upstream `Credential = ApiKeyCredential | OAuthCredential`).
#[derive(Debug, Clone)]
pub enum StoredCredential {
    ApiKey(ApiKeyCredential),
    OAuth(OAuthCredential),
}

impl StoredCredential {
    /// Serialize with the envelope tag applied (see the manual
    /// `Serialize`/`Deserialize` impls below for the wire contract).
    fn to_json(&self) -> AiResult<serde_json::Value> {
        let mut value = match self {
            StoredCredential::ApiKey(credential) => {
                serde_json::to_value(credential).map_err(|error| {
                    AiError::Authentication(format!("failed to serialize credential: {error}"))
                })?
            }
            StoredCredential::OAuth(credential) => {
                serde_json::to_value(credential).map_err(|error| {
                    AiError::Authentication(format!("failed to serialize credential: {error}"))
                })?
            }
        };
        value["type"] = serde_json::Value::String(match self {
            StoredCredential::ApiKey(_) => "api_key".to_string(),
            StoredCredential::OAuth(_) => "oauth".to_string(),
        });
        Ok(value)
    }
}

impl Serialize for StoredCredential {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::Error as _;
        self.to_json()
            .map_err(S::Error::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for StoredCredential {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut value = serde_json::Value::deserialize(deserializer)?;
        let tag = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| serde::de::Error::missing_field("type"))?
            .to_string();
        match tag.as_str() {
            "api_key" => {
                // The api-key credential deserializes with its own `type`
                // field present.
                let credential: ApiKeyCredential =
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(StoredCredential::ApiKey(credential))
            }
            "oauth" => {
                // Strip the envelope tag before the flattened extra map (and
                // the payload) consume the object.
                value
                    .as_object_mut()
                    .ok_or_else(|| serde::de::Error::custom("credential must be an object"))?
                    .remove("type");
                let credential: OAuthCredential =
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(StoredCredential::OAuth(credential))
            }
            other => Err(serde::de::Error::unknown_variant(
                other,
                &["api_key", "oauth"],
            )),
        }
    }
}

impl From<&Credential> for StoredCredential {
    fn from(credential: &Credential) -> Self {
        let Credential::ApiKey(api_key) = credential;
        StoredCredential::ApiKey(api_key.clone())
    }
}

/// The credential type tag, without the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredCredentialType {
    ApiKey,
    OAuth,
}

impl StoredCredential {
    pub fn credential_type(&self) -> StoredCredentialType {
        match self {
            StoredCredential::ApiKey(_) => StoredCredentialType::ApiKey,
            StoredCredential::OAuth(_) => StoredCredentialType::OAuth,
        }
    }
}

const LOCK_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(20);
const LOCK_STALE_AFTER: Duration = Duration::from_secs(30);
/// Cap for the Windows-only transient-failure retry, kept separate from the
/// busy-lock wait (upstream auth-storage.ts `MAX_EPERM_RETRIES`): a real
/// permission error exhausts this quickly instead of waiting out the deadline.
const MAX_TRANSIENT_LOCK_RETRIES: u32 = 5;

/// The parsed `auth.json` map: provider id -> credential.
pub type AuthStorageData = BTreeMap<String, StoredCredential>;

pub fn parse_data(contents: &str) -> AiResult<AuthStorageData> {
    let contents = contents.trim_start_matches('\u{feff}');
    if contents.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    let parsed: serde_json::Value = serde_json::from_str(contents)
        .map_err(|error| AiError::Authentication(format!("Failed to read auth.json: {error}")))?;
    if !matches!(parsed, serde_json::Value::Object(_)) {
        return Err(AiError::Authentication(
            "Invalid auth.json: expected an object".to_string(),
        ));
    }
    serde_json::from_value::<AuthStorageData>(parsed)
        .map_err(|error| AiError::Authentication(format!("Invalid auth.json credential: {error}")))
}

fn serialize_data(data: &AuthStorageData) -> AiResult<String> {
    serde_json::to_string_pretty(data)
        .map(|json| json + "\n")
        .map_err(|error| AiError::Authentication(format!("failed to serialize auth.json: {error}")))
}

fn ensure_parent_dir(path: &Path) -> AiResult<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            #[cfg(unix)]
            {
                std::fs::create_dir_all(parent)?;
                let _ = set_dir_permissions(parent);
            }
            #[cfg(not(unix))]
            {
                std::fs::create_dir_all(parent)?;
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_dir_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(path)?;
    let mut permissions = metadata.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(path, permissions)
}

/// The default `~/.pi/agent/auth.json` path (respects `PIR_AGENT_DIR` /
/// `PI_CODING_AGENT_DIR`, mirroring upstream's `getAgentDir`).
///
/// Process-scoped callers (the auth CLI, provider discovery, the TUI's login
/// flow) keep using this. A live session must use [`auth_path_in`] with its
/// own resolved agent directory instead: re-resolving the environment here
/// would split one session across two directories after a mid-process change.
pub fn default_auth_path() -> PathBuf {
    auth_path_in(&crate::ai::providers::agent_dir())
}

/// `<agent_dir>/auth.json` for an EXPLICIT agent directory (D1): the caller
/// resolves the directory once (a session resolves it at build) and this
/// function never reads the process environment.
pub fn auth_path_in(agent_dir: &Path) -> PathBuf {
    agent_dir.join("auth.json")
}

/// File-backed credential storage implementing both the full stored-credential
/// view and the api-key-only [`CredentialStore`] contract. All writes run
/// under the exclusive `<path>.lock` file, which serializes in-process and
/// cross-process writers alike.
pub struct FileAuthStorage {
    auth_path: PathBuf,
}

impl FileAuthStorage {
    /// Open storage at `auth_path` (creating an empty `{}` on first write).
    pub fn new(auth_path: PathBuf) -> Self {
        Self { auth_path }
    }

    /// Open storage at the default `~/.pi/agent/auth.json` path.
    pub fn default_storage() -> Self {
        Self::new(default_auth_path())
    }

    /// Open storage at `<agent_dir>/auth.json` for an explicit agent directory
    /// (D1): the live-session form of [`Self::default_storage`], which
    /// re-resolves the process environment on every call.
    pub fn in_agent_dir(agent_dir: &Path) -> Self {
        Self::new(auth_path_in(agent_dir))
    }

    /// The backing file path.
    pub fn path(&self) -> &Path {
        &self.auth_path
    }

    fn ensure_file(&self) -> AiResult<()> {
        ensure_parent_dir(&self.auth_path)?;
        if !self.auth_path.exists() {
            crate::config::settings::write_private_text_atomic(&self.auth_path, "{}")?;
        }
        Ok(())
    }

    /// Acquire the cross-process lock file with a blocking retry loop (sync
    /// callers). Bounded acquire with stale-lock takeover after
    /// [`LOCK_STALE_AFTER`].
    fn acquire_file_lock_sync(&self) -> AiResult<LockGuard> {
        let lock_path = self.lock_path();
        let deadline = std::time::Instant::now() + LOCK_ACQUIRE_TIMEOUT;
        let mut transient_retries = 0u32;
        loop {
            match try_create_lock(&lock_path) {
                Ok(Some(guard)) => return Ok(guard),
                Ok(None) => {
                    if stale_lock(&lock_path) {
                        let _ = std::fs::remove_file(&lock_path);
                        continue;
                    }
                    if std::time::Instant::now() >= deadline {
                        return Err(AiError::Authentication(format!(
                            "Failed to acquire auth storage lock after {}s: {}",
                            LOCK_ACQUIRE_TIMEOUT.as_secs(),
                            lock_path.display()
                        )));
                    }
                    std::thread::sleep(LOCK_RETRY_DELAY);
                }
                Err(error)
                    if is_transient_lock_error(&error)
                        && transient_retries < MAX_TRANSIENT_LOCK_RETRIES =>
                {
                    transient_retries += 1;
                    std::thread::sleep(LOCK_RETRY_DELAY);
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// [`Self::acquire_file_lock_sync`] for async contexts: the retry delay
    /// sleeps on the runtime instead of blocking the worker thread.
    async fn acquire_file_lock_async(&self) -> AiResult<LockGuard> {
        let lock_path = self.lock_path();
        let deadline = std::time::Instant::now() + LOCK_ACQUIRE_TIMEOUT;
        let mut transient_retries = 0u32;
        loop {
            match try_create_lock(&lock_path) {
                Ok(Some(guard)) => return Ok(guard),
                Ok(None) => {
                    if stale_lock(&lock_path) {
                        let _ = std::fs::remove_file(&lock_path);
                        continue;
                    }
                    if std::time::Instant::now() >= deadline {
                        return Err(AiError::Authentication(format!(
                            "Failed to acquire auth storage lock after {}s: {}",
                            LOCK_ACQUIRE_TIMEOUT.as_secs(),
                            lock_path.display()
                        )));
                    }
                    tokio::time::sleep(LOCK_RETRY_DELAY).await;
                }
                Err(error)
                    if is_transient_lock_error(&error)
                        && transient_retries < MAX_TRANSIENT_LOCK_RETRIES =>
                {
                    transient_retries += 1;
                    tokio::time::sleep(LOCK_RETRY_DELAY).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn lock_path(&self) -> PathBuf {
        PathBuf::from(format!("{}.lock", self.auth_path.display()))
    }

    fn read_data(&self) -> AiResult<AuthStorageData> {
        match std::fs::read_to_string(&self.auth_path) {
            Ok(contents) => parse_data(&contents),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(AiError::Authentication(format!(
                "Failed to read auth.json: {error}"
            ))),
        }
    }

    fn write_data(&self, data: &AuthStorageData) -> AiResult<()> {
        self.ensure_file()?;
        let contents = serialize_data(data)?;
        // Credentials must never be exposed as a truncated/empty file to a
        // lock-free reader (or lose every stored credential on a crash):
        // same-directory temp + fsync + rename, 0600 on creation.
        crate::config::settings::write_private_text_atomic(&self.auth_path, &contents)?;
        Ok(())
    }

    /// Read the stored credential for one provider, possibly expired
    /// (upstream `CredentialStore.read`). Lock-free: readers see either the
    /// old or the new file contents.
    ///
    /// Stored api keys support the models.json value templates (`$ENV` /
    /// `${ENV}`) resolved at READ time against the credential's own env block
    /// (upstream auth-storage.ts:263-266,444-446); `!command` values are
    /// returned raw like upstream's read path, and an unresolvable template
    /// yields a credential with no key.
    pub fn read_stored(&self, provider_id: &str) -> AiResult<Option<StoredCredential>> {
        Ok(self
            .read_data()?
            .remove(provider_id)
            .map(resolve_stored_credential_templates))
    }

    /// Serialized read-modify-write; the only write path. `f` sees the current
    /// credential and returns the replacement (`None` leaves the entry
    /// unchanged but still reports the post-write credential). Async because
    /// OAuth refresh may run inside the lock, mirroring upstream.
    pub async fn modify<F, Fut>(
        &self,
        provider_id: &str,
        f: F,
    ) -> AiResult<Option<StoredCredential>>
    where
        F: FnOnce(Option<StoredCredential>) -> Fut,
        Fut: std::future::Future<Output = AiResult<Option<StoredCredential>>>,
    {
        let _file_lock = self.acquire_file_lock_async().await?;
        let data = self.read_data()?;
        let current = data.get(provider_id).cloned();
        let next = f(current).await?;
        let mut data = self.read_data()?;
        let post = match next {
            Some(credential) => {
                data.insert(provider_id.to_string(), credential.clone());
                Some(credential)
            }
            None => data.get(provider_id).cloned(),
        };
        self.write_data(&data)?;
        Ok(post)
    }

    /// Store a credential (login / refresh persistence).
    pub fn store(&self, provider_id: &str, credential: StoredCredential) -> AiResult<()> {
        let _file_lock = self.acquire_file_lock_sync()?;
        let mut data = self.read_data()?;
        data.insert(provider_id.to_string(), credential);
        self.write_data(&data)
    }

    /// [`Self::store`] with the async lock acquisition: the bounded retry
    /// delay sleeps on the runtime instead of blocking the caller's thread
    /// (upstream parity with [`Self::modify`]). Same on-disk semantics.
    pub async fn store_async(
        &self,
        provider_id: &str,
        credential: StoredCredential,
    ) -> AiResult<()> {
        let _file_lock = self.acquire_file_lock_async().await?;
        let mut data = self.read_data()?;
        data.insert(provider_id.to_string(), credential);
        self.write_data(&data)
    }

    /// Remove a credential (logout).
    pub fn delete(&self, provider_id: &str) -> AiResult<()> {
        let _file_lock = self.acquire_file_lock_sync()?;
        let mut data = self.read_data()?;
        data.remove(provider_id);
        self.write_data(&data)
    }

    /// [`Self::delete`] with the async lock acquisition (see
    /// [`Self::store_async`]); same on-disk semantics.
    pub async fn delete_async(&self, provider_id: &str) -> AiResult<()> {
        let _file_lock = self.acquire_file_lock_async().await?;
        let mut data = self.read_data()?;
        data.remove(provider_id);
        self.write_data(&data)
    }

    /// List stored credential metadata without exposing secrets.
    pub fn list_stored(&self) -> AiResult<Vec<(String, StoredCredentialType)>> {
        Ok(self
            .read_data()?
            .into_iter()
            .map(|(provider_id, credential)| (provider_id, credential.credential_type()))
            .collect())
    }
}

/// RAII release for the cross-process lock file.
struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Try to create the lock file exclusively; `None` when it already exists.
fn try_create_lock(lock_path: &Path) -> std::io::Result<Option<LockGuard>> {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(lock_path)
    {
        Ok(_) => Ok(Some(LockGuard {
            path: lock_path.to_path_buf(),
        })),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(None),
        Err(error) => Err(error),
    }
}

/// A failed lock create worth a short bounded retry: on Windows a
/// just-removed `<path>.lock` can stay delete-pending while another process
/// (antivirus, indexer, another pir) still holds a handle, and `create_new`
/// fails with ERROR_ACCESS_DENIED (EPERM in libuv terms) until that handle
/// closes (upstream auth-storage.ts `getRetryableLockErrorCode`).
fn is_transient_lock_error(error: &std::io::Error) -> bool {
    cfg!(windows) && error.kind() == ErrorKind::PermissionDenied
}

/// Whether an existing lock file is older than the stale window (its owner
/// process died).
fn stale_lock(lock_path: &Path) -> bool {
    std::fs::metadata(lock_path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .map(|modified| modified.elapsed().unwrap_or(Duration::ZERO) > LOCK_STALE_AFTER)
        .unwrap_or(false)
}

/// Resolve the `$ENV` / `${ENV}` templates of a stored api key at READ time
/// (upstream auth-storage.ts:263-266,444-446): resolution runs against the
/// credential's own env block with the process environment as fallback.
/// OAuth entries pass through untouched; `!command` values are returned raw
/// like upstream's read path (commands resolve at use, not read), and an
/// unresolvable template yields a credential with no key.
fn resolve_stored_credential_templates(credential: StoredCredential) -> StoredCredential {
    let StoredCredential::ApiKey(mut credential) = credential else {
        return credential;
    };
    let Some(key) = credential.key.as_deref().filter(|key| !key.is_empty()) else {
        return StoredCredential::ApiKey(credential);
    };
    if crate::ai::providers::config_value::is_command_config_value(key) {
        return StoredCredential::ApiKey(credential);
    }
    credential.key =
        crate::ai::providers::config_value::resolve_config_value(key, credential.env.as_ref());
    StoredCredential::ApiKey(credential)
}

impl CredentialStore for FileAuthStorage {
    /// The api-key view: OAuth entries resolve to `None` here; OAuth handling
    /// goes through [`FileAuthStorage::read_stored`] and the registry's OAuth
    /// resolution so a stored OAuth credential owns the provider without an
    /// env fallback (upstream resolve.ts).
    fn read(&self, provider_id: &str) -> AiResult<Option<Credential>> {
        Ok(match self.read_stored(provider_id)? {
            Some(StoredCredential::ApiKey(credential)) => Some(Credential::ApiKey(credential)),
            _ => None,
        })
    }

    fn list(&self) -> AiResult<Vec<crate::ai::auth::CredentialInfo>> {
        Ok(self
            .list_stored()?
            .into_iter()
            .map(
                |(provider_id, credential_type)| crate::ai::auth::CredentialInfo {
                    provider_id,
                    credential_type: match credential_type {
                        StoredCredentialType::ApiKey => crate::ai::types::CredentialType::ApiKey,
                        StoredCredentialType::OAuth => crate::ai::types::CredentialType::ApiKey,
                    },
                },
            )
            .collect())
    }

    fn modify(
        &self,
        provider_id: &str,
        f: &mut dyn FnMut(Option<&Credential>) -> AiResult<Option<Credential>>,
    ) -> AiResult<Option<Credential>> {
        let _file_lock = self.acquire_file_lock_sync()?;
        let mut data = self.read_data()?;
        let current = data.get(provider_id).and_then(|stored| match stored {
            StoredCredential::ApiKey(credential) => Some(credential.clone()),
            StoredCredential::OAuth(_) => None,
        });
        let current_credential = current.as_ref().map(|c| Credential::ApiKey(c.clone()));
        let next = f(current_credential.as_ref())?;
        let post = match next {
            Some(Credential::ApiKey(credential)) => {
                data.insert(
                    provider_id.to_string(),
                    StoredCredential::ApiKey(credential.clone()),
                );
                Some(credential)
            }
            // `None` leaves the entry unchanged per the trait contract; the
            // post-write credential is whatever is stored now.
            None => current,
        };
        self.write_data(&data)?;
        Ok(post.map(Credential::ApiKey))
    }

    fn delete(&self, provider_id: &str) -> AiResult<()> {
        FileAuthStorage::delete(self, provider_id)
    }
}
