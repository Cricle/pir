//! Protocol v8 generic RPC envelope: targets, service calls, and provider updates.

use serde::Serialize;
use serde_json::Value;

use super::delta::{DeltaDecoder, DeltaOp};
use super::error::SchemaError;
use super::validate::{
    field, field_opt, require_array, require_id, require_integer, require_object, require_string,
    validate_json_value, validate_strict,
};

// ---------------------------------------------------------------------------
// Protocol v8: generic RPC envelope
//
// Sources: pi packages/protocol/src/protocol.ts (envelope + targets) and
// packages/chord/src/services/wire.ts (service call/update payloads).
// The concrete v1 catalog above is retained as the session service payload
// layer plus the stdio RPC transition surface.
// ---------------------------------------------------------------------------

/// Built-in control service id (upstream `wire.ts` SERVICE_CONTROL_ID).
pub const SERVICE_CONTROL_ID: &str = "$chord.service";
/// pir's hosted session service id (the concrete v1 catalog as chord members).
pub const SESSION_SERVICE_ID: &str = "pir.session";

/// Upstream `ServerIdSchema` pattern (protocol.ts:12-19).
pub const SERVER_ID_PATTERN: &str =
    "^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$";

/// Check a string against the upstream `ServerId` pattern (UUIDv4 shape with
/// lowercase hex, as pinned by `packages/protocol/src/protocol.ts:13`).
pub fn is_server_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (index, byte) in bytes.iter().enumerate() {
        match index {
            8 | 13 | 18 | 23 => {
                if *byte != b'-' {
                    return false;
                }
            }
            14 => {
                if *byte != b'4' {
                    return false;
                }
            }
            19 => {
                if !matches!(*byte, b'8' | b'9' | b'a' | b'b') {
                    return false;
                }
            }
            _ => {
                if !matches!(*byte, b'0'..=b'9' | b'a'..=b'f') {
                    return false;
                }
            }
        }
    }
    true
}

/// Service subscription mode (upstream `ServiceMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceMode {
    Singleton,
    Keyed,
}

impl ServiceMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ServiceMode::Singleton => "singleton",
            ServiceMode::Keyed => "keyed",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "singleton" => Some(ServiceMode::Singleton),
            "keyed" => Some(ServiceMode::Keyed),
            _ => None,
        }
    }
}

/// A server-wide call target, fenced to one logical server.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ServerTarget {
    #[serde(rename = "serverId")]
    pub server_id: String,
}

/// A session call target, fenced to one logical server, durable session, and
/// live attachment (upstream `SessionTarget`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionTarget {
    #[serde(rename = "serverId")]
    pub server_id: String,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "attachmentId")]
    pub attachment_id: String,
}

/// RPC call target: server-wide or session-fenced (upstream `RpcTarget`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum RpcTarget {
    Server(ServerTarget),
    Session(SessionTarget),
}

impl RpcTarget {
    pub fn server(server_id: impl Into<String>) -> Self {
        RpcTarget::Server(ServerTarget {
            server_id: server_id.into(),
        })
    }

    pub fn session(
        server_id: impl Into<String>,
        session_id: impl Into<String>,
        attachment_id: impl Into<String>,
    ) -> Self {
        RpcTarget::Session(SessionTarget {
            server_id: server_id.into(),
            session_id: session_id.into(),
            attachment_id: attachment_id.into(),
        })
    }

    pub fn server_id(&self) -> &str {
        match self {
            RpcTarget::Server(target) => &target.server_id,
            RpcTarget::Session(target) => &target.server_id,
        }
    }
}

/// Parse an RPC target value: `{serverId}` or `{serverId, sessionId, attachmentId}`.
pub fn parse_rpc_target(value: &Value, path: &str) -> Result<RpcTarget, SchemaError> {
    let obj = require_object(value, path)?;
    let is_session = obj.contains_key("sessionId") || obj.contains_key("attachmentId");
    if is_session {
        validate_strict(obj, &["serverId", "sessionId", "attachmentId"], path)?;
        let server_id =
            require_server_id(field(obj, "serverId", path)?, &format!("{path}.serverId"))?;
        let session_id = require_id(field(obj, "sessionId", path)?, &format!("{path}.sessionId"))?;
        let attachment_id = require_id(
            field(obj, "attachmentId", path)?,
            &format!("{path}.attachmentId"),
        )?;
        Ok(RpcTarget::Session(SessionTarget {
            server_id: server_id.to_owned(),
            session_id: session_id.to_owned(),
            attachment_id: attachment_id.to_owned(),
        }))
    } else {
        validate_strict(obj, &["serverId"], path)?;
        let server_id =
            require_server_id(field(obj, "serverId", path)?, &format!("{path}.serverId"))?;
        Ok(RpcTarget::Server(ServerTarget {
            server_id: server_id.to_owned(),
        }))
    }
}

/// Parse a session-only target (`attachment` envelope payload).
pub fn parse_session_target(value: &Value, path: &str) -> Result<SessionTarget, SchemaError> {
    match parse_rpc_target(value, path)? {
        RpcTarget::Session(target) => Ok(target),
        RpcTarget::Server(_) => Err(SchemaError::new(format!(
            "{path}: expected a session target with sessionId and attachmentId"
        ))),
    }
}

fn require_server_id<'a>(v: &'a Value, path: &str) -> Result<&'a str, SchemaError> {
    let s = require_string(v, path)?;
    if !is_server_id(s) {
        return Err(SchemaError::new(format!(
            "{path}: serverId must match {SERVER_ID_PATTERN}"
        )));
    }
    Ok(s)
}

/// Chord service instance address (upstream `ServiceInstanceAddress`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ServiceInstanceAddress {
    pub key: String,
    pub generation: i64,
}

/// Parse one chord instance address: `{key, generation >= 1}`.
pub fn parse_service_instance_address(
    value: &Value,
    path: &str,
) -> Result<ServiceInstanceAddress, SchemaError> {
    let obj = require_object(value, path)?;
    validate_strict(obj, &["key", "generation"], path)?;
    let key = require_id(field(obj, "key", path)?, &format!("{path}.key"))?;
    let generation = require_integer(
        field(obj, "generation", path)?,
        &format!("{path}.generation"),
    )?;
    if generation < 1 {
        return Err(SchemaError::new(format!(
            "{path}.generation: must be >= 1, got {generation}"
        )));
    }
    Ok(ServiceInstanceAddress {
        key: key.to_owned(),
        generation,
    })
}

/// Chord service call payload (upstream `ServiceCall`, wire.ts:89-97).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ServiceCall {
    #[serde(rename = "serviceId")]
    pub service_id: String,
    pub member: String,
    pub args: Vec<Value>,
    #[serde(rename = "instance", skip_serializing_if = "Option::is_none")]
    pub instance: Option<ServiceInstanceAddress>,
}

/// Parse a chord service call: `{serviceId, member, args, instance?}`.
pub fn parse_service_call(value: &Value) -> Result<ServiceCall, SchemaError> {
    let obj = require_object(value, "service call")?;
    validate_strict(
        obj,
        &["serviceId", "member", "args", "instance"],
        "service call",
    )?;
    let service_id = require_id(
        field(obj, "serviceId", "service call")?,
        "service call.serviceId",
    )?;
    let member = require_id(field(obj, "member", "service call")?, "service call.member")?;
    let args = require_array(field(obj, "args", "service call")?, "service call.args")?;
    let mut parsed_args = Vec::with_capacity(args.len());
    for (index, item) in args.iter().enumerate() {
        validate_json_value(item, &format!("service call.args[{index}]"))?;
        parsed_args.push(item.clone());
    }
    let instance = match field_opt(obj, "instance") {
        Some(value) => Some(parse_service_instance_address(
            value,
            "service call.instance",
        )?),
        None => None,
    };
    Ok(ServiceCall {
        service_id: service_id.to_owned(),
        member: member.to_owned(),
        args: parsed_args,
        instance,
    })
}

/// Decoded built-in control service call (upstream `ServiceControlCall`).
#[derive(Debug, Clone, PartialEq)]
pub enum ServiceControlCall {
    Catalogue,
    Subscribe {
        subscription_id: String,
        service_id: String,
        mode: ServiceMode,
    },
    Unsubscribe {
        subscription_id: String,
    },
}

/// Decode a `$chord.service` control call (upstream `decodeServiceControlCall`).
pub fn decode_service_control_call(call: &ServiceCall) -> Option<ServiceControlCall> {
    if call.service_id != SERVICE_CONTROL_ID || call.instance.is_some() {
        return None;
    }
    match call.member.as_str() {
        "catalogue" if call.args.is_empty() => Some(ServiceControlCall::Catalogue),
        "subscribe" if call.args.len() == 3 => {
            let subscription_id = call.args[0].as_str()?;
            let service_id = call.args[1].as_str()?;
            if subscription_id.is_empty() || service_id.is_empty() {
                return None;
            }
            let mode = ServiceMode::parse(call.args[2].as_str()?)?;
            Some(ServiceControlCall::Subscribe {
                subscription_id: subscription_id.to_owned(),
                service_id: service_id.to_owned(),
                mode,
            })
        }
        "unsubscribe" if call.args.len() == 1 => {
            let subscription_id = call.args[0].as_str()?;
            if subscription_id.is_empty() {
                return None;
            }
            Some(ServiceControlCall::Unsubscribe {
                subscription_id: subscription_id.to_owned(),
            })
        }
        _ => None,
    }
}

/// Validate one `ops` array in either vocabulary.
///
/// Each entry is either the legacy `{op, value}` object or a chord delta
/// tuple (base ops, encoded wire ops with `#` definitions and numeric path
/// refs, or arity-shortened forms). Mixing the two in one batch is rejected:
/// a decoder cannot tell where an interleaved object batch begins.
fn parse_state_ops_with(
    ops: &[Value],
    path: &str,
    decoder: &mut DeltaDecoder,
) -> Result<Vec<StateOp>, SchemaError> {
    let mut legacy = false;
    let mut delta = false;
    for op in ops {
        if op.is_object() {
            legacy = true;
        } else if op.is_array() {
            delta = true;
        } else {
            return Err(SchemaError::new(format!(
                "{path}: operations must be objects or delta tuples"
            )));
        }
    }
    if legacy && delta {
        return Err(SchemaError::new(format!(
            "{path}: legacy operations and delta operations cannot be mixed"
        )));
    }
    if delta {
        let decoded = decoder
            .decode(ops)
            .map_err(|error| SchemaError::new(format!("{path}: {error}")))?;
        Ok(decoded.into_iter().map(StateOp::Delta).collect())
    } else {
        let mut parsed = Vec::with_capacity(ops.len());
        for (index, op) in ops.iter().enumerate() {
            let path = format!("{path}[{index}]");
            let obj = require_object(op, &path)?;
            validate_strict(obj, &["op", "value"], &path)?;
            let name = require_id(field(obj, "op", &path)?, &format!("{path}.op"))?.to_owned();
            if let Some(value) = field_opt(obj, "value") {
                validate_json_value(value, &format!("{path}.value"))?;
            }
            parsed.push(StateOp::Legacy {
                op: name,
                value: obj.get("value").cloned(),
            });
        }
        Ok(parsed)
    }
}

/// [`parse_state_ops_with`] for one independent batch (a fresh decoder).
fn parse_state_ops(ops: &[Value], path: &str) -> Result<Vec<StateOp>, SchemaError> {
    parse_state_ops_with(ops, path, &mut DeltaDecoder::new())
}

/// One state-update operation in either vocabulary.
#[derive(Debug, Clone, PartialEq)]
pub enum StateOp {
    /// The legacy v8 object form (`{op, value}`), kept verbatim.
    Legacy { op: String, value: Option<Value> },
    /// A chord delta tuple (decoded).
    Delta(DeltaOp),
}

/// A decoded pir.session `state` service-provider update.
///
/// Shape: `{type:"state", member, sequence, instance?, ops}`. `ops` may be
/// the legacy `{op, value}` objects or chord delta tuples; both are surfaced
/// through [`StateOp`].
#[derive(Debug, Clone, PartialEq)]
pub struct StateUpdate {
    pub member: String,
    pub sequence: i64,
    pub instance: Option<ServiceInstanceAddress>,
    pub ops: Vec<StateOp>,
}

/// Parse a pir.session `state` update into typed operations.
pub fn parse_state_update(value: &Value) -> Result<StateUpdate, SchemaError> {
    parse_state_update_with(value, &mut DeltaDecoder::new())
}

fn parse_state_update_with(
    value: &Value,
    decoder: &mut DeltaDecoder,
) -> Result<StateUpdate, SchemaError> {
    let obj = require_object(value, "state update")?;
    validate_strict(
        obj,
        &["type", "member", "sequence", "ops", "instance"],
        "state update",
    )?;
    let kind = require_string(field(obj, "type", "state update")?, "state update.type")?;
    if kind != "state" {
        return Err(SchemaError::new(
            "state update.type: expected \"state\"".to_owned(),
        ));
    }
    let member =
        require_id(field(obj, "member", "state update")?, "state update.member")?.to_owned();
    let sequence = require_integer(
        field(obj, "sequence", "state update")?,
        "state update.sequence",
    )?;
    if sequence < 1 {
        return Err(SchemaError::new(format!(
            "state update.sequence: must be >= 1, got {sequence}"
        )));
    }
    let instance = match field_opt(obj, "instance") {
        Some(value) => Some(parse_service_instance_address(
            value,
            "state update.instance",
        )?),
        None => None,
    };
    let raw_ops = require_array(field(obj, "ops", "state update")?, "state update.ops")?;
    let ops = parse_state_ops_with(raw_ops, "state update.ops", decoder)?;
    Ok(StateUpdate {
        member,
        sequence,
        instance,
        ops,
    })
}

/// The payload discrimination of an upstream chord `state` frame
/// (`wire.ts:131-139`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChordStateKind {
    Snap,
    Reset,
    Ops,
    Error,
}

impl ChordStateKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChordStateKind::Snap => "snap",
            ChordStateKind::Reset => "reset",
            ChordStateKind::Ops => "ops",
            ChordStateKind::Error => "error",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "snap" => Some(ChordStateKind::Snap),
            "reset" => Some(ChordStateKind::Reset),
            "ops" => Some(ChordStateKind::Ops),
            "error" => Some(ChordStateKind::Error),
            _ => None,
        }
    }
}

/// A decoded upstream chord `state` frame: `{t:"state", hold, m, as, value?,
/// ops?, error?}`. Accepted alongside pir's `{type:"state", member, ...}`
/// update so an upstream chord producer's payload validates on this side.
#[derive(Debug, Clone, PartialEq)]
pub struct ChordStateUpdate {
    pub hold: i64,
    pub member: String,
    pub kind: ChordStateKind,
    pub value: Option<Value>,
    pub ops: Vec<DeltaOp>,
    pub error: Option<Value>,
}

/// Parse and validate an upstream chord `state` frame.
pub fn parse_chord_state_update(value: &Value) -> Result<ChordStateUpdate, SchemaError> {
    parse_chord_state_update_with(value, &mut DeltaDecoder::new())
}

fn parse_chord_state_update_with(
    value: &Value,
    decoder: &mut DeltaDecoder,
) -> Result<ChordStateUpdate, SchemaError> {
    let obj = require_object(value, "chord state")?;
    validate_strict(
        obj,
        &["t", "hold", "m", "as", "value", "ops", "error"],
        "chord state",
    )?;
    let tag = require_string(field(obj, "t", "chord state")?, "chord state.t")?;
    if tag != "state" {
        return Err(SchemaError::new(
            "chord state.t: expected \"state\"".to_owned(),
        ));
    }
    let hold = require_integer(field(obj, "hold", "chord state")?, "chord state.hold")?;
    if hold < 1 {
        return Err(SchemaError::new(format!(
            "chord state.hold: must be >= 1, got {hold}"
        )));
    }
    let member = require_id(field(obj, "m", "chord state")?, "chord state.m")?.to_owned();
    let as_ = require_string(field(obj, "as", "chord state")?, "chord state.as")?;
    let kind = ChordStateKind::parse(as_).ok_or_else(|| {
        SchemaError::new("chord state.as: expected snap/reset/ops/error".to_owned())
    })?;
    let value = field_opt(obj, "value").cloned();
    if let Some(value) = &value {
        validate_json_value(value, "chord state.value")?;
    }
    let ops = match field_opt(obj, "ops") {
        Some(value) => {
            let raw = require_array(value, "chord state.ops")?;
            decoder
                .decode(raw)
                .map_err(|error| SchemaError::new(format!("chord state.ops: {error}")))?
        }
        None => Vec::new(),
    };
    if kind == ChordStateKind::Ops && field_opt(obj, "ops").is_none() {
        return Err(SchemaError::new(
            "chord state: as \"ops\" requires ops".to_owned(),
        ));
    }
    let error = field_opt(obj, "error").cloned();
    if let Some(error) = &error {
        let wire = require_object(error, "chord state.error")?;
        validate_strict(wire, &["code", "message", "reason"], "chord state.error")?;
        require_string(
            field(wire, "code", "chord state.error")?,
            "chord state.error.code",
        )?;
        require_string(
            field(wire, "message", "chord state.error")?,
            "chord state.error.message",
        )?;
        if let Some(reason) = field_opt(wire, "reason") {
            require_string(reason, "chord state.error.reason")?;
        }
    }
    if kind == ChordStateKind::Error && error.is_none() {
        return Err(SchemaError::new(
            "chord state: as \"error\" requires error".to_owned(),
        ));
    }
    Ok(ChordStateUpdate {
        hold,
        member,
        kind,
        value,
        ops,
        error,
    })
}

/// Either state-update shape a v8 `service_update` may carry: pir's
/// `{type:"state", member, sequence, ops}` or an upstream chord
/// `{t:"state", hold, m, as, ...}` frame.
#[derive(Debug, Clone, PartialEq)]
pub enum ServiceUpdate {
    State(StateUpdate),
    Chord(ChordStateUpdate),
}

/// Parse a state update in either shape (the discriminator is `type` vs `t`).
pub fn parse_service_state_update(value: &Value) -> Result<ServiceUpdate, SchemaError> {
    let obj = require_object(value, "service update")?;
    if obj.contains_key("t") {
        return parse_chord_state_update(value).map(ServiceUpdate::Chord);
    }
    parse_state_update(value).map(ServiceUpdate::State)
}

/// Hard ceiling on the number of distinct `(instance, member)` streams one
/// [`StateUpdateDecoder`] keeps delta dictionaries for. Update keys are
/// wire-driven (a peer can invent instance keys), so the map is bounded: the
/// least recently used stream is evicted and a follow-up that references one of
/// its path ids fails with a structured error, which tells the producer to
/// replay from a base batch. Nothing panics.
pub const MAX_STATE_UPDATE_STREAMS: usize = 1024;

/// A stateful decoder for one subscription's stream of state updates.
///
/// Chord delta batches are encoded per state stream: a `["#", id, path]`
/// definition in one update is referenced by numeric ids in later updates.
/// A one-shot parser (`parse_state_update`, `parse_service_state_update`)
/// starts a fresh dictionary per update and therefore rejects those
/// follow-ups; this decoder keeps one [`DeltaDecoder`] per
/// `(instance key, member)` (pir updates) or `(hold, member)` (chord frames),
/// exactly like upstream's per-member decoder. A pir instance generation
/// change under the same key starts a fresh dictionary (the old instance's
/// path ids must not leak into its replacement).
#[derive(Debug, Default)]
pub struct StateUpdateDecoder {
    streams: std::collections::HashMap<String, StreamState>,
    /// Monotonic use counter for the stream LRU.
    tick: u64,
}

/// One state stream's dictionary and LRU bookkeeping.
#[derive(Debug)]
struct StreamState {
    /// The instance generation the dictionary belongs to: a replacement
    /// instance reuses the `key` but is a different stream, so its path ids
    /// must not resolve against the old instance's dictionary.
    generation: i64,
    used: u64,
    decoder: DeltaDecoder,
}

impl StateUpdateDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// The decoder of `key` for `generation`, marking it most recently used.
    /// A generation change under the same key starts a fresh dictionary (the
    /// old instance's path ids must not resolve). When a NEW key would push
    /// the map past [`MAX_STATE_UPDATE_STREAMS`], the least recently used
    /// stream is evicted first.
    fn stream_mut(&mut self, key: String, generation: i64) -> &mut DeltaDecoder {
        self.tick = self.tick.saturating_add(1);
        let tick = self.tick;
        if self
            .streams
            .get(&key)
            .is_some_and(|stream| stream.generation != generation)
        {
            self.streams.remove(&key);
        }
        if !self.streams.contains_key(&key) && self.streams.len() >= MAX_STATE_UPDATE_STREAMS {
            if let Some(oldest) = self
                .streams
                .iter()
                .min_by_key(|(_, stream)| stream.used)
                .map(|(key, _)| key.clone())
            {
                self.streams.remove(&oldest);
            }
        }
        let entry = self.streams.entry(key).or_insert_with(|| StreamState {
            generation,
            used: tick,
            decoder: DeltaDecoder::new(),
        });
        entry.generation = generation;
        entry.used = tick;
        &mut entry.decoder
    }

    /// Decode one update, advancing its stream's path dictionary.
    pub fn decode(&mut self, value: &Value) -> Result<ServiceUpdate, SchemaError> {
        let obj = require_object(value, "service update")?;
        if obj.contains_key("t") {
            let hold = require_integer(field(obj, "hold", "chord state")?, "chord state.hold")?;
            let member = require_id(field(obj, "m", "chord state")?, "chord state.m")?;
            let key = format!("chord:{hold}:{member}");
            let decoder = self.stream_mut(key, hold);
            return parse_chord_state_update_with(value, decoder).map(ServiceUpdate::Chord);
        }
        let member = require_id(field(obj, "member", "state update")?, "state update.member")?;
        let instance = field_opt(obj, "instance")
            .map(|value| parse_service_instance_address(value, "state update.instance"))
            .transpose()?;
        let (instance, generation) = instance
            .map(|address| (address.key, address.generation))
            .unwrap_or_default();
        let key = format!("pir:{instance}:{member}");
        let decoder = self.stream_mut(key, generation);
        parse_state_update_with(value, decoder).map(ServiceUpdate::State)
    }

    /// Drop one stream's decoder (e.g. after the instance was replaced).
    pub fn forget(&mut self, stream: &str) {
        self.streams.remove(stream);
    }

    /// Number of retained stream dictionaries (test/profiling probe: the
    /// map is bounded by [`MAX_STATE_UPDATE_STREAMS`]).
    #[doc(hidden)]
    pub fn stream_count(&self) -> usize {
        self.streams.len()
    }
}

fn parse_service_member_snapshot(value: &Value, path: &str) -> Result<(), SchemaError> {
    let obj = require_object(value, path)?;
    let kind = require_string(field(obj, "kind", path)?, &format!("{path}.kind"))?;
    match kind {
        "method" => {
            validate_strict(obj, &["name", "kind"], path)?;
            require_id(field(obj, "name", path)?, &format!("{path}.name"))?;
        }
        "state" => {
            validate_strict(obj, &["name", "kind", "sequence", "ops"], path)?;
            require_id(field(obj, "name", path)?, &format!("{path}.name"))?;
            let sequence =
                require_integer(field(obj, "sequence", path)?, &format!("{path}.sequence"))?;
            if sequence < 0 {
                return Err(SchemaError::new(format!("{path}.sequence: must be >= 0")));
            }
            let ops = require_array(field(obj, "ops", path)?, &format!("{path}.ops"))?;
            parse_state_ops(ops, &format!("{path}.ops"))?;
        }
        _ => {
            return Err(SchemaError::new(format!(
                "{path}.kind: expected \"method\" or \"state\""
            )))
        }
    }
    Ok(())
}

fn parse_service_instance_snapshot(value: &Value, path: &str) -> Result<(), SchemaError> {
    let obj = require_object(value, path)?;
    validate_strict(obj, &["instance", "members"], path)?;
    if let Some(address) = field_opt(obj, "instance") {
        parse_service_instance_address(address, &format!("{path}.instance"))?;
    }
    let members = require_array(field(obj, "members", path)?, &format!("{path}.members"))?;
    for (index, member) in members.iter().enumerate() {
        parse_service_member_snapshot(member, &format!("{path}.members[{index}]"))?;
    }
    Ok(())
}

/// Validate a chord service provider update (upstream `parseServiceProviderUpdate`).
pub fn parse_service_provider_update(value: &Value) -> Result<(), SchemaError> {
    let obj = require_object(value, "service provider update")?;
    // An upstream chord state frame names itself with `t`, while pir's state
    // update carries `type`; accept both payloads.
    if obj.contains_key("t") {
        parse_chord_state_update(value)?;
        return Ok(());
    }
    let t = require_string(field(obj, "type", "service provider update")?, ".type")?;
    match t {
        "state" => {
            validate_strict(
                obj,
                &["type", "member", "sequence", "ops", "instance"],
                "state update",
            )?;
            require_id(field(obj, "member", "state update")?, ".member")?;
            let sequence = require_integer(field(obj, "sequence", "state update")?, ".sequence")?;
            if sequence < 1 {
                return Err(SchemaError::new(format!(
                    ".sequence: must be >= 1, got {sequence}"
                )));
            }
            let ops = require_array(field(obj, "ops", "state update")?, ".ops")?;
            parse_state_ops(ops, ".ops")?;
            if let Some(address) = field_opt(obj, "instance") {
                parse_service_instance_address(address, ".instance")?;
            }
            Ok(())
        }
        "unavailable" => {
            validate_strict(obj, &["type"], "unavailable update")?;
            Ok(())
        }
        "replaced" => {
            validate_strict(obj, &["type", "snapshot"], "replacement update")?;
            parse_service_instance_snapshot(
                field(obj, "snapshot", "replacement update")?,
                ".snapshot",
            )
        }
        "spawned" => {
            validate_strict(obj, &["type", "instance"], "spawn update")?;
            parse_service_instance_snapshot(field(obj, "instance", "spawn update")?, ".instance")
        }
        "closed" => {
            validate_strict(obj, &["type", "instance"], "close update")?;
            parse_service_instance_address(field(obj, "instance", "close update")?, ".instance")
                .map(|_| ())
        }
        _ => Err(SchemaError::new(
            "service provider update: expected state/unavailable/replaced/spawned/closed",
        )),
    }
}
