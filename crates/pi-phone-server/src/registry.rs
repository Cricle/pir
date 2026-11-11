use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;
use tokio::sync::mpsc;

use crate::protocol::{json_line, SessionDescriptor};

pub type ClientId = u64;

// Status metadata is the only thing the relay caches. Transcripts, snapshots
// and images are forwarded and then dropped; clients resync through the agent.
const MAX_TOTAL_CACHE_BYTES: usize = 64 * 1024 * 1024;

pub struct DeviceRuntime {
    pub device_id: String,
    pub user_id: String,
    pub name: String,
    pub platform: Option<String>,
    pub tx: mpsc::Sender<Arc<str>>,
    pub close: Option<tokio::sync::oneshot::Sender<()>>,
    pub connection_id: u64,
    pub sessions: HashSet<String>,
    pub custom_events: HashSet<String>,
    pub last_seen: Instant,
}

pub struct SessionRuntime {
    pub id: String,
    pub user_id: String,
    pub device_id: String,
    pub local_id: String,
    pub descriptor: SessionDescriptor,
    pub online: bool,
    pub subscribers: HashMap<ClientId, mpsc::Sender<Arc<str>>>,
    pub cached_status: Option<Arc<str>>,
    pub cache_size: usize,
    pub last_seen: Instant,
}

pub enum PendingReply {
    Client {
        client_id: ClientId,
        original_id: Option<String>,
    },
    Http {
        tx: tokio::sync::oneshot::Sender<Value>,
    },
}

pub struct PendingCommand {
    pub reply: PendingReply,
    pub created_at: Instant,
    pub user_id: String,
    pub device_id: String,
    pub session_id: String,
}

pub struct ClientRuntime {
    pub user_id: String,
    pub tx: mpsc::Sender<Arc<str>>,
    pub subscriptions: HashSet<String>,
}

#[derive(Default)]
pub struct Registry {
    pub devices: HashMap<String, DeviceRuntime>,
    pub sessions: HashMap<String, SessionRuntime>,
    pub clients: HashMap<ClientId, ClientRuntime>,
    pub pending: HashMap<String, PendingCommand>,
    next_client_id: ClientId,
    next_forward_id: u64,
    next_connection_id: u64,
    total_cache_bytes: usize,
    online_session_count: usize,
    cached_session_count: usize,
    subscriber_count_total: usize,
    cache_order: VecDeque<String>,
    /// 0 = no high-water eviction (historical behavior).
    max_resident_sessions: usize,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            next_client_id: 1,
            next_forward_id: 1,
            next_connection_id: 1,
            max_resident_sessions: 100_000,
            ..Self::default()
        }
    }

    pub fn set_max_resident_sessions(&mut self, max: usize) {
        self.max_resident_sessions = max;
        self.evict_offline_sessions();
    }

    /// Keep the registry bounded without changing online sessions: only
    /// offline sessions with no subscribers are eligible, oldest first.
    fn evict_offline_sessions(&mut self) {
        if self.max_resident_sessions == 0 || self.sessions.len() <= self.max_resident_sessions {
            return;
        }
        let mut candidates: Vec<(String, Instant)> = self
            .sessions
            .iter()
            .filter(|(_, session)| !session.online && session.subscribers.is_empty())
            .map(|(id, session)| (id.clone(), session.last_seen))
            .collect();
        candidates.sort_by_key(|(_, last_seen)| *last_seen);
        for (id, _) in candidates {
            if self.sessions.len() <= self.max_resident_sessions {
                break;
            }
            if let Some(session) = self.sessions.remove(&id) {
                self.total_cache_bytes = self.total_cache_bytes.saturating_sub(session.cache_size);
                if session.cache_size > 0 {
                    self.cached_session_count = self.cached_session_count.saturating_sub(1);
                }
                self.cache_order.retain(|cached| cached != &id);
                for device in self.devices.values_mut() {
                    device.sessions.remove(&id);
                }
            }
        }
    }

    pub fn alloc_client(&mut self, user_id: &str, tx: mpsc::Sender<Arc<str>>) -> ClientId {
        let id = self.next_client_id;
        self.next_client_id = self.next_client_id.saturating_add(1);
        self.clients.insert(
            id,
            ClientRuntime {
                user_id: user_id.to_owned(),
                tx,
                subscriptions: HashSet::new(),
            },
        );
        id
    }

    pub fn remove_client(&mut self, client_id: ClientId) {
        let Some(client) = self.clients.remove(&client_id) else {
            return;
        };
        for session_id in client.subscriptions {
            let removed = self
                .sessions
                .get_mut(&session_id)
                .and_then(|session| session.subscribers.remove(&client_id))
                .is_some();
            if removed {
                self.subscriber_count_total = self.subscriber_count_total.saturating_sub(1);
            }
        }
    }

    pub fn register_device(
        &mut self,
        user_id: &str,
        hello: &crate::protocol::AgentHello,
        tx: mpsc::Sender<Arc<str>>,
        close: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> (Vec<String>, u64) {
        let device_id = hello.device_id.clone();
        let now = Instant::now();
        let connection_id = self.next_connection_id;
        self.next_connection_id = self.next_connection_id.saturating_add(1);

        let mut catalog_ids = Vec::with_capacity(hello.sessions.len());
        for descriptor in &hello.sessions {
            catalog_ids.push(self.upsert_session(user_id, &device_id, descriptor, true).0);
        }

        if let Some(device) = self.devices.get_mut(&device_id) {
            // A reconnecting agent replaces the old socket. Keep the session
            // records but drop sessions the new hello no longer reports.
            let keep: HashSet<String> = catalog_ids.iter().cloned().collect();
            let stale: Vec<String> = device
                .sessions
                .iter()
                .filter(|id| !keep.contains(*id))
                .cloned()
                .collect();
            for id in stale {
                if let Some(session) = self.sessions.remove(&id) {
                    if session.online {
                        self.online_session_count = self.online_session_count.saturating_sub(1);
                    }
                    self.subscriber_count_total = self
                        .subscriber_count_total
                        .saturating_sub(session.subscribers.len());
                    if session.cache_size > 0 {
                        self.cached_session_count = self.cached_session_count.saturating_sub(1);
                    }
                    self.total_cache_bytes =
                        self.total_cache_bytes.saturating_sub(session.cache_size);
                    self.cache_order.retain(|cached| cached != &id);
                }
            }
            device.user_id = user_id.to_owned();
            device.name = hello
                .device_name
                .clone()
                .unwrap_or_else(|| device_id.clone());
            device.platform = hello.platform.clone();
            device.tx = tx;
            device.close = close;
            device.connection_id = connection_id;
            device.sessions = keep;
            device.custom_events = hello
                .custom_events
                .iter()
                .filter(|name| {
                    name.starts_with("phone.custom.") || name.as_str() == "phone.custom.*"
                })
                .cloned()
                .collect();
            device.last_seen = now;
        } else {
            self.devices.insert(
                device_id.clone(),
                DeviceRuntime {
                    device_id: device_id.clone(),
                    user_id: user_id.to_owned(),
                    name: hello
                        .device_name
                        .clone()
                        .unwrap_or_else(|| device_id.clone()),
                    platform: hello.platform.clone(),
                    tx,
                    close,
                    connection_id,
                    sessions: catalog_ids.iter().cloned().collect(),
                    custom_events: hello
                        .custom_events
                        .iter()
                        .filter(|name| {
                            name.starts_with("phone.custom.") || name.as_str() == "phone.custom.*"
                        })
                        .cloned()
                        .collect(),
                    last_seen: now,
                },
            );
        }

        (catalog_ids, connection_id)
    }

    /// Remove a device connection. `connection_id = None` forces removal
    /// (admin revoke); an agent cleanup must pass its own connection id so a
    /// stale socket cannot remove its newer replacement.
    pub fn unregister_device(&mut self, device_id: &str, connection_id: Option<u64>) {
        if let Some(connection_id) = connection_id {
            if self
                .devices
                .get(device_id)
                .is_some_and(|device| device.connection_id != connection_id)
            {
                return;
            }
        }
        if let Some(device) = self.devices.remove(device_id) {
            if let Some(close) = device.close {
                let _ = close.send(());
            }
            let mut cache_bytes = 0usize;
            let mut cached_count = 0usize;
            let session_ids: Vec<String> = device.sessions.into_iter().collect();
            for session_id in &session_ids {
                if let Some(session) = self.sessions.get_mut(session_id) {
                    if session.online {
                        session.online = false;
                        self.online_session_count = self.online_session_count.saturating_sub(1);
                    }
                    if session.cache_size > 0 {
                        cache_bytes = cache_bytes.saturating_add(session.cache_size);
                        cached_count += 1;
                        session.cache_size = 0;
                        session.cached_status = None;
                    }
                    let line = json_line(serde_json::json!({
                        "channel": "server",
                        "event": "device-offline",
                        "data": { "deviceId": device_id, "name": device.name },
                    }));
                    broadcast_to(&mut session.subscribers, &line);
                }
            }
            self.total_cache_bytes = self.total_cache_bytes.saturating_sub(cache_bytes);
            self.cached_session_count = self.cached_session_count.saturating_sub(cached_count);
            if cache_bytes > 0 {
                for session_id in &session_ids {
                    self.cache_order.retain(|cached| cached != session_id);
                }
            }
            self.evict_offline_sessions();
        }
    }

    /// Insert or update a live session. Returns the catalog id and whether the
    /// descriptor/online state actually changed. Status frames are noisy; the
    /// changed flag keeps unchanged frames out of redb and the catalog fan-out.
    pub fn upsert_session(
        &mut self,
        user_id: &str,
        device_id: &str,
        descriptor: &SessionDescriptor,
        online: bool,
    ) -> (String, bool) {
        let catalog_id = format!("{device_id}:{}", descriptor.id);
        let now = Instant::now();
        let (changed, online_delta) = if let Some(session) = self.sessions.get_mut(&catalog_id) {
            let was_online = session.online;
            let descriptor_changed = merge_descriptor(&mut session.descriptor, descriptor);
            let changed = descriptor_changed || was_online != online;
            if changed {
                session.online = online;
                session.last_seen = now;
            }
            (
                changed,
                (if online { 1i32 } else { 0 }) - (if was_online { 1i32 } else { 0 }),
            )
        } else {
            self.sessions.insert(
                catalog_id.clone(),
                SessionRuntime {
                    id: catalog_id.clone(),
                    user_id: user_id.to_owned(),
                    device_id: device_id.to_owned(),
                    local_id: descriptor.id.clone(),
                    descriptor: descriptor.clone(),
                    online,
                    subscribers: HashMap::new(),
                    cached_status: None,
                    cache_size: 0,
                    last_seen: now,
                },
            );
            (true, if online { 1i32 } else { 0 })
        };
        if online_delta > 0 {
            self.online_session_count = self
                .online_session_count
                .saturating_add(online_delta as usize);
        } else if online_delta < 0 {
            self.online_session_count = self
                .online_session_count
                .saturating_sub((-online_delta) as usize);
        }
        if let Some(device) = self.devices.get_mut(device_id) {
            device.sessions.insert(catalog_id.clone());
        }
        (catalog_id, changed)
    }

    pub fn remove_session(&mut self, device_id: &str, local_id: &str) {
        let catalog_id = format!("{device_id}:{local_id}");
        self.remove_session_id(&catalog_id);
    }

    pub fn remove_session_id(&mut self, catalog_id: &str) {
        if let Some(session) = self.sessions.remove(catalog_id) {
            if session.online {
                self.online_session_count = self.online_session_count.saturating_sub(1);
            }
            self.subscriber_count_total = self
                .subscriber_count_total
                .saturating_sub(session.subscribers.len());
            if session.cache_size > 0 {
                self.cached_session_count = self.cached_session_count.saturating_sub(1);
            }
            self.total_cache_bytes = self.total_cache_bytes.saturating_sub(session.cache_size);
            self.cache_order.retain(|id| id != catalog_id);
            if let Some(device) = self.devices.get_mut(&session.device_id) {
                device.sessions.remove(catalog_id);
            }
            let line = json_line(serde_json::json!({
                "channel": "server",
                "event": "session-gone",
                "data": { "sessionId": catalog_id },
            }));
            let mut subscribers = session.subscribers;
            broadcast_to(&mut subscribers, &line);
        }
    }

    pub fn subscribe(&mut self, client_id: ClientId, session_id: &str) -> Option<&SessionRuntime> {
        if !self
            .sessions
            .get(session_id)
            .is_some_and(|session| session.online)
        {
            return None;
        }
        let tx = {
            let client = self.clients.get_mut(&client_id)?;
            client.subscriptions.insert(session_id.to_owned());
            client.tx.clone()
        };
        let session = self.sessions.get_mut(session_id)?;
        if !session.online {
            return None;
        }
        if session.subscribers.insert(client_id, tx).is_none() {
            self.subscriber_count_total = self.subscriber_count_total.saturating_add(1);
        }
        Some(session)
    }

    pub fn unsubscribe(&mut self, client_id: ClientId, session_id: &str) {
        let removed = self
            .sessions
            .get_mut(session_id)
            .and_then(|session| session.subscribers.remove(&client_id))
            .is_some();
        if removed {
            self.subscriber_count_total = self.subscriber_count_total.saturating_sub(1);
        }
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.subscriptions.remove(session_id);
        }
    }

    /// Cache only the latest metadata status line. Transcript/snapshot frames
    /// are relayed to current subscribers and immediately dropped.
    pub fn cache_frame(&mut self, session_id: &str, payload: &Value, line: &Arc<str>) {
        if payload.get("channel").and_then(Value::as_str) != Some("server")
            || payload.get("event").and_then(Value::as_str) != Some("status")
        {
            return;
        }

        let (old_size, new_size) = {
            let Some(session) = self.sessions.get_mut(session_id) else {
                return;
            };
            let old_size = session.cache_size;
            let was_cached = old_size > 0;
            session.cached_status = Some(Arc::clone(line));
            let new_size = line.len();
            session.cache_size = new_size;
            if !was_cached && new_size > 0 {
                self.cached_session_count = self.cached_session_count.saturating_add(1);
            } else if was_cached && new_size == 0 {
                self.cached_session_count = self.cached_session_count.saturating_sub(1);
            }
            (old_size, new_size)
        };

        self.total_cache_bytes = self
            .total_cache_bytes
            .saturating_sub(old_size)
            .saturating_add(new_size);
        self.cache_order.retain(|id| id != session_id);
        self.cache_order.push_back(session_id.to_owned());
        while self.total_cache_bytes > MAX_TOTAL_CACHE_BYTES {
            let Some(oldest) = self.cache_order.pop_front() else {
                break;
            };
            let Some(session) = self.sessions.get_mut(&oldest) else {
                continue;
            };
            if session.cache_size > 0 {
                self.cached_session_count = self.cached_session_count.saturating_sub(1);
            }
            self.total_cache_bytes = self.total_cache_bytes.saturating_sub(session.cache_size);
            session.cache_size = 0;
            session.cached_status = None;
        }
    }

    /// Broadcast one pre-serialized line. Returns the number of subscribers
    /// that accepted the frame.
    pub fn broadcast(&mut self, session_id: &str, line: &Arc<str>) -> usize {
        if let Some(session) = self.sessions.get_mut(session_id) {
            broadcast_to(&mut session.subscribers, line)
        } else {
            0
        }
    }

    pub fn next_forward_id(&mut self) -> String {
        let sequence = self.next_forward_id;
        self.next_forward_id = self.next_forward_id.saturating_add(1);
        // Object-capability style ID: sequence makes logs readable, UUID makes
        // cross-tenant guessing impractical.
        format!("f-{sequence}-{}", uuid::Uuid::new_v4())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_pending(
        &mut self,
        forward_id: String,
        reply: PendingReply,
        user_id: &str,
        device_id: &str,
        session_id: &str,
    ) {
        self.pending.insert(
            forward_id,
            PendingCommand {
                reply,
                created_at: Instant::now(),
                user_id: user_id.to_owned(),
                device_id: device_id.to_owned(),
                session_id: session_id.to_owned(),
            },
        );
    }

    pub fn take_pending(&mut self, forward_id: &str) -> Option<PendingCommand> {
        self.pending.remove(forward_id)
    }

    /// Only the agent that owns the originating session may complete a
    /// command. This prevents another tenant from spraying sequential IDs.
    pub fn take_pending_for(
        &mut self,
        forward_id: &str,
        user_id: &str,
        device_id: &str,
        session_id: &str,
    ) -> Option<PendingCommand> {
        let matches = self.pending.get(forward_id).is_some_and(|pending| {
            pending.user_id == user_id
                && pending.device_id == device_id
                && pending.session_id == session_id
        });
        if matches {
            self.pending.remove(forward_id)
        } else {
            None
        }
    }

    pub fn remove_pending_for_client(&mut self, client_id: ClientId) {
        self.pending.retain(|_, pending| {
            !matches!(
                &pending.reply,
                PendingReply::Client { client_id: id, .. } if *id == client_id
            )
        });
    }

    pub fn remove_pending_for_device(&mut self, device_id: &str) {
        self.pending
            .retain(|_, pending| pending.device_id != device_id);
    }

    pub fn expire_pending(&mut self, older_than: std::time::Duration) {
        let now = Instant::now();
        self.pending
            .retain(|_, pending| now.duration_since(pending.created_at) < older_than);
    }

    pub fn online_sessions(&self) -> usize {
        self.online_session_count
    }

    pub fn online_devices(&self) -> usize {
        self.devices.len()
    }

    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    pub fn cache_bytes(&self) -> usize {
        self.total_cache_bytes
    }

    pub fn cached_sessions(&self) -> usize {
        self.cached_session_count
    }

    pub fn subscriber_count(&self) -> usize {
        self.subscriber_count_total
    }

    pub fn pending_commands(&self) -> usize {
        self.pending.len()
    }
}

/// Merge a (possibly partial) status descriptor into the authoritative hello /
/// session-upsert descriptor. Missing fields mean "unchanged", not "clear".
fn merge_descriptor(dst: &mut SessionDescriptor, src: &SessionDescriptor) -> bool {
    let mut changed = false;
    macro_rules! merge_option {
        ($field:ident) => {
            if let Some(value) = &src.$field {
                if dst.$field.as_ref() != Some(value) {
                    dst.$field = Some(value.clone());
                    changed = true;
                }
            }
        };
    }
    merge_option!(session_id);
    merge_option!(label);
    merge_option!(kind);
    merge_option!(cwd);
    merge_option!(thinking_level);
    merge_option!(last_error);
    merge_option!(last_activity_at);
    if let Some(model) = &src.model {
        if dst.model.as_ref() != Some(model) {
            dst.model = Some(model.clone());
            changed = true;
        }
    }
    macro_rules! merge_bool {
        ($field:ident) => {
            if let Some(value) = src.$field {
                if dst.$field != Some(value) {
                    dst.$field = Some(value);
                    changed = true;
                }
            }
        };
    }
    merge_bool!(has_pending_ui_request);
    merge_bool!(is_streaming);
    merge_bool!(is_compacting);
    if let Some(value) = src.message_count {
        if dst.message_count != Some(value) {
            dst.message_count = Some(value);
            changed = true;
        }
    }
    changed
}

fn broadcast_to(
    subscribers: &mut HashMap<ClientId, mpsc::Sender<Arc<str>>>,
    line: &Arc<str>,
) -> usize {
    let mut slow = Vec::new();
    let mut delivered = 0;
    for (id, tx) in subscribers.iter() {
        if tx.try_send(Arc::clone(line)).is_err() {
            slow.push(*id);
        } else {
            delivered += 1;
        }
    }
    for id in slow {
        subscribers.remove(&id);
    }
    delivered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_reply_is_scoped_to_owner() {
        let mut registry = Registry::new();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        registry.add_pending(
            "f-1".to_owned(),
            PendingReply::Http { tx },
            "user-a",
            "device-a",
            "device-a:session",
        );
        assert!(registry
            .take_pending_for("f-1", "user-b", "device-a", "device-a:session")
            .is_none());
        assert!(registry
            .take_pending_for("f-1", "user-a", "device-b", "device-a:session")
            .is_none());
        assert!(registry
            .take_pending_for("f-1", "user-a", "device-a", "device-b:session")
            .is_none());
        assert!(registry
            .take_pending_for("f-1", "user-a", "device-a", "device-a:session")
            .is_some());
    }

    fn descriptor(id: &str) -> SessionDescriptor {
        SessionDescriptor {
            id: id.to_owned(),
            session_id: None,
            has_pending_ui_request: None,
            last_activity_at: None,
            label: None,
            kind: None,
            cwd: None,
            model: None,
            thinking_level: None,
            is_streaming: None,
            is_compacting: None,
            message_count: None,
            last_error: None,
        }
    }

    #[test]
    fn upsert_session_merges_live_pending_ui_and_activity() {
        let mut registry = Registry::new();
        assert!(
            registry
                .upsert_session("user-a", "device-a", &descriptor("active-session-1"), true)
                .1
        );

        // A catalog push for a pool child may differ only in the live fields
        // the bridge tracks; it must still mark the row changed so the relay
        // persists and broadcasts them.
        let mut live = descriptor("active-session-1");
        live.session_id = Some("native-child".to_owned());
        live.has_pending_ui_request = Some(true);
        live.last_activity_at = Some(1750);
        assert!(registry.upsert_session("user-a", "device-a", &live, true).1);
        let runtime = registry
            .sessions
            .get("device-a:active-session-1")
            .expect("session row");
        assert_eq!(
            runtime.descriptor.has_pending_ui_request,
            Some(true),
            "pending flag must survive the merge"
        );
        assert_eq!(runtime.descriptor.last_activity_at, Some(1750));
        assert_eq!(
            runtime.descriptor.session_id.as_deref(),
            Some("native-child")
        );

        // Answering the dialog clears the flag through the same path.
        let mut cleared = descriptor("active-session-1");
        cleared.has_pending_ui_request = Some(false);
        assert!(
            registry
                .upsert_session("user-a", "device-a", &cleared, true)
                .1
        );
        let runtime = registry
            .sessions
            .get("device-a:active-session-1")
            .expect("session row");
        assert_eq!(runtime.descriptor.has_pending_ui_request, Some(false));
        assert_eq!(runtime.descriptor.last_activity_at, Some(1750));
    }

    #[test]
    fn forward_ids_are_not_predictable_sequences() {
        let mut registry = Registry::new();
        let first = registry.next_forward_id();
        let second = registry.next_forward_id();
        assert_ne!(first, second);
        assert!(!second.ends_with("-1"));
        assert!(first.len() > 20);
    }

    #[test]
    fn high_water_evicts_only_offline_unsubscribed_sessions() {
        let mut registry = Registry::new();
        registry.set_max_resident_sessions(2);
        for index in 0..4 {
            let id = format!("device:session-{index}");
            registry.sessions.insert(
                id.clone(),
                SessionRuntime {
                    id: id.clone(),
                    user_id: "user".to_owned(),
                    device_id: "device".to_owned(),
                    local_id: format!("session-{index}"),
                    descriptor: crate::protocol::SessionDescriptor {
                        id: format!("session-{index}"),
                        session_id: None,
                        has_pending_ui_request: None,
                        last_activity_at: None,
                        label: None,
                        kind: None,
                        cwd: None,
                        model: None,
                        thinking_level: None,
                        is_streaming: None,
                        is_compacting: None,
                        message_count: None,
                        last_error: None,
                    },
                    online: index == 3,
                    subscribers: HashMap::new(),
                    cached_status: None,
                    cache_size: 0,
                    last_seen: Instant::now(),
                },
            );
        }
        registry.online_session_count = 1;
        registry.evict_offline_sessions();
        assert_eq!(registry.sessions.len(), 2);
        assert!(registry.sessions.contains_key("device:session-3"));
        assert_eq!(registry.online_session_count, 1);
    }
}
