use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::Serialize;
use uuid::Uuid;

use crate::auth::{generate_token, hash_token};
use crate::protocol::{now_ms, SessionDescriptor, SessionRow};

const USERS: TableDefinition<&str, &[u8]> = TableDefinition::new("users");
const USERS_BY_NAME: TableDefinition<&str, &str> = TableDefinition::new("users_by_name");
const TOKENS: TableDefinition<&str, &[u8]> = TableDefinition::new("tokens");
const TOKENS_BY_ID: TableDefinition<&str, &[u8]> = TableDefinition::new("tokens_by_id");
const TOKENS_BY_USER: TableDefinition<&str, &str> = TableDefinition::new("tokens_by_user");
const DEVICES: TableDefinition<&str, &[u8]> = TableDefinition::new("devices");
const DEVICES_BY_USER: TableDefinition<&str, &str> = TableDefinition::new("devices_by_user");
const SESSIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("sessions");
const SESSIONS_BY_USER: TableDefinition<&str, &str> = TableDefinition::new("sessions_by_user");
const SESSIONS_BY_USER_UPDATED: TableDefinition<&str, &str> =
    TableDefinition::new("sessions_by_user_updated");
const SESSIONS_BY_DEVICE: TableDefinition<&str, &str> = TableDefinition::new("sessions_by_device");
const AUDIT: TableDefinition<&str, &[u8]> = TableDefinition::new("audit");

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserRow {
    pub id: String,
    pub username: String,
    pub display_name: Option<String>,
    pub created_at: i64,
    pub disabled_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenRow {
    pub id: String,
    pub user_id: String,
    pub kind: String,
    pub name: Option<String>,
    pub token_hash: String,
    pub device_id: Option<String>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRow {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub platform: Option<String>,
    pub created_at: i64,
    pub last_seen_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

#[derive(Clone)]
pub struct Store {
    db: Arc<Database>,
    pub data_dir: PathBuf,
    pub pepper: String,
}

impl Store {
    pub fn open(data_dir: impl AsRef<Path>) -> anyhow::Result<Self> {
        let data_dir = data_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&data_dir)?;
        let secret_path = data_dir.join("secret");
        let pepper = std::env::var("PIR_PHONE_SERVER_PEPPER")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .or_else(|| {
                std::fs::read_to_string(&secret_path)
                    .ok()
                    .map(|value| value.trim().to_owned())
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or_else(|| {
                let secret = generate_token();
                let _ = std::fs::write(&secret_path, &secret);
                set_private_permissions(&secret_path);
                secret
            });
        if pepper.len() < 32 {
            anyhow::bail!("server secret is too short: {}", secret_path.display());
        }
        let db = Database::create(data_dir.join("phone.redb"))
            .with_context(|| format!("open {}", data_dir.join("phone.redb").display()))?;
        let store = Self {
            db: Arc::new(db),
            data_dir,
            pepper,
        };
        store.init_tables()?;
        store.rebuild_session_updated_index()?;
        Ok(store)
    }

    fn init_tables(&self) -> anyhow::Result<()> {
        let txn = self.db.begin_write()?;
        {
            txn.open_table(USERS)?;
            txn.open_table(USERS_BY_NAME)?;
            txn.open_table(TOKENS)?;
            txn.open_table(TOKENS_BY_ID)?;
            txn.open_table(TOKENS_BY_USER)?;
            txn.open_table(DEVICES)?;
            txn.open_table(DEVICES_BY_USER)?;
            txn.open_table(SESSIONS)?;
            txn.open_table(SESSIONS_BY_USER)?;
            txn.open_table(SESSIONS_BY_USER_UPDATED)?;
            txn.open_table(SESSIONS_BY_DEVICE)?;
            txn.open_table(AUDIT)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Backfill the updated-at index for databases created before the index
    /// existed (or after an interrupted write). Idempotent.
    fn rebuild_session_updated_index(&self) -> anyhow::Result<()> {
        let read = self.db.begin_read()?;
        let existing = read.open_table(SESSIONS_BY_USER_UPDATED)?;
        if existing.iter()?.next().is_some() {
            return Ok(());
        }
        drop(existing);
        let sessions = read.open_table(SESSIONS)?;
        let mut rows = Vec::new();
        for item in sessions.iter()? {
            let (_, value) = item?;
            rows.push(decode_session(value.value())?);
        }
        drop(sessions);
        drop(read);
        let txn = self.db.begin_write()?;
        {
            let mut index = txn.open_table(SESSIONS_BY_USER_UPDATED)?;
            for row in rows {
                index.insert(
                    session_updated_key(&row.user_id, row.updated_at, &row.id).as_str(),
                    row.id.as_str(),
                )?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    pub fn create_user(
        &self,
        username: &str,
        display_name: Option<&str>,
    ) -> anyhow::Result<UserRow> {
        let read = self.db.begin_read()?;
        let names = read.open_table(USERS_BY_NAME)?;
        if names.get(username)?.is_some() {
            anyhow::bail!("user already exists: {username}");
        }
        drop(names);
        drop(read);

        let row = UserRow {
            id: Uuid::new_v4().to_string(),
            username: username.to_owned(),
            display_name: display_name.map(str::to_owned),
            created_at: now_ms(),
            disabled_at: None,
        };
        let txn = self.db.begin_write()?;
        {
            let mut users = txn.open_table(USERS)?;
            let mut names = txn.open_table(USERS_BY_NAME)?;
            let bytes = serde_json::to_vec(&row)?;
            users.insert(row.id.as_str(), bytes.as_slice())?;
            names.insert(row.username.as_str(), row.id.as_str())?;
        }
        txn.commit()?;
        Ok(row)
    }

    pub fn find_user_by_id(&self, id: &str) -> anyhow::Result<Option<UserRow>> {
        let read = self.db.begin_read()?;
        let table = read.open_table(USERS)?;
        let Some(value) = table.get(id)? else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_slice(value.value())?))
    }

    pub fn find_user_by_username(&self, username: &str) -> anyhow::Result<Option<UserRow>> {
        let read = self.db.begin_read()?;
        let names = read.open_table(USERS_BY_NAME)?;
        let Some(id) = names.get(username)? else {
            return Ok(None);
        };
        let id = id.value().to_owned();
        drop(names);
        drop(read);
        self.find_user_by_id(&id)
    }

    pub fn count_users(&self) -> anyhow::Result<u64> {
        let read = self.db.begin_read()?;
        let table = read.open_table(USERS)?;
        Ok(table.len()?)
    }

    pub fn create_token(
        &self,
        user_id: &str,
        kind: &str,
        name: Option<&str>,
        device_id: Option<&str>,
    ) -> anyhow::Result<(String, TokenRow)> {
        let plaintext = generate_token();
        let row = TokenRow {
            id: Uuid::new_v4().to_string(),
            user_id: user_id.to_owned(),
            kind: kind.to_owned(),
            name: name.map(str::to_owned),
            token_hash: hash_token(&self.pepper, &plaintext),
            device_id: device_id.map(str::to_owned),
            created_at: now_ms(),
            last_used_at: None,
            revoked_at: None,
        };
        let bytes = serde_json::to_vec(&row)?;
        let user_key = format!("{user_id}\0{}", row.id);
        let txn = self.db.begin_write()?;
        {
            let mut tokens = txn.open_table(TOKENS)?;
            let mut by_id = txn.open_table(TOKENS_BY_ID)?;
            let mut by_user = txn.open_table(TOKENS_BY_USER)?;
            tokens.insert(row.token_hash.as_str(), bytes.as_slice())?;
            by_id.insert(row.id.as_str(), bytes.as_slice())?;
            by_user.insert(user_key.as_str(), user_id)?;
        }
        txn.commit()?;
        Ok((plaintext, row))
    }

    pub fn find_token_by_hash(&self, hash: &str) -> anyhow::Result<Option<TokenRow>> {
        let read = self.db.begin_read()?;
        let table = read.open_table(TOKENS)?;
        let Some(value) = table.get(hash)? else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_slice(value.value())?))
    }

    pub fn touch_token(&self, token_id: &str) -> anyhow::Result<()> {
        let read = self.db.begin_read()?;
        let table = read.open_table(TOKENS_BY_ID)?;
        let Some(value) = table.get(token_id)? else {
            return Ok(());
        };
        let mut row: TokenRow = serde_json::from_slice(value.value())?;
        drop(value);
        drop(table);
        drop(read);
        row.last_used_at = Some(now_ms());
        self.put_token(&row)
    }

    fn put_token(&self, row: &TokenRow) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec(row)?;
        let txn = self.db.begin_write()?;
        {
            // Authentication reads `TOKENS` by hash; admin listings read
            // `TOKENS_BY_ID`. Both copies must change or a revoked token keeps
            // authenticating.
            let mut tokens = txn.open_table(TOKENS)?;
            tokens.insert(row.token_hash.as_str(), bytes.as_slice())?;
            let mut by_id = txn.open_table(TOKENS_BY_ID)?;
            by_id.insert(row.id.as_str(), bytes.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn revoke_token(&self, user_id: &str, token_id: &str) -> anyhow::Result<bool> {
        let read = self.db.begin_read()?;
        let table = read.open_table(TOKENS_BY_ID)?;
        let Some(value) = table.get(token_id)? else {
            return Ok(false);
        };
        let mut row: TokenRow = serde_json::from_slice(value.value())?;
        drop(value);
        drop(table);
        drop(read);
        if row.user_id != user_id || row.revoked_at.is_some() {
            return Ok(false);
        }
        row.revoked_at = Some(now_ms());
        self.put_token(&row)?;
        Ok(true)
    }

    pub fn list_tokens(&self, user_id: &str) -> anyhow::Result<Vec<TokenRow>> {
        let read = self.db.begin_read()?;
        let index = read.open_table(TOKENS_BY_USER)?;
        let tokens = read.open_table(TOKENS_BY_ID)?;
        let prefix = format!("{user_id}\0");
        let upper = format!("{prefix}\u{10FFFF}");
        let mut rows = Vec::new();
        // `TOKENS_BY_USER` values are only a membership marker; the token id
        // is the key suffix. Reading the values used to return the user id and
        // silently produced an empty list, which also made device revocation
        // skip every token.
        for item in index.range::<&str>(prefix.as_str()..upper.as_str())? {
            let (key, _) = item?;
            let Some(id) = key.value().strip_prefix(&prefix) else {
                continue;
            };
            if let Some(value) = tokens.get(id)? {
                rows.push(serde_json::from_slice(value.value())?);
            }
        }
        rows.sort_by_key(|row: &TokenRow| std::cmp::Reverse(row.created_at));
        Ok(rows)
    }

    pub fn create_device(
        &self,
        user_id: &str,
        name: &str,
        platform: Option<&str>,
    ) -> anyhow::Result<DeviceRow> {
        let row = DeviceRow {
            id: Uuid::new_v4().to_string(),
            user_id: user_id.to_owned(),
            name: name.to_owned(),
            platform: platform.map(str::to_owned),
            created_at: now_ms(),
            last_seen_at: None,
            revoked_at: None,
        };
        let bytes = serde_json::to_vec(&row)?;
        let key = format!("{user_id}\0{}", row.id);
        let txn = self.db.begin_write()?;
        {
            let mut devices = txn.open_table(DEVICES)?;
            let mut by_user = txn.open_table(DEVICES_BY_USER)?;
            devices.insert(row.id.as_str(), bytes.as_slice())?;
            by_user.insert(key.as_str(), row.id.as_str())?;
        }
        txn.commit()?;
        Ok(row)
    }

    pub fn find_device(&self, user_id: &str, device_id: &str) -> anyhow::Result<Option<DeviceRow>> {
        let Some(row) = self.find_device_by_id(device_id)? else {
            return Ok(None);
        };
        if row.user_id != user_id {
            return Ok(None);
        }
        Ok(Some(row))
    }

    pub fn find_device_by_id(&self, device_id: &str) -> anyhow::Result<Option<DeviceRow>> {
        let read = self.db.begin_read()?;
        let table = read.open_table(DEVICES)?;
        let Some(value) = table.get(device_id)? else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_slice(value.value())?))
    }

    /// Register an agent-reported device on first connect. Device tokens may be
    /// unbound (the agent generates its own stable device id), so the relay
    /// creates the device row on the first hello instead of requiring a
    /// pre-created device.
    pub fn ensure_device(
        &self,
        user_id: &str,
        device_id: &str,
        name: &str,
        platform: Option<&str>,
    ) -> anyhow::Result<DeviceRow> {
        let now = now_ms();
        let mut row = if let Some(existing) = self.find_device_by_id(device_id)? {
            if existing.user_id != user_id {
                anyhow::bail!("device id already belongs to another user");
            }
            existing
        } else {
            DeviceRow {
                id: device_id.to_owned(),
                user_id: user_id.to_owned(),
                name: name.to_owned(),
                platform: platform.map(str::to_owned),
                created_at: now,
                last_seen_at: None,
                revoked_at: None,
            }
        };
        if row.revoked_at.is_some() {
            anyhow::bail!("device is revoked");
        }
        row.name = name.to_owned();
        if platform.is_some() {
            row.platform = platform.map(str::to_owned);
        }
        row.last_seen_at = Some(now);
        let bytes = serde_json::to_vec(&row)?;
        let key = format!("{user_id}\0{}", row.id);
        let txn = self.db.begin_write()?;
        {
            let mut devices = txn.open_table(DEVICES)?;
            let mut by_user = txn.open_table(DEVICES_BY_USER)?;
            devices.insert(row.id.as_str(), bytes.as_slice())?;
            by_user.insert(key.as_str(), row.id.as_str())?;
        }
        txn.commit()?;
        Ok(row)
    }

    pub fn list_devices(&self, user_id: &str) -> anyhow::Result<Vec<DeviceRow>> {
        let read = self.db.begin_read()?;
        let index = read.open_table(DEVICES_BY_USER)?;
        let ids = prefix_values(&index, &format!("{user_id}\0"))?;
        let devices = read.open_table(DEVICES)?;
        let mut rows = Vec::new();
        for id in ids {
            if let Some(value) = devices.get(id.as_str())? {
                rows.push(serde_json::from_slice(value.value())?);
            }
        }
        rows.sort_by_key(|row: &DeviceRow| row.created_at);
        Ok(rows)
    }

    pub fn revoke_device(&self, user_id: &str, device_id: &str) -> anyhow::Result<bool> {
        let Some(mut device) = self.find_device(user_id, device_id)? else {
            return Ok(false);
        };
        if device.revoked_at.is_some() {
            return Ok(false);
        }
        device.revoked_at = Some(now_ms());
        let txn = self.db.begin_write()?;
        {
            let mut devices = txn.open_table(DEVICES)?;
            let bytes = serde_json::to_vec(&device)?;
            devices.insert(device.id.as_str(), bytes.as_slice())?;
        }
        txn.commit()?;
        for token in self.list_tokens(user_id)? {
            if token.device_id.as_deref() == Some(device_id) && token.revoked_at.is_none() {
                let mut token = token;
                token.revoked_at = Some(now_ms());
                self.put_token(&token)?;
            }
        }
        Ok(true)
    }

    pub fn touch_device(&self, device_id: &str) -> anyhow::Result<()> {
        let read = self.db.begin_read()?;
        let table = read.open_table(DEVICES)?;
        let Some(value) = table.get(device_id)? else {
            return Ok(());
        };
        let mut row: DeviceRow = serde_json::from_slice(value.value())?;
        drop(value);
        drop(table);
        drop(read);
        row.last_seen_at = Some(now_ms());
        let bytes = serde_json::to_vec(&row)?;
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(DEVICES)?;
            table.insert(row.id.as_str(), bytes.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn count_devices(&self) -> anyhow::Result<u64> {
        let read = self.db.begin_read()?;
        let table = read.open_table(DEVICES)?;
        Ok(table.len()?)
    }

    pub fn upsert_session(
        &self,
        user_id: &str,
        device_id: &str,
        descriptor: &SessionDescriptor,
        online: bool,
    ) -> anyhow::Result<SessionRow> {
        let id = format!("{device_id}:{}", descriptor.id);
        // Use the unfiltered row so a soft-deleted session cannot be
        // resurrected by a later status frame from the agent.
        let existing = self.get_session_any(&id)?;
        let previous_updated = existing.as_ref().map(|row| row.updated_at);
        let now = now_ms();
        let model = descriptor.model.as_ref();
        let previous = existing.as_ref();
        let row = SessionRow {
            id: id.clone(),
            user_id: user_id.to_owned(),
            device_id: device_id.to_owned(),
            local_id: descriptor.id.clone(),
            session_id: descriptor
                .session_id
                .clone()
                .or_else(|| previous.and_then(|row| row.session_id.clone())),
            has_pending_ui_request: descriptor
                .has_pending_ui_request
                .or_else(|| previous.and_then(|row| row.has_pending_ui_request)),
            last_activity_at: descriptor
                .last_activity_at
                .or_else(|| previous.and_then(|row| row.last_activity_at)),
            label: descriptor
                .label
                .clone()
                .or_else(|| previous.and_then(|row| row.label.clone())),
            kind: descriptor
                .kind
                .clone()
                .or_else(|| previous.map(|row| row.kind.clone()))
                .unwrap_or_else(|| "parent".to_owned()),
            cwd: descriptor
                .cwd
                .clone()
                .or_else(|| previous.and_then(|row| row.cwd.clone())),
            model_provider: model
                .map(|model| model.provider.clone())
                .or_else(|| previous.and_then(|row| row.model_provider.clone())),
            model_id: model
                .map(|model| model.id.clone())
                .or_else(|| previous.and_then(|row| row.model_id.clone())),
            model_name: model
                .and_then(|model| model.name.clone())
                .or_else(|| previous.and_then(|row| row.model_name.clone())),
            thinking_level: descriptor
                .thinking_level
                .clone()
                .or_else(|| previous.and_then(|row| row.thinking_level.clone())),
            is_streaming: descriptor
                .is_streaming
                .unwrap_or_else(|| previous.is_some_and(|row| row.is_streaming)),
            is_compacting: descriptor
                .is_compacting
                .unwrap_or_else(|| previous.is_some_and(|row| row.is_compacting)),
            message_count: descriptor
                .message_count
                .unwrap_or_else(|| previous.map(|row| row.message_count).unwrap_or(0)),
            last_error: descriptor
                .last_error
                .clone()
                .or_else(|| previous.and_then(|row| row.last_error.clone())),
            online,
            created_at: previous.map(|row| row.created_at).unwrap_or(now),
            updated_at: now,
            last_seen_at: Some(now),
            deleted_at: previous.and_then(|row| row.deleted_at),
        };
        let bytes = encode_session(&row)?;
        let user_key = format!("{user_id}\0{id}");
        let device_key = format!("{device_id}\0{id}");
        let updated_key = session_updated_key(user_id, now, &id);
        let previous_updated_key =
            previous_updated.map(|updated| session_updated_key(user_id, updated, &id));
        let txn = self.db.begin_write()?;
        {
            let mut sessions = txn.open_table(SESSIONS)?;
            let mut by_user = txn.open_table(SESSIONS_BY_USER)?;
            let mut by_user_updated = txn.open_table(SESSIONS_BY_USER_UPDATED)?;
            let mut by_device = txn.open_table(SESSIONS_BY_DEVICE)?;
            sessions.insert(id.as_str(), bytes.as_slice())?;
            by_user.insert(user_key.as_str(), id.as_str())?;
            if let Some(previous) = previous_updated_key {
                by_user_updated.remove(previous.as_str())?;
            }
            by_user_updated.insert(updated_key.as_str(), id.as_str())?;
            by_device.insert(device_key.as_str(), id.as_str())?;
        }
        txn.commit()?;
        Ok(row)
    }

    /// Batch variant used by agent hello and load tests. One write transaction
    /// instead of one per session.
    pub fn upsert_sessions_batch(
        &self,
        user_id: &str,
        device_id: &str,
        descriptors: &[SessionDescriptor],
        online: bool,
    ) -> anyhow::Result<Vec<SessionRow>> {
        let now = now_ms();
        let read = self.db.begin_read()?;
        let sessions = read.open_table(SESSIONS)?;
        let mut rows = Vec::with_capacity(descriptors.len());
        let mut previous = Vec::with_capacity(descriptors.len());
        for descriptor in descriptors {
            let id = format!("{device_id}:{}", descriptor.id);
            let existing = sessions
                .get(id.as_str())?
                .map(|value| decode_session(value.value()))
                .transpose()?;
            let created_at = existing.as_ref().map(|row| row.created_at).unwrap_or(now);
            let deleted_at = existing.as_ref().and_then(|row| row.deleted_at);
            previous.push(existing.map(|row| row.updated_at));
            let mut row =
                build_session_row(user_id, device_id, descriptor, online, created_at, now);
            row.deleted_at = deleted_at;
            rows.push(row);
        }
        drop(sessions);
        drop(read);

        let txn = self.db.begin_write()?;
        {
            let mut sessions = txn.open_table(SESSIONS)?;
            let mut by_user = txn.open_table(SESSIONS_BY_USER)?;
            let mut by_user_updated = txn.open_table(SESSIONS_BY_USER_UPDATED)?;
            let mut by_device = txn.open_table(SESSIONS_BY_DEVICE)?;
            for (row, previous_updated) in rows.iter().zip(previous) {
                let bytes = encode_session(row)?;
                sessions.insert(row.id.as_str(), bytes.as_slice())?;
                by_user.insert(format!("{user_id}\0{}", row.id).as_str(), row.id.as_str())?;
                if let Some(previous_updated) = previous_updated {
                    by_user_updated
                        .remove(session_updated_key(user_id, previous_updated, &row.id).as_str())?;
                }
                by_user_updated.insert(
                    session_updated_key(user_id, row.updated_at, &row.id).as_str(),
                    row.id.as_str(),
                )?;
                by_device.insert(format!("{device_id}\0{}", row.id).as_str(), row.id.as_str())?;
            }
        }
        txn.commit()?;
        Ok(rows)
    }

    pub fn get_session(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> anyhow::Result<Option<SessionRow>> {
        let Some(row) = self.get_session_any(session_id)? else {
            return Ok(None);
        };
        if row.user_id != user_id || row.deleted_at.is_some() {
            return Ok(None);
        }
        Ok(Some(row))
    }

    fn get_session_any(&self, session_id: &str) -> anyhow::Result<Option<SessionRow>> {
        let read = self.db.begin_read()?;
        let table = read.open_table(SESSIONS)?;
        let Some(value) = table.get(session_id)? else {
            return Ok(None);
        };
        Ok(Some(decode_session(value.value())?))
    }

    pub fn list_sessions(
        &self,
        user_id: &str,
        offset: usize,
        limit: usize,
        query: Option<&str>,
        device_id: Option<&str>,
    ) -> anyhow::Result<Vec<SessionRow>> {
        let read = self.db.begin_read()?;
        let needle = query
            .filter(|value| !value.is_empty())
            .map(|value| value.to_ascii_lowercase());
        let sessions = read.open_table(SESSIONS)?;

        // Unfiltered catalog: walk the updated-at index in descending order and
        // materialize only the requested page. This keeps a 10k-session
        // catalog from parsing every JSON row.
        if needle.is_none() && device_id.is_none() {
            let index = read.open_table(SESSIONS_BY_USER_UPDATED)?;
            // Walk the B-tree in reverse order instead of materializing and
            // sorting every key for each catalog page.
            let prefix = format!("{user_id}\0");
            let upper = format!("{prefix}\u{10FFFF}");
            let mut rows = Vec::new();
            let mut live_seen = 0usize;
            let iterator = index.range::<&str>(prefix.as_str()..upper.as_str())?;
            for item in iterator.rev() {
                let (_, id) = item?;
                let Some(value) = sessions.get(id.value())? else {
                    continue;
                };
                let row: SessionRow = decode_session(value.value())?;
                if row.deleted_at.is_some() {
                    continue;
                }
                if live_seen < offset {
                    live_seen += 1;
                    continue;
                }
                rows.push(row);
                if rows.len() >= limit {
                    break;
                }
            }
            return Ok(rows);
        }

        let index = read.open_table(SESSIONS_BY_USER)?;
        let ids = prefix_values(&index, &format!("{user_id}\0"))?;
        let mut rows = Vec::new();
        for id in ids {
            if let Some(value) = sessions.get(id.as_str())? {
                let row: SessionRow = decode_session(value.value())?;
                if row.deleted_at.is_some() {
                    continue;
                }
                if let Some(device_id) = device_id {
                    if row.device_id != device_id {
                        continue;
                    }
                }
                if let Some(needle) = &needle {
                    let haystack = format!(
                        "{} {}",
                        row.label.as_deref().unwrap_or(""),
                        row.cwd.as_deref().unwrap_or("")
                    )
                    .to_ascii_lowercase();
                    if !haystack.contains(needle) {
                        continue;
                    }
                }
                rows.push(row);
            }
        }
        rows.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| right.id.cmp(&left.id))
        });
        Ok(rows.into_iter().skip(offset).take(limit).collect())
    }

    pub fn soft_delete_session(&self, user_id: &str, session_id: &str) -> anyhow::Result<bool> {
        let Some(mut row) = self.get_session(user_id, session_id)? else {
            return Ok(false);
        };
        let index_key = session_updated_key(&row.user_id, row.updated_at, &row.id);
        row.deleted_at = Some(now_ms());
        row.online = false;
        let bytes = encode_session(&row)?;
        let txn = self.db.begin_write()?;
        {
            let mut sessions = txn.open_table(SESSIONS)?;
            let mut by_user_updated = txn.open_table(SESSIONS_BY_USER_UPDATED)?;
            sessions.insert(row.id.as_str(), bytes.as_slice())?;
            by_user_updated.remove(index_key.as_str())?;
        }
        txn.commit()?;
        Ok(true)
    }

    pub fn mark_all_sessions_offline(&self) -> anyhow::Result<()> {
        let read = self.db.begin_read()?;
        let sessions = read.open_table(SESSIONS)?;
        let mut rows = Vec::new();
        for item in sessions.iter()? {
            let (_, value) = item?;
            let row: SessionRow = decode_session(value.value())?;
            if row.online {
                rows.push(row);
            }
        }
        drop(sessions);
        drop(read);
        if rows.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin_write()?;
        {
            let mut sessions = txn.open_table(SESSIONS)?;
            for mut row in rows {
                row.online = false;
                let bytes = encode_session(&row)?;
                sessions.insert(row.id.as_str(), bytes.as_slice())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    pub fn mark_device_sessions_offline(&self, device_id: &str) -> anyhow::Result<()> {
        let read = self.db.begin_read()?;
        let index = read.open_table(SESSIONS_BY_DEVICE)?;
        let ids = prefix_values(&index, &format!("{device_id}\0"))?;
        let sessions = read.open_table(SESSIONS)?;
        let mut rows = Vec::new();
        for id in ids {
            if let Some(value) = sessions.get(id.as_str())? {
                let mut row: SessionRow = decode_session(value.value())?;
                if row.online {
                    row.online = false;
                    rows.push(row);
                }
            }
        }
        drop(sessions);
        drop(read);
        if rows.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin_write()?;
        {
            let mut sessions = txn.open_table(SESSIONS)?;
            for row in rows {
                let bytes = encode_session(&row)?;
                sessions.insert(row.id.as_str(), bytes.as_slice())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    pub fn count_sessions(&self) -> anyhow::Result<u64> {
        let read = self.db.begin_read()?;
        let table = read.open_table(SESSIONS)?;
        // O(1) table length; soft-deleted tombstones remain in the table but
        // never appear in catalog pages.
        Ok(table.len()?)
    }

    pub fn audit(&self, user_id: Option<&str>, action: &str, detail: Option<&str>) {
        let row = serde_json::json!({
            "userId": user_id,
            "action": action,
            "detail": detail,
            "createdAt": now_ms(),
        });
        let Ok(bytes) = serde_json::to_vec(&row) else {
            return;
        };
        let key = format!("{:020}\0{}", now_ms(), Uuid::new_v4());
        if let Ok(txn) = self.db.begin_write() {
            if let Ok(mut table) = txn.open_table(AUDIT) {
                let _ = table.insert(key.as_str(), bytes.as_slice());
            }
            let _ = txn.commit();
        }
    }
}

fn encode_session(row: &SessionRow) -> anyhow::Result<Vec<u8>> {
    Ok(bincode::serialize(row)?)
}

fn decode_session(bytes: &[u8]) -> anyhow::Result<SessionRow> {
    // Read databases written before the compact bincode row format.
    bincode::deserialize(bytes)
        .or_else(|_| serde_json::from_slice(bytes))
        .map_err(Into::into)
}

fn prefix_values(
    table: &impl ReadableTable<&'static str, &'static str>,
    prefix: &str,
) -> anyhow::Result<Vec<String>> {
    let upper = format!("{prefix}\u{10FFFF}");
    let mut values = Vec::new();
    for item in table.range::<&str>(prefix..upper.as_str())? {
        let (key, value) = item?;
        if key.value().starts_with(prefix) {
            values.push(value.value().to_owned());
        }
    }
    Ok(values)
}

fn session_updated_key(user_id: &str, updated_at: i64, session_id: &str) -> String {
    format!("{user_id}\0{updated_at:020}\0{session_id}")
}

fn build_session_row(
    user_id: &str,
    device_id: &str,
    descriptor: &SessionDescriptor,
    online: bool,
    created_at: i64,
    updated_at: i64,
) -> SessionRow {
    let model = descriptor.model.as_ref();
    SessionRow {
        id: format!("{device_id}:{}", descriptor.id),
        user_id: user_id.to_owned(),
        device_id: device_id.to_owned(),
        local_id: descriptor.id.clone(),
        session_id: descriptor.session_id.clone(),
        has_pending_ui_request: descriptor.has_pending_ui_request,
        last_activity_at: descriptor.last_activity_at,
        label: descriptor.label.clone(),
        kind: descriptor
            .kind
            .clone()
            .unwrap_or_else(|| "parent".to_owned()),
        cwd: descriptor.cwd.clone(),
        model_provider: model.map(|model| model.provider.clone()),
        model_id: model.map(|model| model.id.clone()),
        model_name: model.and_then(|model| model.name.clone()),
        thinking_level: descriptor.thinking_level.clone(),
        is_streaming: descriptor.is_streaming.unwrap_or(false),
        is_compacting: descriptor.is_compacting.unwrap_or(false),
        message_count: descriptor.message_count.unwrap_or(0),
        last_error: descriptor.last_error.clone(),
        online,
        created_at,
        updated_at,
        last_seen_at: Some(updated_at),
        deleted_at: None,
    }
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = std::fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600);
        let _ = std::fs::set_permissions(path, permissions);
    }
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_tokens_reads_key_suffixes_and_device_revoke_kills_device_tokens() {
        let dir = std::env::temp_dir().join(format!("pir-phone-store-test-{}", Uuid::new_v4()));
        let store = Store::open(&dir).expect("open store");
        let user = store.create_user("token-user", None).expect("user");
        let (user_token, user_row) = store
            .create_token(&user.id, "user", Some("user token"), None)
            .expect("user token");
        let device = store
            .create_device(&user.id, "device", None)
            .expect("device");
        let (device_token, device_row) = store
            .create_token(&user.id, "device", Some("device token"), Some(&device.id))
            .expect("device token");

        let rows = store.list_tokens(&user.id).expect("list tokens");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.id == user_row.id));
        assert!(rows.iter().any(|row| row.id == device_row.id));

        assert!(store
            .revoke_device(&user.id, &device.id)
            .expect("revoke device"));
        let rows = store
            .list_tokens(&user.id)
            .expect("list tokens after revoke");
        assert!(rows
            .iter()
            .find(|row| row.id == device_row.id)
            .is_some_and(|row| row.revoked_at.is_some()));
        assert!(rows
            .iter()
            .find(|row| row.id == user_row.id)
            .is_some_and(|row| row.revoked_at.is_none()));
        assert!(!store
            .revoke_device(&user.id, &device.id)
            .expect("repeat revoke"));
        assert!(crate::auth::authenticate_token(&store, &device_token, "device").is_err());
        assert!(crate::auth::authenticate_token(&store, &user_token, "user").is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
