//! Command executor — transactional event processing for command kinds.
//!
//! Command kinds (41010–41012, 30620, 46020, 46030–46031) are processed
//! transactionally: validate → begin tx → insert event → execute mutations → commit.
//!
//! SECURITY: This module is only reachable AFTER the ingest pipeline has verified:
//! 1. Event signature (verify_event)
//! 2. Timestamp freshness (±15 min)
//! 3. Pubkey/auth identity match
//! 4. Per-kind scope authorization

use std::sync::Arc;

use chrono::Utc;
use nostr::Event;
use sha2::{Digest, Sha256};
use tracing::warn;
use uuid::Uuid;

use buzz_core::kind::*;
use buzz_core::tenant::{CommunityId, TenantContext};
use buzz_datastore_tracing::datastore_span;
use buzz_db::workflow::{ApprovalStatus, RunStatus};
use buzz_db::DbError;
use buzz_workflow::executor::TriggerContext;

use crate::state::AppState;
use crate::webhook_secret;

use super::ingest::{extract_channel_id, IngestAuth, IngestError, IngestResult};
use super::side_effects::{
    emit_group_discovery_events, emit_membership_notification, emit_system_message,
    publish_dm_visibility_snapshot,
};

/// Route a command-kind event to the appropriate handler.
pub async fn handle_command(
    tenant: &TenantContext,
    state: &Arc<AppState>,
    event: Event,
    auth: IngestAuth,
) -> Result<IngestResult, IngestError> {
    // Ensure the authenticated user exists in the users table (foreign key requirement).
    // The old REST handlers did this via extract_auth_context; command executor must do it explicitly.
    let pubkey_bytes = auth.pubkey().to_bytes().to_vec();
    match state
        .db
        .ensure_user(tenant.community(), &pubkey_bytes)
        .await
    {
        Ok(true) => {
            metrics::counter!(
                "buzz_users_created_total",
                "community" => tenant.host().to_owned()
            )
            .increment(1);
        }
        Ok(false) => {}
        Err(e) => {
            tracing::warn!("command_executor: ensure_user failed: {e}");
        }
    }

    let kind = event.kind.as_u16() as u32;
    match kind {
        KIND_DM_OPEN => handle_dm_open(tenant, state, &event, &auth).await,
        KIND_DM_ADD_MEMBER => handle_dm_add_member(tenant, state, &event, &auth).await,
        KIND_DM_HIDE => handle_dm_hide(tenant, state, &event, &auth).await,
        KIND_WORKFLOW_DEF => handle_workflow_def(tenant, state, &event, &auth).await,
        KIND_WORKFLOW_TRIGGER => handle_workflow_trigger(tenant, state, &event, &auth).await,
        KIND_APPROVAL_GRANT => handle_approval_grant(tenant, state, &event, &auth).await,
        KIND_APPROVAL_DENY => handle_approval_deny(tenant, state, &event, &auth).await,
        _ => Err(IngestError::Rejected(format!(
            "unknown command kind: {kind}"
        ))),
    }
}

/// Result of persisting a command event: either a duplicate (already processed)
/// or an open transaction that the handler must commit after executing mutations.
enum PersistResult {
    /// Event was already processed — return idempotent success.
    Duplicate,
    /// Event inserted — transaction is open, handler must commit after mutations.
    Inserted(sqlx::Transaction<'static, sqlx::Postgres>),
}

/// Persist a command event inside a transaction. Returns the OPEN transaction
/// as an idempotency guard — if the event was already stored, `Duplicate` is
/// returned and the handler skips execution.
///
/// If the event is a duplicate (ON CONFLICT DO NOTHING), the transaction is
/// rolled back and `PersistResult::Duplicate` is returned — no mutations needed.
///
/// NOTE: Domain mutations (open_dm, upsert_workflow, etc.) execute on the
/// connection pool, NOT inside this transaction. The pattern is idempotent but
/// not strictly atomic: if a mutation succeeds but commit fails, the mutation
/// persists without the event record. On retry, the event INSERT succeeds
/// (no conflict), and the mutation re-executes — which is safe for idempotent
/// operations (open_dm, hide_dm, update_approval, upsert_workflow).
#[datastore_span(name = "persist_command_event", system = "postgresql")]
async fn persist_command_event(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    event: &Event,
    channel_id_override: Option<Uuid>,
) -> Result<PersistResult, IngestError> {
    let channel_id = channel_id_override.or_else(|| extract_channel_id(event));

    let mut tx = state
        .db
        .begin_transaction()
        .await
        .map_err(|e| IngestError::Internal(format!("error: begin transaction: {e}")))?;
    buzz_deletion::store(&state.db)
        .guard_transaction(&mut tx, tenant.community())
        .await
        .map_err(|error| {
            IngestError::Rejected(format!("restricted: community writes are fenced: {error}"))
        })?;

    // INSERT with ON CONFLICT DO NOTHING — idempotency guard.
    let id_bytes = event.id.as_bytes();
    let pubkey_bytes = event.pubkey.to_bytes();
    let sig_bytes = event.sig.serialize();
    let tags_json = serde_json::to_value(&event.tags)
        .map_err(|e| IngestError::Internal(format!("error: serialize tags: {e}")))?;
    let kind_i32 = event.kind.as_u16() as i32;
    let created_at_secs = event.created_at.as_secs() as i64;
    let created_at = chrono::DateTime::from_timestamp(created_at_secs, 0).ok_or_else(|| {
        IngestError::Rejected(format!("invalid: bad timestamp {created_at_secs}"))
    })?;
    let received_at = chrono::Utc::now();

    // Extract d_tag for parameterized replaceable kinds (NIP-33).
    let d_tag = buzz_db::event::extract_d_tag(event);
    if let Some(ref d_tag) = d_tag {
        if d_tag.len() > buzz_db::event::D_TAG_MAX_LEN {
            return Err(IngestError::Rejected(format!(
                "invalid: d tag too long ({} bytes, max {})",
                d_tag.len(),
                buzz_db::event::D_TAG_MAX_LEN,
            )));
        }

        // Command kinds normally use plain insert semantics, but workflow
        // definitions are NIP-33 events. Serialize writers for the same
        // coordinate and reject stale writes before executing the domain
        // mutation, otherwise old updates can overwrite newer workflow state.
        let lock_key = {
            let mut h: u64 = 0xcbf29ce484222325;
            for b in tenant.community().as_uuid().as_bytes() {
                h ^= *b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            for b in kind_i32.to_le_bytes() {
                h ^= b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            for b in pubkey_bytes.as_slice() {
                h ^= *b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            for b in d_tag.as_bytes() {
                h ^= *b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            h as i64
        };

        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(lock_key)
            .execute(tx.as_mut())
            .await
            .map_err(|e| IngestError::Internal(format!("error: lock event coordinate: {e}")))?;

        let existing: Option<(chrono::DateTime<chrono::Utc>, Vec<u8>)> = sqlx::query_as(
            "SELECT created_at, id FROM events \
             WHERE community_id = $1 AND kind = $2 AND pubkey = $3 AND d_tag = $4 AND deleted_at IS NULL \
             ORDER BY created_at DESC, id ASC LIMIT 1",
        )
        .bind(tenant.community().as_uuid())
        .bind(kind_i32)
        .bind(pubkey_bytes.as_slice())
        .bind(d_tag)
        .fetch_optional(tx.as_mut())
        .await
        .map_err(|e| IngestError::Internal(format!("error: query event coordinate: {e}")))?;

        let incoming_id = event.id.as_bytes().as_slice();
        if let Some((existing_ts, existing_id)) = existing {
            let dominated = created_at < existing_ts
                || (created_at == existing_ts && incoming_id >= existing_id.as_slice());
            if dominated {
                return Ok(PersistResult::Duplicate);
            }

            sqlx::query(
                "UPDATE events SET deleted_at = NOW() \
                 WHERE community_id = $1 AND kind = $2 AND pubkey = $3 AND d_tag = $4 AND deleted_at IS NULL",
            )
            .bind(tenant.community().as_uuid())
            .bind(kind_i32)
            .bind(pubkey_bytes.as_slice())
            .bind(d_tag)
            .execute(tx.as_mut())
            .await
            .map_err(|e| IngestError::Internal(format!("error: replace old event: {e}")))?;
        }
    }

    let result = sqlx::query(
        r#"
        INSERT INTO events (community_id, id, pubkey, created_at, kind, tags, content, sig, received_at, channel_id, d_tag)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(tenant.community().as_uuid())
    .bind(id_bytes.as_slice())
    .bind(pubkey_bytes.as_slice())
    .bind(created_at)
    .bind(kind_i32)
    .bind(&tags_json)
    .bind(&event.content)
    .bind(sig_bytes.as_slice())
    .bind(received_at)
    .bind(channel_id)
    .bind(d_tag.as_deref())
    .execute(tx.as_mut())
    .await
    .map_err(|e| IngestError::Internal(format!("error: insert event: {e}")))?;

    if result.rows_affected() == 0 {
        // Duplicate — rollback (implicit on drop) and signal idempotent success.
        Ok(PersistResult::Duplicate)
    } else {
        Ok(PersistResult::Inserted(tx))
    }
}

/// Extract all `p` tag values (hex pubkeys) from an event.
fn extract_p_tags(event: &Event) -> Vec<String> {
    event
        .tags
        .iter()
        .filter_map(|t| {
            if t.kind().to_string() == "p" {
                t.content().map(|s| s.to_string())
            } else {
                None
            }
        })
        .collect()
}

/// Extract the first `h` tag value (channel UUID) from an event.
fn extract_h_tag(event: &Event) -> Option<String> {
    event.tags.iter().find_map(|t| {
        if t.kind().to_string() == "h" {
            t.content().map(|s| s.to_string())
        } else {
            None
        }
    })
}

/// Extract the first `d` tag value from an event.
fn extract_d_tag(event: &Event) -> Option<String> {
    event.tags.iter().find_map(|t| {
        if t.kind().to_string() == "d" {
            t.content().map(|s| s.to_string())
        } else {
            None
        }
    })
}

/// Extract the first `e` tag value from an event.
fn extract_e_tag(event: &Event) -> Option<String> {
    event.tags.iter().find_map(|t| {
        if t.kind().to_string() == "e" {
            t.content().map(|s| s.to_string())
        } else {
            None
        }
    })
}

/// Extract a tag value by name.
fn extract_tag(event: &Event, tag_name: &str) -> Option<String> {
    event.tags.iter().find_map(|t| {
        if t.kind().to_string() == tag_name {
            t.content().map(|s| s.to_string())
        } else {
            None
        }
    })
}

/// Decode a hex pubkey string to 32 bytes.
fn decode_pubkey(hex_str: &str) -> Result<Vec<u8>, IngestError> {
    let bytes = hex::decode(hex_str)
        .map_err(|_| IngestError::Rejected(format!("invalid: bad pubkey hex: {hex_str}")))?;
    if bytes.len() != 32 {
        return Err(IngestError::Rejected(format!(
            "invalid: pubkey must be 32 bytes: {hex_str}"
        )));
    }
    Ok(bytes)
}

/// Compute SHA-256 hash of a string, returning raw bytes.
fn compute_definition_hash(json_str: &str) -> Vec<u8> {
    Sha256::digest(json_str.as_bytes()).to_vec()
}

async fn handle_dm_open(
    tenant: &TenantContext,
    state: &Arc<AppState>,
    event: &Event,
    auth: &IngestAuth,
) -> Result<IngestResult, IngestError> {
    let self_bytes = auth.pubkey().to_bytes().to_vec();
    let self_hex = hex::encode(&self_bytes);

    // 1. Extract participant pubkeys from `p` tags
    let p_tags = extract_p_tags(event);

    // 2. Validate: at least 1 other participant, max 8 others (9 total)
    if p_tags.is_empty() {
        return Err(IngestError::Rejected(
            "invalid: pubkeys must contain at least 1 other participant".into(),
        ));
    }
    if p_tags.len() > 8 {
        return Err(IngestError::Rejected(
            "invalid: pubkeys may contain at most 8 other participants (9 total)".into(),
        ));
    }

    // Decode all provided pubkeys
    let mut other_bytes: Vec<Vec<u8>> = Vec::with_capacity(p_tags.len());
    for hex_str in &p_tags {
        other_bytes.push(decode_pubkey(hex_str)?);
    }

    // 3. Build full participant set (self + others, deduplicated)
    let mut all_bytes: Vec<Vec<u8>> = vec![self_bytes.clone()];
    for ob in &other_bytes {
        if !all_bytes.iter().any(|b| b == ob) {
            all_bytes.push(ob.clone());
        }
    }

    // Persist the command event (idempotency) — returns open transaction
    let tx = match persist_command_event(state, tenant, event, None).await? {
        PersistResult::Duplicate => {
            return Ok(IngestResult {
                event_id: event.id.to_hex(),
                accepted: true,
                message: "duplicate: already processed".into(),
            });
        }
        PersistResult::Inserted(tx) => tx,
    };

    // 4. Execute: open_dm
    let all_refs: Vec<&[u8]> = all_bytes.iter().map(|b| b.as_slice()).collect();
    let (channel, was_created) = state
        .db
        .open_dm(tenant.community(), &all_refs, &self_bytes)
        .await
        .map_err(|e| IngestError::Internal(format!("error: db open_dm: {e}")))?;

    // Commit: event + mutation succeeded atomically.
    tx.commit()
        .await
        .map_err(|e| IngestError::Internal(format!("error: commit transaction: {e}")))?;

    // 5. Side effects if newly created (post-commit, best-effort)
    if was_created {
        metrics::counter!(
            "buzz_channels_created_total",
            "community" => tenant.host().to_owned(),
            "type" => "dm"
        )
        .increment(1);

        // Invalidate caches for all participants
        for pk in &all_bytes {
            state.invalidate_membership(tenant, channel.id, pk);
        }

        let participant_hexes: Vec<String> = all_bytes.iter().map(hex::encode).collect();
        if let Err(e) = emit_system_message(
            tenant,
            state,
            channel.id,
            serde_json::json!({
                "type": "dm_created",
                "actor": self_hex,
                "participants": participant_hexes,
            }),
        )
        .await
        {
            warn!("DM open: system message failed: {e}");
        }

        if let Err(e) = emit_group_discovery_events(tenant, state, channel.id).await {
            warn!(channel = %channel.id, "DM open: discovery emission failed: {e}");
        }

        for participant in &all_bytes {
            if let Err(e) = emit_membership_notification(
                tenant,
                state,
                channel.id,
                participant,
                &self_bytes,
                KIND_MEMBER_ADDED_NOTIFICATION,
            )
            .await
            {
                warn!("DM open: membership notification failed: {e}");
            }
        }
    } else {
        // Re-open of an existing DM cleared the caller's hidden_at; refresh
        // their NIP-DV snapshot so the DM reappears in the sidebar.
        if let Err(e) = publish_dm_visibility_snapshot(tenant, state, &self_bytes).await {
            warn!("DM re-open: visibility snapshot failed: {e}");
        }
    }

    // 6. Return response
    Ok(IngestResult {
        event_id: event.id.to_hex(),
        accepted: true,
        message: format!(
            "response:{}",
            serde_json::json!({
                "channel_id": channel.id.to_string(),
                "created": was_created,
            })
        ),
    })
}

async fn handle_dm_add_member(
    tenant: &TenantContext,
    state: &Arc<AppState>,
    event: &Event,
    auth: &IngestAuth,
) -> Result<IngestResult, IngestError> {
    let self_bytes = auth.pubkey().to_bytes().to_vec();

    // 1. Extract target channel from `h` tag, new member pubkeys from `p` tags
    let channel_id_str = extract_h_tag(event)
        .ok_or_else(|| IngestError::Rejected("invalid: missing h tag (channel_id)".into()))?;
    let channel_id = Uuid::parse_str(&channel_id_str)
        .map_err(|_| IngestError::Rejected("invalid: bad channel_id format".into()))?;

    let p_tags = extract_p_tags(event);
    if p_tags.is_empty() {
        return Err(IngestError::Rejected(
            "invalid: must specify at least 1 new participant in p tags".into(),
        ));
    }

    // 2. Validate caller is member of existing DM
    let is_member = state
        .is_member_cached(tenant.community(), channel_id, &self_bytes)
        .await
        .map_err(|e| IngestError::Internal(format!("error: membership check: {e}")))?;
    if !is_member {
        return Err(IngestError::Rejected(
            "forbidden: not a member of this DM".into(),
        ));
    }

    // 3. Validate channel is type "dm"
    let existing_channel = state
        .db
        .get_channel(tenant.community(), channel_id)
        .await
        .map_err(|_| IngestError::Rejected("invalid: DM not found".into()))?;
    if existing_channel.channel_type != "dm" {
        return Err(IngestError::Rejected("invalid: channel is not a DM".into()));
    }

    // 4. Get existing members, merge with new
    let existing_members = state
        .db
        .get_members(tenant.community(), channel_id)
        .await
        .map_err(|e| IngestError::Internal(format!("error: get members: {e}")))?;

    let mut all_bytes: Vec<Vec<u8>> = existing_members.into_iter().map(|m| m.pubkey).collect();

    // Decode and merge new pubkeys
    for hex_str in &p_tags {
        let bytes = decode_pubkey(hex_str)?;
        if !all_bytes.iter().any(|b| b == &bytes) {
            all_bytes.push(bytes);
        }
    }

    // 5. Enforce max 9 participants
    if all_bytes.len() > 9 {
        return Err(IngestError::Rejected(
            "invalid: DM supports at most 9 participants".into(),
        ));
    }

    // Persist the command event — returns open transaction
    let tx = match persist_command_event(state, tenant, event, None).await? {
        PersistResult::Duplicate => {
            return Ok(IngestResult {
                event_id: event.id.to_hex(),
                accepted: true,
                message: "duplicate: already processed".into(),
            });
        }
        PersistResult::Inserted(tx) => tx,
    };

    // 6. Execute: open_dm with expanded set (creates NEW DM — DM sets are immutable)
    let all_refs: Vec<&[u8]> = all_bytes.iter().map(|b| b.as_slice()).collect();
    let (new_channel, was_created) = state
        .db
        .open_dm(tenant.community(), &all_refs, &self_bytes)
        .await
        .map_err(|e| IngestError::Internal(format!("error: db open_dm: {e}")))?;

    // Commit: event + mutation succeeded atomically.
    tx.commit()
        .await
        .map_err(|e| IngestError::Internal(format!("error: commit transaction: {e}")))?;

    // 7. Cache invalidation + notifications for new DM (post-commit, best-effort)
    if was_created {
        metrics::counter!(
            "buzz_channels_created_total",
            "community" => tenant.host().to_owned(),
            "type" => "dm"
        )
        .increment(1);

        for pk in &all_bytes {
            state.invalidate_membership(tenant, new_channel.id, pk);
        }

        if let Err(e) = emit_group_discovery_events(tenant, state, new_channel.id).await {
            warn!(channel = %new_channel.id, "DM add_member: discovery emission failed: {e}");
        }

        for participant_bytes in &all_bytes {
            if let Err(e) = emit_membership_notification(
                tenant,
                state,
                new_channel.id,
                participant_bytes,
                &self_bytes,
                KIND_MEMBER_ADDED_NOTIFICATION,
            )
            .await
            {
                warn!("DM add_member: membership notification failed: {e}");
            }
        }
    }

    // 8. Return response
    Ok(IngestResult {
        event_id: event.id.to_hex(),
        accepted: true,
        message: format!(
            "response:{}",
            serde_json::json!({
                "channel_id": new_channel.id.to_string(),
            })
        ),
    })
}

async fn handle_dm_hide(
    tenant: &TenantContext,
    state: &Arc<AppState>,
    event: &Event,
    auth: &IngestAuth,
) -> Result<IngestResult, IngestError> {
    let self_bytes = auth.pubkey().to_bytes().to_vec();

    // 1. Extract channel from `h` tag
    let channel_id_str = extract_h_tag(event)
        .ok_or_else(|| IngestError::Rejected("invalid: missing h tag (channel_id)".into()))?;
    let channel_id = Uuid::parse_str(&channel_id_str)
        .map_err(|_| IngestError::Rejected("invalid: bad channel_id format".into()))?;

    // 2. Validate caller is member of the DM
    let is_member = state
        .is_member_cached(tenant.community(), channel_id, &self_bytes)
        .await
        .map_err(|e| IngestError::Internal(format!("error: membership check: {e}")))?;
    if !is_member {
        return Err(IngestError::Rejected(
            "forbidden: not a member of this DM".into(),
        ));
    }

    // 3. Validate channel is type "dm"
    let channel = state
        .db
        .get_channel(tenant.community(), channel_id)
        .await
        .map_err(|_| IngestError::Rejected("invalid: DM not found".into()))?;
    if channel.channel_type != "dm" {
        return Err(IngestError::Rejected("invalid: channel is not a DM".into()));
    }

    // Persist the command event — returns open transaction
    let tx = match persist_command_event(state, tenant, event, None).await? {
        PersistResult::Duplicate => {
            return Ok(IngestResult {
                event_id: event.id.to_hex(),
                accepted: true,
                message: "duplicate: already processed".into(),
            });
        }
        PersistResult::Inserted(tx) => tx,
    };

    // 4. Execute: hide_dm
    state
        .db
        .hide_dm(tenant.community(), channel_id, &self_bytes)
        .await
        .map_err(|e| IngestError::Internal(format!("error: db hide_dm: {e}")))?;

    // Commit: event + mutation succeeded atomically.
    tx.commit()
        .await
        .map_err(|e| IngestError::Internal(format!("error: commit transaction: {e}")))?;

    // 5. Side effect (post-commit, best-effort): refresh the caller's NIP-DV
    // visibility snapshot so clients can filter this DM out of the sidebar.
    if let Err(e) = publish_dm_visibility_snapshot(tenant, state, &self_bytes).await {
        warn!("DM hide: visibility snapshot failed: {e}");
    }

    // 6. Return response
    Ok(IngestResult {
        event_id: event.id.to_hex(),
        accepted: true,
        message: "{}".into(),
    })
}

async fn handle_workflow_def(
    tenant: &TenantContext,
    state: &Arc<AppState>,
    event: &Event,
    auth: &IngestAuth,
) -> Result<IngestResult, IngestError> {
    let self_bytes = auth.pubkey().to_bytes().to_vec();

    // 1. Extract channel and the canonical workflow UUID from the NIP-33 d-tag.
    let channel_id_str = extract_h_tag(event)
        .ok_or_else(|| IngestError::Rejected("invalid: missing h tag (channel_id)".into()))?;
    let channel_id = Uuid::parse_str(&channel_id_str)
        .map_err(|_| IngestError::Rejected("invalid: bad channel_id format".into()))?;

    let workflow_id_str = extract_d_tag(event)
        .ok_or_else(|| IngestError::Rejected("invalid: missing d tag (workflow_id)".into()))?;
    let workflow_id = Uuid::parse_str(&workflow_id_str)
        .map_err(|_| IngestError::Rejected("invalid: bad workflow_id format".into()))?;

    // 2. Validate caller has channel access (minimum: is a member)
    let is_member = state
        .is_member_cached(tenant.community(), channel_id, &self_bytes)
        .await
        .map_err(|e| IngestError::Internal(format!("error: membership check: {e}")))?;
    if !is_member {
        return Err(IngestError::Rejected(
            "forbidden: not a member of this channel".into(),
        ));
    }

    // 3. Parse YAML from event.content
    let (def, definition_json_str) = buzz_workflow::WorkflowEngine::parse_yaml(&event.content)
        .map_err(|e| IngestError::Rejected(format!("invalid: workflow YAML parse error: {e}")))?;
    let workflow_name = extract_tag(event, "name").unwrap_or_else(|| def.name.clone());

    // SEC-006: definitions with exfiltration-capable actions (call_webhook)
    // require elevated channel authority to save — plain membership is not
    // enough, because the workflow will forward channel content outward with
    // the owner's standing authority. Fail-closed on lookup errors.
    if def.requires_elevated_authority() {
        let role = state
            .db
            .get_member_role(tenant.community(), channel_id, &self_bytes)
            .await
            .map_err(|e| IngestError::Internal(format!("error: role check: {e}")))?;
        if !matches!(role.as_deref(), Some("owner") | Some("admin")) {
            return Err(IngestError::Rejected(
                "forbidden: workflows with call_webhook actions require the owner or admin role"
                    .into(),
            ));
        }
    }

    let mut definition_json: serde_json::Value = serde_json::from_str(&definition_json_str)
        .map_err(|e| IngestError::Internal(format!("error: json parse of definition: {e}")))?;

    let existing_workflow = match state.db.get_workflow(tenant.community(), workflow_id).await {
        Ok(workflow) => {
            if workflow.owner_pubkey != self_bytes || workflow.channel_id != Some(channel_id) {
                return Err(IngestError::Rejected(
                    "forbidden: workflow belongs to a different owner or channel".into(),
                ));
            }
            Some(workflow)
        }
        Err(DbError::NotFound(_)) => None,
        Err(e) => {
            return Err(IngestError::Internal(format!(
                "error: db get_workflow: {e}"
            )));
        }
    };

    // Preserve the existing webhook secret across updates. A new secret is
    // returned only when the workflow first gains a webhook trigger.
    let webhook_secret = if matches!(def.trigger, buzz_workflow::TriggerDef::Webhook) {
        let existing_secret = existing_workflow
            .as_ref()
            .and_then(|workflow| webhook_secret::extract_secret(&workflow.definition));
        let secret = existing_secret.unwrap_or_else(webhook_secret::generate_webhook_secret);
        webhook_secret::inject_secret(&mut definition_json, &secret);
        if existing_workflow
            .as_ref()
            .and_then(|workflow| webhook_secret::extract_secret(&workflow.definition))
            .is_none()
        {
            Some(secret)
        } else {
            None
        }
    } else {
        None
    };

    // Compute hash AFTER secret injection
    let definition_json_final = serde_json::to_string(&definition_json)
        .map_err(|e| IngestError::Internal(format!("error: json serialize: {e}")))?;
    let hash = compute_definition_hash(&definition_json_final);

    // Persist the command event — returns open transaction
    let tx = match persist_command_event(state, tenant, event, None).await? {
        PersistResult::Duplicate => {
            return Ok(IngestResult {
                event_id: event.id.to_hex(),
                accepted: true,
                message: "duplicate: already processed".into(),
            });
        }
        PersistResult::Inserted(tx) => tx,
    };

    // 4. Execute: upsert by the NIP-33 d-tag UUID. A retry updates the same
    // row instead of creating another enabled workflow that would fan out on
    // every matching event. The workflow's community is the request's
    // server-bound tenant — never re-derived from the (client-supplied) channel
    // id. `community_of_channel(channel_id)` is ambiguous when the same channel
    // UUID exists in two communities and could mint the workflow under the wrong
    // tenant; `tenant.community()` is the authoritative owner. We then verify the
    // channel actually exists *inside that community* (scoped `get_channel`),
    // which fails closed if the client named a channel that belongs to a
    // different community — the same guarantee the `(community_id, channel_id)`
    // composite FK enforces on insert, surfaced here as a clean rejection.
    let community_id = tenant.community();
    state
        .db
        .get_channel(community_id, channel_id)
        .await
        .map_err(|_| IngestError::Rejected("invalid: workflow channel not found".into()))?;

    state
        .db
        .upsert_workflow(
            community_id,
            workflow_id,
            Some(channel_id),
            &self_bytes,
            &workflow_name,
            &definition_json_final,
            &hash,
        )
        .await
        .map_err(|e| match e {
            DbError::AccessDenied(_) => IngestError::Rejected(
                "forbidden: workflow belongs to a different owner or channel".into(),
            ),
            other => IngestError::Internal(format!("error: db upsert_workflow: {other}")),
        })?;

    // Drop the trigger-path cache entry so the new/updated definition fires on
    // the next matching event instead of after the cache TTL.
    state
        .workflow_engine
        .invalidate_channel_workflows(community_id, channel_id);

    // Commit the event transaction after the idempotent workflow upsert succeeds.
    tx.commit()
        .await
        .map_err(|e| IngestError::Internal(format!("error: commit transaction: {e}")))?;

    // 5. Return response
    let mut resp = serde_json::json!({
        "workflow_id": workflow_id.to_string(),
    });
    if let Some(secret) = webhook_secret {
        resp["webhook_secret"] = serde_json::Value::String(secret);
    }

    Ok(IngestResult {
        event_id: event.id.to_hex(),
        accepted: true,
        message: format!("response:{}", resp),
    })
}

async fn handle_workflow_trigger(
    tenant: &TenantContext,
    state: &Arc<AppState>,
    event: &Event,
    auth: &IngestAuth,
) -> Result<IngestResult, IngestError> {
    let self_bytes = auth.pubkey().to_bytes().to_vec();

    // 1. Extract workflow reference from `d` tag or `e` tag
    let workflow_id_str = extract_d_tag(event)
        .or_else(|| extract_e_tag(event))
        .ok_or_else(|| {
            IngestError::Rejected("invalid: missing workflow reference (d or e tag)".into())
        })?;
    let workflow_id = Uuid::parse_str(&workflow_id_str)
        .map_err(|_| IngestError::Rejected("invalid: bad workflow_id format".into()))?;

    // 2. Validate workflow exists — scoped to the caller's community. The same
    // workflow UUID can exist in another community; a bare-id lookup could load
    // B's workflow and then satisfy the membership check below against B's
    // colliding channel, letting B trigger A's workflow.
    let community_id = tenant.community();
    let workflow = state
        .db
        .get_workflow(community_id, workflow_id)
        .await
        .map_err(|_| IngestError::Rejected("invalid: workflow not found".into()))?;

    // 3. Manual triggers execute with the workflow owner's authority, so only
    // the owner may start them. Channel membership alone is insufficient: a
    // member could otherwise invoke another user's webhook or message actions.
    if workflow.owner_pubkey != self_bytes {
        return Err(IngestError::Rejected(
            "forbidden: not authorized to trigger this workflow".into(),
        ));
    }

    // SEC-006: manual triggers must honor the workflow's lifecycle state and
    // recheck the owner's *current* channel authority before creating a run.
    // Without this, a disabled workflow — including one disabled because its
    // owner was removed from the channel — could still be fired by the owner.
    if !workflow.enabled || workflow.status != buzz_db::workflow::WorkflowStatus::Active {
        return Err(IngestError::Rejected(
            "forbidden: workflow is disabled or inactive".into(),
        ));
    }
    let def: buzz_workflow::WorkflowDef = serde_json::from_value(workflow.definition.clone())
        .map_err(|e| IngestError::Internal(format!("error: corrupt workflow definition: {e}")))?;
    let Some(wf_channel_id) = workflow.channel_id else {
        // No channel scope means no channel authority to verify — fail closed.
        return Err(IngestError::Rejected(
            "forbidden: workflow has no channel scope".into(),
        ));
    };
    state
        .workflow_engine
        .check_owner_authority(community_id, wf_channel_id, &workflow.owner_pubkey, &def)
        .await
        .map_err(|_| {
            IngestError::Rejected("forbidden: not authorized to trigger this workflow".into())
        })?;

    // Persist the command event under the workflow channel even though the
    // trigger event itself only carries the workflow UUID. Storing channel
    // triggers as global events leaks workflow IDs to unrelated relay members.
    let tx = match persist_command_event(state, tenant, event, workflow.channel_id).await? {
        PersistResult::Duplicate => {
            return Ok(IngestResult {
                event_id: event.id.to_hex(),
                accepted: true,
                message: "duplicate: already processed".into(),
            });
        }
        PersistResult::Inserted(tx) => tx,
    };

    // 4. Execute: create workflow run
    let mut trigger_ctx = TriggerContext {
        channel_id: workflow
            .channel_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
        author: hex::encode(&self_bytes),
        ..Default::default()
    };
    if !event.content.is_empty() {
        if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(&event.content) {
            for (k, v) in map {
                let val_str = match v {
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                };
                trigger_ctx.webhook_fields.insert(k, val_str);
            }
        }
    }
    let trigger_ctx_json = serde_json::to_value(&trigger_ctx).ok();

    let event_id_bytes = event.id.as_bytes().to_vec();
    // The checks above judged a snapshot, and authority and persistence were
    // awaited since. The insert decides again, in its own statement: no run
    // if the workflow was switched off (either switch) or its definition was
    // re-saved in between. Returning here drops `tx`, so the command event is
    // not stored either.
    let run_id = state
        .db
        .create_workflow_run_if_runnable(
            community_id,
            workflow_id,
            &workflow.definition_hash,
            Some(&event_id_bytes),
            trigger_ctx_json.as_ref(),
        )
        .await
        .map_err(|e| IngestError::Internal(format!("error: db create_workflow_run: {e}")))?
        .ok_or_else(|| {
            IngestError::Rejected(
                "forbidden: workflow was disabled or changed before the run was created".into(),
            )
        })?;

    // Commit: event + run creation succeeded atomically.
    tx.commit()
        .await
        .map_err(|e| IngestError::Internal(format!("error: commit transaction: {e}")))?;

    // 5. Spawn workflow execution
    let engine = Arc::clone(&state.workflow_engine);
    let db = state.db.clone();
    let def_value = workflow.definition.clone();
    let trigger_ctx_clone = trigger_ctx.clone();
    tokio::spawn(async move {
        let def: buzz_workflow::WorkflowDef = match serde_json::from_value(def_value) {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("workflow_trigger: failed to parse definition: {e}");
                if let Err(db_err) = db
                    .update_workflow_run(
                        community_id,
                        run_id,
                        RunStatus::Failed,
                        0,
                        &serde_json::json!([]),
                        Some(buzz_db::workflow::WorkflowRunFailure {
                            code: "invalid_definition",
                            message: &format!("definition parse error: {e}"),
                        }),
                    )
                    .await
                {
                    tracing::error!("workflow_trigger: failed to mark run as failed: {db_err}");
                }
                return;
            }
        };

        let result = buzz_workflow::executor::execute_from_step(
            &engine,
            community_id,
            run_id,
            &def,
            &trigger_ctx_clone,
            0,
            None,
        )
        .await;
        engine
            .finalize_run(community_id, run_id, result, None)
            .await;
    });

    // 6. Return response
    Ok(IngestResult {
        event_id: event.id.to_hex(),
        accepted: true,
        message: format!(
            "response:{}",
            serde_json::json!({
                "run_id": run_id.to_string(),
            })
        ),
    })
}

/// Enforce the approver_spec field against the requesting pubkey.
///
/// Accepted specs:
/// - `""` or `"any"` — any authenticated user may approve.
/// - 64-char lowercase hex string — only that exact pubkey may approve.
///
/// All other formats are rejected (fail-closed).
fn check_approver_spec(approver_spec: &str, requester_hex: &str) -> Result<(), IngestError> {
    let spec = approver_spec.trim();

    // Empty or "any" — anyone may approve
    if spec.is_empty() || spec == "any" {
        return Ok(());
    }

    // Exact pubkey match (64-char hex, case-insensitive)
    if spec.len() == 64 && spec.chars().all(|c| c.is_ascii_hexdigit()) {
        if requester_hex.to_lowercase() == spec.to_lowercase() {
            return Ok(());
        }
        return Err(IngestError::Rejected(
            "forbidden: not the designated approver for this request".into(),
        ));
    }

    // Role-based or unrecognised — fail closed
    Err(IngestError::Rejected(format!(
        "forbidden: approver spec '{}' is not yet supported",
        spec
    )))
}

async fn handle_approval_grant(
    tenant: &TenantContext,
    state: &Arc<AppState>,
    event: &Event,
    auth: &IngestAuth,
) -> Result<IngestResult, IngestError> {
    let self_bytes = auth.pubkey().to_bytes().to_vec();
    let self_hex = hex::encode(&self_bytes);

    // 1. Extract approval reference from `e` tag (references the approval-requested event)
    //    or `d` tag (contains the token hash hex)
    let token_hash_hex = extract_d_tag(event)
        .or_else(|| extract_e_tag(event))
        .ok_or_else(|| {
            IngestError::Rejected("invalid: missing approval reference (d or e tag)".into())
        })?;

    let token_hash = hex::decode(&token_hash_hex)
        .map_err(|_| IngestError::Rejected("invalid: bad approval token hash hex".into()))?;

    // 2. Look up the approval record
    let approval = state
        .db
        .get_approval_by_stored_hash(tenant.community(), &token_hash)
        .await
        .map_err(|_| IngestError::Rejected("invalid: approval not found".into()))?;

    // 3. Validate approval is pending and not expired
    if approval.status != ApprovalStatus::Pending {
        return Err(IngestError::Rejected(format!(
            "invalid: approval already {}",
            approval.status
        )));
    }
    if Utc::now() > approval.expires_at {
        return Err(IngestError::Rejected(
            "invalid: approval token has expired".into(),
        ));
    }

    // 4. Validate caller is authorized approver
    check_approver_spec(&approval.approver_spec, &self_hex)?;

    // Persist the command event — returns open transaction
    let tx = match persist_command_event(state, tenant, event, None).await? {
        PersistResult::Duplicate => {
            return Ok(IngestResult {
                event_id: event.id.to_hex(),
                accepted: true,
                message: "duplicate: already processed".into(),
            });
        }
        PersistResult::Inserted(tx) => tx,
    };

    // 5. Execute: update approval status to granted
    let note = if event.content.is_empty() {
        None
    } else {
        Some(event.content.as_str())
    };

    let updated = state
        .db
        .update_approval_by_stored_hash(
            tenant.community(),
            &token_hash,
            ApprovalStatus::Granted,
            Some(&self_bytes),
            note,
        )
        .await
        .map_err(|e| IngestError::Internal(format!("error: db update_approval: {e}")))?;

    if !updated {
        return Err(IngestError::Rejected(
            "invalid: approval already acted on (race)".into(),
        ));
    }

    // Commit: event + approval update succeeded atomically.
    tx.commit()
        .await
        .map_err(|e| IngestError::Internal(format!("error: commit transaction: {e}")))?;

    // 6. Resume workflow execution (post-commit, async)
    let community_id = tenant.community();
    let run_id = approval.run_id;
    let workflow_id = approval.workflow_id;
    let resume_index = approval.step_index as usize + 1;
    let engine = Arc::clone(&state.workflow_engine);
    let db = state.db.clone();

    tokio::spawn(async move {
        resume_workflow_after_approval(engine, db, community_id, run_id, workflow_id, resume_index)
            .await;
    });

    // 7. Return response
    Ok(IngestResult {
        event_id: event.id.to_hex(),
        accepted: true,
        message: format!(
            "response:{}",
            serde_json::json!({
                "status": "granted",
                "run_id": run_id.to_string(),
            })
        ),
    })
}

async fn handle_approval_deny(
    tenant: &TenantContext,
    state: &Arc<AppState>,
    event: &Event,
    auth: &IngestAuth,
) -> Result<IngestResult, IngestError> {
    let self_bytes = auth.pubkey().to_bytes().to_vec();
    let self_hex = hex::encode(&self_bytes);

    // 1. Extract approval reference
    let token_hash_hex = extract_d_tag(event)
        .or_else(|| extract_e_tag(event))
        .ok_or_else(|| {
            IngestError::Rejected("invalid: missing approval reference (d or e tag)".into())
        })?;

    let token_hash = hex::decode(&token_hash_hex)
        .map_err(|_| IngestError::Rejected("invalid: bad approval token hash hex".into()))?;

    // 2. Look up the approval record
    let approval = state
        .db
        .get_approval_by_stored_hash(tenant.community(), &token_hash)
        .await
        .map_err(|_| IngestError::Rejected("invalid: approval not found".into()))?;

    // 3. Validate approval is pending and not expired
    if approval.status != ApprovalStatus::Pending {
        return Err(IngestError::Rejected(format!(
            "invalid: approval already {}",
            approval.status
        )));
    }
    if Utc::now() > approval.expires_at {
        return Err(IngestError::Rejected(
            "invalid: approval token has expired".into(),
        ));
    }

    // 4. Validate caller is authorized approver
    check_approver_spec(&approval.approver_spec, &self_hex)?;

    // Persist the command event — returns open transaction
    let tx = match persist_command_event(state, tenant, event, None).await? {
        PersistResult::Duplicate => {
            return Ok(IngestResult {
                event_id: event.id.to_hex(),
                accepted: true,
                message: "duplicate: already processed".into(),
            });
        }
        PersistResult::Inserted(tx) => tx,
    };

    // 5. Execute: update approval status to denied
    let note = if event.content.is_empty() {
        None
    } else {
        Some(event.content.as_str())
    };

    let updated = state
        .db
        .update_approval_by_stored_hash(
            tenant.community(),
            &token_hash,
            ApprovalStatus::Denied,
            Some(&self_bytes),
            note,
        )
        .await
        .map_err(|e| IngestError::Internal(format!("error: db update_approval: {e}")))?;

    if !updated {
        return Err(IngestError::Rejected(
            "invalid: approval already acted on (race)".into(),
        ));
    }

    // Commit: event + approval denial succeeded atomically.
    tx.commit()
        .await
        .map_err(|e| IngestError::Internal(format!("error: commit transaction: {e}")))?;

    // 6. Cancel the workflow run (post-commit, async)
    let community_id = tenant.community();
    let run_id = approval.run_id;
    let pubkey_hex = self_hex.clone();
    let db = state.db.clone();

    tokio::spawn(async move {
        let run = match db.get_workflow_run(community_id, run_id).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("approval_deny: failed to fetch run {run_id}: {e}");
                return;
            }
        };

        if run.status != RunStatus::WaitingApproval {
            tracing::warn!(
                "approval_deny: run {run_id} has status '{}', expected 'waiting_approval'",
                run.status
            );
            return;
        }

        let cancel_msg = format!("workflow cancelled: approval denied by {pubkey_hex}");
        if let Err(e) = db
            .update_workflow_run(
                community_id,
                run_id,
                RunStatus::Cancelled,
                run.current_step,
                &run.execution_trace,
                Some(buzz_db::workflow::WorkflowRunFailure {
                    code: "approval_denied",
                    message: &cancel_msg,
                }),
            )
            .await
        {
            tracing::error!("approval_deny: failed to cancel run {run_id}: {e}");
        }
    });

    // 7. Return response
    Ok(IngestResult {
        event_id: event.id.to_hex(),
        accepted: true,
        message: format!(
            "response:{}",
            serde_json::json!({
                "status": "denied",
                "run_id": run_id.to_string(),
            })
        ),
    })
}

/// Resume a suspended workflow run after an approval gate has been granted.
async fn resume_workflow_after_approval(
    engine: Arc<buzz_workflow::WorkflowEngine>,
    db: buzz_db::Db,
    community_id: CommunityId,
    run_id: Uuid,
    workflow_id: Uuid,
    resume_index: usize,
) {
    let run = match db.get_workflow_run(community_id, run_id).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("resume_workflow: failed to fetch run {run_id}: {e}");
            return;
        }
    };

    // Guard: only resume runs that are actually waiting for approval
    if run.status != RunStatus::WaitingApproval {
        tracing::warn!(
            "resume_workflow: run {run_id} has status '{}', expected 'waiting_approval'",
            run.status
        );
        return;
    }

    let workflow = match db.get_workflow(community_id, workflow_id).await {
        Ok(w) => w,
        Err(e) => {
            tracing::error!("resume_workflow: failed to fetch workflow {workflow_id}: {e}");
            return;
        }
    };

    let def: buzz_workflow::WorkflowDef = match serde_json::from_value(workflow.definition.clone())
    {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("resume_workflow: failed to parse workflow definition: {e}");
            if let Err(db_err) = db
                .update_workflow_run(
                    community_id,
                    run_id,
                    RunStatus::Failed,
                    run.current_step,
                    &run.execution_trace,
                    Some(buzz_db::workflow::WorkflowRunFailure {
                        code: "invalid_definition",
                        message: &format!("definition parse error: {e}"),
                    }),
                )
                .await
            {
                tracing::error!("resume_workflow: failed to mark run as failed: {db_err}");
            }
            return;
        }
    };

    // Reconstruct step_outputs from execution trace for template resolution
    let mut initial_outputs: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    if let Some(trace_arr) = run.execution_trace.as_array() {
        for entry in trace_arr {
            if let (Some(step_id), Some(output)) = (
                entry.get("step_id").and_then(|v| v.as_str()),
                entry.get("output"),
            ) {
                initial_outputs.insert(step_id.to_string(), output.clone());
            }
        }
    }

    // Restore trigger context for {{trigger.*}} templates
    let trigger_ctx: TriggerContext = run
        .trigger_context
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    // Execute remaining steps
    let existing_trace = run.execution_trace.as_array().cloned();
    let result = buzz_workflow::executor::execute_from_step(
        &engine,
        community_id,
        run_id,
        &def,
        &trigger_ctx,
        resume_index,
        Some(initial_outputs),
    )
    .await;
    engine
        .finalize_run(community_id, run_id, result, existing_trace)
        .await;
}

#[cfg(test)]
mod tests {
    // B8: a workflow whose definition says `enabled: false` must not run from
    // a manual trigger (kind:46020) or its webhook. Before the fix both gates
    // read only the `workflows.enabled` column, which `upsert_workflow` writes
    // TRUE on create and never updates, so the definition's flag was ignored.
    //
    // These drive the real save path (kind:30620 through `handle_command`), the
    // real manual trigger, and the real webhook handler against Postgres, so
    // the wiring is pinned, not a predicate. Selected explicitly in CI's
    // Backend Integration job; they hard-fail when Postgres is unreachable.
    use super::*;
    use axum::extract::{Path, Query, State};
    use axum::http::{HeaderMap, HeaderValue, StatusCode};
    use buzz_auth::Scope;
    use nostr::{EventBuilder, Keys, Kind, Tag, Timestamp};

    use crate::handlers::ingest::HttpAuthMethod;

    const TEST_DB_URL: &str = "postgres://buzz:buzz_dev@localhost:5432/buzz"; // sadscan:disable np.postgres.1
    const DISABLED: &str = "forbidden: workflow is disabled or inactive";
    const DISABLED_BEFORE_INSERT: &str =
        "forbidden: workflow was disabled or changed before the run was created";

    /// Postgres keeps 63 bytes of an application name.
    fn session_name(host: &str) -> String {
        host.chars().take(63).collect()
    }

    fn test_database_url() -> String {
        std::env::var("BUZZ_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .unwrap_or_else(|_| TEST_DB_URL.to_string())
    }

    /// Real `AppState` + tenant for a fresh community on `host`. Mirrors
    /// `handlers::relay_admin::tests::workspace_profile_test_state`.
    async fn trigger_gate_state(host: &str) -> (Arc<AppState>, TenantContext) {
        let mut config = crate::config::Config::from_env().expect("config from env");
        let database_url = test_database_url();
        config.database_url = database_url.clone();
        config.redis_url = "redis://127.0.0.1:1".to_string();
        config.relay_url = format!("wss://{host}");

        // The pool carries the fixture's host as its application name, so a
        // test can tell its own sessions apart in pg_stat_activity.
        let options: sqlx::postgres::PgConnectOptions = database_url.parse().expect("database url");
        let pool = sqlx::PgPool::connect_with(options.application_name(&session_name(host)))
            .await
            .expect("requires reachable Postgres");
        let db = buzz_db::Db::from_pool(pool.clone());
        let record = db
            .ensure_configured_community(host)
            .await
            .expect("ensure community");
        let tenant = TenantContext::resolved(record.id, host);

        let redis_pool = deadpool_redis::Config::from_url(&config.redis_url)
            .create_pool(Some(deadpool_redis::Runtime::Tokio1))
            .expect("redis pool config");
        let pubsub = Arc::new(
            buzz_pubsub::PubSubManager::new(&config.redis_url, redis_pool.clone())
                .await
                .expect("pubsub manager"),
        );
        let audit = buzz_audit::AuditService::new(pool.clone());
        let auth = buzz_auth::AuthService::new(config.auth.clone());
        let search = buzz_search::SearchService::new(pool.clone());
        let workflow_engine = Arc::new(buzz_workflow::WorkflowEngine::new(
            db.clone(),
            buzz_workflow::WorkflowConfig::default(),
        ));
        let media_storage = buzz_media::MediaStorage::new(&config.media).expect("media storage");
        let (state, _audit_shutdown) = AppState::new(
            config,
            db,
            redis_pool,
            audit,
            pubsub,
            auth,
            search,
            workflow_engine,
            Keys::generate(),
            media_storage,
        );
        (Arc::new(state), tenant)
    }

    /// One owner, one channel they created (so they hold the `owner` role),
    /// and one workflow id that every save in a test re-uses.
    struct Fixture {
        state: Arc<AppState>,
        tenant: TenantContext,
        host: String,
        owner: Keys,
        channel_id: Uuid,
        workflow_id: Uuid,
        clock: u64,
    }

    impl Fixture {
        async fn new(label: &str) -> Self {
            let host = format!("b8-{label}-{}.example", Uuid::new_v4().simple());
            let (state, tenant) = trigger_gate_state(&host).await;
            let owner = Keys::generate();
            let owner_pk = owner.public_key().to_bytes().to_vec();
            state
                .db
                .ensure_user(tenant.community(), &owner_pk)
                .await
                .expect("owner user");
            let channel_id = Uuid::new_v4();
            state
                .db
                .create_channel_with_id(
                    tenant.community(),
                    channel_id,
                    &format!("b8-{}", channel_id.simple()),
                    buzz_db::channel::ChannelType::Stream,
                    buzz_db::channel::ChannelVisibility::Open,
                    None,
                    &owner_pk,
                    None,
                )
                .await
                .expect("channel");
            Self {
                state,
                tenant,
                host,
                owner,
                channel_id,
                workflow_id: Uuid::new_v4(),
                clock: Timestamp::now().as_secs(),
            }
        }

        /// Strictly increasing timestamps, so a re-save is never dominated
        /// by the previous save of the same NIP-33 coordinate.
        fn tick(&mut self) -> Timestamp {
            self.clock += 1;
            Timestamp::from_secs(self.clock)
        }

        fn auth(&self) -> IngestAuth {
            IngestAuth::Http {
                pubkey: self.owner.public_key(),
                scopes: Scope::all_known(),
                auth_method: HttpAuthMethod::Nip98,
            }
        }

        /// Save the workflow through the real kind:30620 path. The definition
        /// has a webhook trigger, so one workflow exercises both gates.
        async fn save(&mut self, enabled: bool) {
            let yaml = format!(
                "name: b8 gate\nenabled: {enabled}\ntrigger:\n  on: webhook\nsteps:\n  - id: say\n    action: send_message\n    text: b8\n"
            );
            let event = EventBuilder::new(Kind::Custom(KIND_WORKFLOW_DEF as u16), yaml)
                .tags(vec![
                    Tag::parse(["h", &self.channel_id.to_string()]).expect("h tag"),
                    Tag::parse(["d", &self.workflow_id.to_string()]).expect("d tag"),
                ])
                .custom_created_at(self.tick())
                .sign_with_keys(&self.owner)
                .expect("sign 30620");
            let saved = handle_command(&self.tenant, &self.state, event, self.auth())
                .await
                .expect("the owner's save must be accepted");
            assert!(
                saved.message.starts_with("response:"),
                "the save must execute, not dedupe: {}",
                saved.message
            );
            let stored = self.workflow().await;
            let stored_def: buzz_workflow::WorkflowDef =
                serde_json::from_value(stored.definition).expect("stored definition");
            assert_eq!(
                stored_def.enabled, enabled,
                "the stored definition must carry the saved flag"
            );
        }

        async fn workflow(&self) -> buzz_db::workflow::WorkflowRecord {
            self.state
                .db
                .get_workflow(self.tenant.community(), self.workflow_id)
                .await
                .expect("workflow row")
        }

        /// Fire the real manual trigger (kind:46020) as the owner.
        async fn trigger(&mut self) -> Result<IngestResult, IngestError> {
            let event = EventBuilder::new(Kind::Custom(KIND_WORKFLOW_TRIGGER as u16), "")
                .tags(vec![
                    Tag::parse(["d", &self.workflow_id.to_string()]).expect("d tag")
                ])
                .custom_created_at(self.tick())
                .sign_with_keys(&self.owner)
                .expect("sign 46020");
            handle_command(&self.tenant, &self.state, event, self.auth()).await
        }

        /// POST the workflow's webhook with its real secret and Host binding.
        async fn webhook(&self) -> StatusCode {
            let secret = webhook_secret::extract_secret(&self.workflow().await.definition)
                .expect("a webhook workflow carries a secret");
            let mut headers = HeaderMap::new();
            headers.insert(
                axum::http::header::HOST,
                HeaderValue::from_str(&self.host).expect("host header"),
            );
            headers.insert(
                "x-webhook-secret",
                HeaderValue::from_str(&secret).expect("secret header"),
            );
            match crate::api::bridge::workflow_webhook(
                State(Arc::clone(&self.state)),
                Path(self.workflow_id.to_string()),
                Query(crate::api::bridge::WebhookQuery { secret: None }),
                headers,
                axum::body::Bytes::new(),
            )
            .await
            {
                Ok((status, _)) | Err((status, _)) => status,
            }
        }

        /// A second handle on the same workflow, so one task can trigger
        /// while the test keeps saving.
        fn twin(&self) -> Self {
            Self {
                state: Arc::clone(&self.state),
                tenant: self.tenant.clone(),
                host: self.host.clone(),
                owner: self.owner.clone(),
                channel_id: self.channel_id,
                workflow_id: self.workflow_id,
                clock: self.clock,
            }
        }

        async fn runs(&self) -> usize {
            self.state
                .db
                .list_workflow_runs(self.tenant.community(), self.workflow_id, 100)
                .await
                .expect("list runs")
                .len()
        }
    }

    fn refusal(result: Result<IngestResult, IngestError>) -> String {
        match result {
            Err(IngestError::Rejected(message)) => message,
            Err(other) => panic!("expected a rejection, got {other:?}"),
            Ok(accepted) => panic!("expected a rejection, got accepted: {}", accepted.message),
        }
    }

    /// Created with `enabled: false`: the manual trigger is refused and no run
    /// is created, while the column still reads TRUE, so the refusal can only
    /// come from the definition.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn manual_trigger_refuses_a_workflow_created_disabled() {
        let mut f = Fixture::new("created-disabled").await;
        f.save(false).await;
        assert!(
            f.workflow().await.enabled,
            "create writes the column TRUE whatever the YAML says"
        );

        assert_eq!(refusal(f.trigger().await), DISABLED);
        assert_eq!(f.runs().await, 0, "a refused trigger must not create a run");
    }

    /// Control plus the update path: enabled runs; re-saved disabled is
    /// refused; re-saved enabled runs again. The column never changes, since
    /// `upsert_workflow` does not touch it on update.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn manual_trigger_follows_the_definition_across_updates() {
        let mut f = Fixture::new("updates").await;
        f.save(true).await;
        let accepted = f.trigger().await.expect("an enabled workflow must run");
        assert!(accepted.accepted);
        assert_eq!(f.runs().await, 1);

        f.save(false).await;
        assert!(f.workflow().await.enabled, "update leaves the column TRUE");
        assert_eq!(refusal(f.trigger().await), DISABLED);
        assert_eq!(f.runs().await, 1, "a refused trigger must not create a run");

        f.save(true).await;
        f.trigger()
            .await
            .expect("re-enabled workflow must run again");
        assert_eq!(f.runs().await, 2);
    }

    /// The webhook gate (`api::bridge::workflow_webhook`) honors the same flag
    /// and keeps its generic 404 for a disabled workflow.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn webhook_follows_the_definition_enabled_flag() {
        let mut f = Fixture::new("webhook").await;
        f.save(false).await;
        assert_eq!(f.webhook().await, StatusCode::NOT_FOUND);
        assert_eq!(f.runs().await, 0, "a refused webhook must not create a run");

        f.save(true).await;
        assert_eq!(f.webhook().await, StatusCode::ACCEPTED);
        assert_eq!(f.runs().await, 1);

        f.save(false).await;
        assert_eq!(f.webhook().await, StatusCode::NOT_FOUND);
        assert_eq!(f.runs().await, 1, "a refused webhook must not create a run");
    }

    /// Owner-removal arm: the column write the removal path makes
    /// (`disable_workflows_for_owner_in_channel`, called from
    /// `side_effects::disable_departed_member_workflows`) keeps both gates
    /// shut even after the definition is re-saved with `enabled: true`.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn owner_removal_revocation_survives_an_enabled_resave() {
        let mut f = Fixture::new("revoked").await;
        f.save(true).await;
        f.trigger().await.expect("an enabled workflow must run");
        assert_eq!(f.runs().await, 1);

        let owner_pk = f.owner.public_key().to_bytes().to_vec();
        let disabled = f
            .state
            .db
            .disable_workflows_for_owner_in_channel(f.tenant.community(), f.channel_id, &owner_pk)
            .await
            .expect("owner-removal disable");
        assert_eq!(disabled, 1, "the removal write must hit this workflow");

        f.save(true).await;
        assert!(
            !f.workflow().await.enabled,
            "a re-save must not switch a revoked workflow back on"
        );
        assert_eq!(refusal(f.trigger().await), DISABLED);
        assert_eq!(f.webhook().await, StatusCode::NOT_FOUND);
        assert_eq!(f.runs().await, 1, "a refused trigger must not create a run");
    }

    /// Which switch is thrown while the run insert is held.
    #[derive(Clone, Copy)]
    enum SwitchOff {
        /// The author re-saves the definition with `enabled: false`.
        Definition,
        /// Owner removal clears the `workflows.enabled` column.
        Column,
    }

    /// What the held trigger answered once the insert was let go.
    enum Held {
        Manual(Result<IngestResult, IngestError>),
        Webhook(StatusCode),
    }

    /// A disable committed after the gate's snapshot check but before the run
    /// insert must leave no run. The ordering is forced with a real lock, not
    /// a sleep: `workflow_runs` is held ACCESS EXCLUSIVE, the real trigger is
    /// started and observed waiting on that lock at its run insert, the
    /// workflow is switched off and that write is confirmed stored, and only
    /// then is the lock released. The wait is read for this fixture's own
    /// sessions only, since other tests insert runs into the same table.
    /// (Arm from buzz-build's B8 review.)
    async fn disable_while_the_run_insert_is_held(manual: bool, switch: SwitchOff, label: &str) {
        let mut f = Fixture::new(label).await;
        f.save(true).await;

        let pool = sqlx::PgPool::connect(&test_database_url())
            .await
            .expect("requires reachable Postgres");
        let mut lock = pool.begin().await.expect("lock transaction");
        sqlx::query("LOCK TABLE workflow_runs IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *lock)
            .await
            .expect("hold run inserts");

        let mut runner = f.twin();
        let pending = tokio::spawn(async move {
            if manual {
                Held::Manual(runner.trigger().await)
            } else {
                Held::Webhook(runner.webhook().await)
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                let waiting: i64 = sqlx::query_scalar(
                    r#"
                    SELECT count(*) FROM pg_stat_activity
                    WHERE datname = current_database()
                      AND application_name = $1
                      AND wait_event_type = 'Lock'
                      AND query LIKE '%INSERT INTO workflow_runs%'
                    "#,
                )
                .bind(session_name(&f.host))
                .fetch_one(&pool)
                .await
                .expect("read pg_stat_activity");
                if waiting > 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the trigger must pass its gates and reach the run insert");

        match switch {
            SwitchOff::Definition => {
                // `save` asserts the stored definition now carries `false`.
                tokio::time::timeout(std::time::Duration::from_secs(20), f.save(false))
                    .await
                    .expect("the disabling save must finish while the insert is held");
            }
            SwitchOff::Column => {
                let owner_pk = f.owner.public_key().to_bytes().to_vec();
                let disabled = f
                    .state
                    .db
                    .disable_workflows_for_owner_in_channel(
                        f.tenant.community(),
                        f.channel_id,
                        &owner_pk,
                    )
                    .await
                    .expect("owner-removal disable");
                assert_eq!(disabled, 1, "the removal write must hit this workflow");
                assert!(!f.workflow().await.enabled, "the column must read FALSE");
            }
        }

        lock.commit().await.expect("release run inserts");
        let held = tokio::time::timeout(std::time::Duration::from_secs(20), pending)
            .await
            .expect("the held trigger must finish")
            .expect("trigger task");

        assert_eq!(
            f.runs().await,
            0,
            "a disable committed before the run insert must leave no run"
        );
        match held {
            Held::Manual(result) => assert_eq!(refusal(result), DISABLED_BEFORE_INSERT),
            Held::Webhook(status) => assert_eq!(status, StatusCode::NOT_FOUND),
        }
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn manual_trigger_makes_no_run_when_the_definition_is_disabled_before_the_insert() {
        disable_while_the_run_insert_is_held(true, SwitchOff::Definition, "race-manual-def").await;
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn webhook_makes_no_run_when_the_definition_is_disabled_before_the_insert() {
        disable_while_the_run_insert_is_held(false, SwitchOff::Definition, "race-webhook-def")
            .await;
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn manual_trigger_makes_no_run_when_the_owner_is_revoked_before_the_insert() {
        disable_while_the_run_insert_is_held(true, SwitchOff::Column, "race-manual-col").await;
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn webhook_makes_no_run_when_the_owner_is_revoked_before_the_insert() {
        disable_while_the_run_insert_is_held(false, SwitchOff::Column, "race-webhook-col").await;
    }
}
