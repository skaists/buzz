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
    let run_id = state
        .db
        .create_workflow_run(
            community_id,
            workflow_id,
            Some(&event_id_bytes),
            trigger_ctx_json.as_ref(),
        )
        .await
        .map_err(|e| IngestError::Internal(format!("error: db create_workflow_run: {e}")))?;

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

/// The binding a grant/deny event claims (WF-08). Every field is optional on
/// the wire; `verify_grant_binding` decides which ones the record demands.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct GrantBinding {
    /// `run` tag — run id the signer believes they are deciding.
    run: Option<String>,
    /// `step` tag — step id the signer believes they are deciding.
    step: Option<String>,
    /// `candidate` tag — exact candidate the signer reviewed.
    candidate: Option<String>,
}

impl GrantBinding {
    fn from_event(event: &Event) -> Self {
        let get = |name: &str| {
            event.tags.iter().find_map(|t| {
                let s = t.as_slice();
                (s.first().map(|k| k.as_str()) == Some(name))
                    .then(|| s.get(1).map(|v| v.trim().to_owned()))
                    .flatten()
                    .filter(|v| !v.is_empty())
            })
        };
        Self {
            run: get("run"),
            step: get("step"),
            candidate: get("candidate"),
        }
    }
}

/// Fail-closed binding check between a decision event and the pending record.
///
/// - `run` / `step`, when present, must match the record exactly.
/// - A record minted **with** a `candidate_ref` requires an identical
///   `candidate` tag: a stale approval for candidate A can never approve
///   corrected candidate B, and a grant that names nothing cannot approve
///   anything that was bound.
/// - A record minted **without** a candidate rejects a grant that names one:
///   the signer believes they are approving something the gate never bound.
fn verify_grant_binding(
    binding: &GrantBinding,
    approval: &buzz_db::workflow::ApprovalRecord,
) -> Result<(), String> {
    if let Some(run) = &binding.run {
        if run.to_lowercase() != approval.run_id.to_string() {
            return Err("forbidden: decision names a different run than the approval".into());
        }
    }
    if let Some(step) = &binding.step {
        if step != &approval.step_id {
            return Err("forbidden: decision names a different step than the approval".into());
        }
    }
    match (
        approval.candidate_ref.as_deref(),
        binding.candidate.as_deref(),
    ) {
        (None, None) => Ok(()),
        (Some(bound), Some(claimed)) if bound == claimed => Ok(()),
        (Some(_), Some(_)) => Err(
            "forbidden: candidate mismatch — this approval is bound to a different candidate"
                .into(),
        ),
        (Some(_), None) => Err(
            "forbidden: this approval is bound to a candidate; the decision must name it \
             (candidate tag)"
                .into(),
        ),
        (None, Some(_)) => Err(
            "forbidden: decision names a candidate but the approval was not bound to one".into(),
        ),
    }
}

/// Idempotent replay: the same signer repeating the decision that was already
/// applied to this record is accepted as a no-op. Nothing advances, nothing
/// duplicates, no error — restarts and retried CLI calls converge.
fn duplicate_decision(
    approval: &buzz_db::workflow::ApprovalRecord,
    decision: ApprovalStatus,
    signer: &[u8],
    event: &Event,
) -> Option<IngestResult> {
    if approval.status == decision && approval.approver_pubkey.as_deref() == Some(signer) {
        return Some(IngestResult {
            event_id: event.id.to_hex(),
            accepted: true,
            message: format!(
                "duplicate: approval already {} by this signer",
                approval.status
            ),
        });
    }
    None
}

/// The record must still be pending and unexpired for a *new* decision.
///
/// T4c: the first terminal decision is immutable. A later contradictory
/// decision (grant after deny, deny after grant, or a second decision by a
/// different signer) is rejected and recorded in the relay log with the
/// full binding; history is never rewritten.
fn ensure_pending_and_live(
    approval: &buzz_db::workflow::ApprovalRecord,
    now: chrono::DateTime<Utc>,
) -> Result<(), IngestError> {
    if approval.status != ApprovalStatus::Pending {
        warn!(
            run_id = %approval.run_id,
            step_id = %approval.step_id,
            approval_ref = %hex::encode(&approval.token),
            candidate_ref = ?approval.candidate_ref,
            existing = %approval.status,
            existing_signer = ?approval.approver_pubkey.as_ref().map(hex::encode),
            "WF-08 T4c: contradictory decision rejected — terminal decision is immutable"
        );
        return Err(IngestError::Rejected(format!(
            "invalid: approval already {}",
            approval.status
        )));
    }
    if now > approval.expires_at {
        return Err(IngestError::Rejected(
            "invalid: approval token has expired".into(),
        ));
    }
    Ok(())
}

/// The reviewer's decision as durable evidence: this exact JSON becomes the
/// gate step's `output` in the run trace and the content of the
/// kind:46011/46012 event. `approval_ref` is the token hash — the raw token
/// never appears here.
fn approval_decision_evidence(
    approval: &buzz_db::workflow::ApprovalRecord,
    decision: ApprovalStatus,
    approver_hex: &str,
    note: &str,
    decision_event_id: &str,
    decided_at: chrono::DateTime<Utc>,
) -> serde_json::Value {
    let decision_str = match decision {
        ApprovalStatus::Granted => "granted",
        ApprovalStatus::Denied => "denied",
        ApprovalStatus::Pending => "pending",
        ApprovalStatus::Expired => "expired",
    };
    serde_json::json!({
        "type": format!("workflow_approval_{decision_str}"),
        "decision": decision_str,
        "workflow_id": approval.workflow_id,
        "run_id": approval.run_id,
        "step_id": approval.step_id,
        "step_index": approval.step_index,
        "approval_ref": hex::encode(&approval.token),
        "approver_spec": approval.approver_spec,
        "approver": approver_hex,
        "candidate_ref": approval.candidate_ref,
        "note": if note.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(note.to_owned()) },
        "decision_event_id": decision_event_id,
        "decided_at": decided_at.to_rfc3339(),
    })
}

/// Write the decision into the trace entry of the gate step. The last entry
/// for `step_id` (the `waiting_approval` one minted at suspension) is
/// completed in place, keeping its `requested_at`; if no such entry exists
/// (legacy run), a fresh completed entry is appended so the evidence is
/// never dropped.
fn record_approval_evidence(
    trace: &mut Vec<serde_json::Value>,
    step_id: &str,
    evidence: &serde_json::Value,
) -> bool {
    let status = match evidence.get("decision").and_then(|d| d.as_str()) {
        Some("granted") => "completed",
        Some("denied") => "denied",
        _ => "completed",
    };
    let completed_at = evidence
        .get("decided_at")
        .and_then(|d| d.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp());
    if let Some(entry) = trace
        .iter_mut()
        .rev()
        .find(|e| e.get("step_id").and_then(|s| s.as_str()) == Some(step_id))
    {
        let mut output = entry
            .get("output")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        if let (Some(dst), Some(src)) = (output.as_object_mut(), evidence.as_object()) {
            for (k, v) in src {
                dst.insert(k.clone(), v.clone());
            }
        }
        entry["output"] = output;
        entry["status"] = serde_json::Value::String(status.to_owned());
        if let Some(ts) = completed_at {
            entry["completed_at"] = serde_json::json!(ts);
        }
        return true;
    }
    trace.push(serde_json::json!({
        "step_id": step_id,
        "status": status,
        "output": evidence,
        "completed_at": completed_at,
    }));
    false
}

/// Publish the kind:46011/46012 decision event into the workflow's channel.
/// Failures only log: the decision is already durable in
/// `workflow_approvals` and the run trace; the event is the broadcast, not
/// the record.
async fn publish_approval_decision(
    state: &Arc<AppState>,
    community_id: CommunityId,
    workflow_id: Uuid,
    kind: u32,
    evidence: &serde_json::Value,
    approver_spec: &str,
) {
    let workflow = match state.db.get_workflow(community_id, workflow_id).await {
        Ok(w) => w,
        Err(e) => {
            tracing::error!("approval_decision: workflow {workflow_id} unreadable: {e}");
            return;
        }
    };
    let Some(channel_id) = workflow.channel_id else {
        tracing::error!("approval_decision: workflow {workflow_id} has no channel scope");
        return;
    };
    let mut notify = vec![hex::encode(&workflow.owner_pubkey)];
    if approver_spec != "any" && !notify.iter().any(|n| n == approver_spec) {
        notify.push(approver_spec.to_owned());
    }
    let sink = crate::workflow_sink::RelayActionSink::new(state);
    match buzz_workflow::ActionSink::emit_workflow_event(
        &sink,
        community_id,
        &channel_id.to_string(),
        kind,
        &evidence.to_string(),
        &notify,
    )
    .await
    {
        Ok(id) => tracing::info!(kind, event_id = %id, "approval decision published"),
        Err(e) => tracing::error!(kind, "approval decision event not published: {e}"),
    }
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

    // 3. WF-08 gate checks, in fail-closed order:
    //    signer must be the designated approver → the grant must name the
    //    same run/step/candidate the record was minted for → a repeat of an
    //    identical, already-applied decision by the same signer is an
    //    idempotent no-op → otherwise the record must still be pending and
    //    unexpired.
    check_approver_spec(&approval.approver_spec, &self_hex)?;
    let binding = GrantBinding::from_event(event);
    verify_grant_binding(&binding, &approval).map_err(IngestError::Rejected)?;
    if let Some(dup) = duplicate_decision(&approval, ApprovalStatus::Granted, &self_bytes, event) {
        return Ok(dup);
    }
    ensure_pending_and_live(&approval, Utc::now())?;

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

    // 6. Resume workflow execution (post-commit, async). The reviewer's
    //    evidence travels with the resume so it lands in the run trace and
    //    the kind:46011 event before any later step runs.
    let community_id = tenant.community();
    let run_id = approval.run_id;
    let evidence = approval_decision_evidence(
        &approval,
        ApprovalStatus::Granted,
        &self_hex,
        event.content.as_str(),
        &event.id.to_hex(),
        Utc::now(),
    );
    let engine = Arc::clone(&state.workflow_engine);
    let db = state.db.clone();
    let state_for_sink = Arc::clone(state);

    tokio::spawn(async move {
        publish_approval_decision(
            &state_for_sink,
            community_id,
            approval.workflow_id,
            KIND_WORKFLOW_APPROVAL_GRANTED,
            &evidence,
            &approval.approver_spec,
        )
        .await;
        resume_workflow_after_approval(engine, db, community_id, approval, evidence).await;
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

    // 3. WF-08 gate checks — same fail-closed order as the grant path.
    check_approver_spec(&approval.approver_spec, &self_hex)?;
    let binding = GrantBinding::from_event(event);
    verify_grant_binding(&binding, &approval).map_err(IngestError::Rejected)?;
    if let Some(dup) = duplicate_decision(&approval, ApprovalStatus::Denied, &self_bytes, event) {
        return Ok(dup);
    }
    ensure_pending_and_live(&approval, Utc::now())?;

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
    let evidence = approval_decision_evidence(
        &approval,
        ApprovalStatus::Denied,
        &self_hex,
        event.content.as_str(),
        &event.id.to_hex(),
        Utc::now(),
    );
    let db = state.db.clone();
    let state_for_sink = Arc::clone(state);

    tokio::spawn(async move {
        publish_approval_decision(
            &state_for_sink,
            community_id,
            approval.workflow_id,
            KIND_WORKFLOW_APPROVAL_DENIED,
            &evidence,
            &approval.approver_spec,
        )
        .await;

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

        // WF-08: the denial is evidence too — it completes the gate entry in
        // the trace (status `denied`) so a restarted or re-read run shows who
        // rejected which candidate and why. The run then closes as cancelled
        // with the stable `approval_denied` code; a corrected candidate is a
        // new run, never a mutation of this one.
        let mut trace_vec = run.execution_trace.as_array().cloned().unwrap_or_default();
        record_approval_evidence(&mut trace_vec, &approval.step_id, &evidence);
        let trace_json = serde_json::Value::Array(trace_vec);

        let cancel_msg = format!("workflow cancelled: approval denied by {pubkey_hex}");
        if let Err(e) = db
            .update_workflow_run(
                community_id,
                run_id,
                RunStatus::Cancelled,
                run.current_step,
                &trace_json,
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
    approval: buzz_db::workflow::ApprovalRecord,
    evidence: serde_json::Value,
) {
    let run_id = approval.run_id;
    let workflow_id = approval.workflow_id;
    let resume_index = approval.step_index as usize + 1;

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

    // WF-08: the reviewer's decision becomes the gate step's durable output
    // *before* anything else runs — restart after this point replays the
    // evidence from the trace, and later steps can cite
    // `{{steps.<gate>.output.note}}` / `.approver` / `.candidate_ref`.
    let mut trace_vec = run.execution_trace.as_array().cloned().unwrap_or_default();
    record_approval_evidence(&mut trace_vec, &approval.step_id, &evidence);
    let trace_json = serde_json::Value::Array(trace_vec);
    if let Err(e) = db
        .update_workflow_run(
            community_id,
            run_id,
            RunStatus::WaitingApproval,
            run.current_step,
            &trace_json,
            None,
        )
        .await
    {
        tracing::error!("resume_workflow: failed to persist approval evidence for {run_id}: {e}");
        return;
    }
    let run = buzz_db::workflow::WorkflowRunRecord {
        execution_trace: trace_json,
        ..run
    };

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
mod wf08_tests {
    //! WF-08 designated-review binding. These are the unit-level RED→GREEN
    //! proofs for the acceptance matrix rows that do not need a relay:
    //! T3 (stale candidate), T4 (duplicate grant), wrong-signer spec, and
    //! evidence persistence in the trace. On base `191a577` none of these
    //! helpers exist — the file does not compile with these tests present.
    use super::*;
    use buzz_db::workflow::{ApprovalRecord, ApprovalStatus};
    use nostr::{EventBuilder, Keys, Kind, Tag};

    const REVIEWER: &str = "755029663fee78e734a217867246e22933a4683dda19dd8435488688a29f9541";

    fn record(
        candidate: Option<&str>,
        status: ApprovalStatus,
        approver: Option<&[u8]>,
    ) -> ApprovalRecord {
        ApprovalRecord {
            token: vec![0xab; 32],
            workflow_id: Uuid::new_v4(),
            run_id: Uuid::new_v4(),
            step_id: "review".to_owned(),
            step_index: 1,
            approver_spec: REVIEWER.to_owned(),
            status,
            approver_pubkey: approver.map(|a| a.to_vec()),
            note: None,
            candidate_ref: candidate.map(str::to_owned),
            expires_at: Utc::now() + chrono::Duration::hours(1),
            created_at: Utc::now(),
        }
    }

    fn signed_event(tags: Vec<Tag>, content: &str) -> Event {
        let keys = Keys::generate();
        EventBuilder::new(Kind::Custom(KIND_APPROVAL_GRANT as u16), content)
            .tags(tags)
            .sign_with_keys(&keys)
            .expect("sign")
    }

    // -- T3: stale candidate ------------------------------------------------

    #[test]
    fn t4b_bound_gate_rejects_grant_for_a_different_candidate() {
        let rec = record(Some("commitA"), ApprovalStatus::Pending, None);
        let b = GrantBinding {
            candidate: Some("commitB".into()),
            ..Default::default()
        };
        let err = verify_grant_binding(&b, &rec).unwrap_err();
        assert!(err.contains("candidate mismatch"), "{err}");
    }

    #[test]
    fn bound_gate_rejects_grant_that_names_no_candidate() {
        let rec = record(Some("commitA"), ApprovalStatus::Pending, None);
        let err = verify_grant_binding(&GrantBinding::default(), &rec).unwrap_err();
        assert!(err.contains("must name it"), "{err}");
    }

    #[test]
    fn bound_gate_accepts_exact_candidate() {
        let rec = record(Some("commitA"), ApprovalStatus::Pending, None);
        let b = GrantBinding {
            candidate: Some("commitA".into()),
            ..Default::default()
        };
        assert!(verify_grant_binding(&b, &rec).is_ok());
    }

    #[test]
    fn unbound_gate_rejects_grant_that_invents_a_candidate() {
        let rec = record(None, ApprovalStatus::Pending, None);
        let b = GrantBinding {
            candidate: Some("commitA".into()),
            ..Default::default()
        };
        assert!(verify_grant_binding(&b, &rec).is_err());
        assert!(verify_grant_binding(&GrantBinding::default(), &rec).is_ok());
    }

    #[test]
    fn run_and_step_tags_must_match_when_present() {
        let rec = record(None, ApprovalStatus::Pending, None);
        let wrong_run = GrantBinding {
            run: Some(Uuid::new_v4().to_string()),
            ..Default::default()
        };
        assert!(verify_grant_binding(&wrong_run, &rec).is_err());
        let wrong_step = GrantBinding {
            step: Some("deploy".into()),
            ..Default::default()
        };
        assert!(verify_grant_binding(&wrong_step, &rec).is_err());
        let right = GrantBinding {
            run: Some(rec.run_id.to_string().to_uppercase()),
            step: Some("review".into()),
            candidate: None,
        };
        assert!(
            verify_grant_binding(&right, &rec).is_ok(),
            "run id compares case-insensitively"
        );
    }

    #[test]
    fn grant_binding_is_read_from_event_tags() {
        let rec = record(Some("abc"), ApprovalStatus::Pending, None);
        let ev = signed_event(
            vec![
                Tag::parse(["d", &hex::encode([0xab; 32])]).unwrap(),
                Tag::parse(["candidate", " abc "]).unwrap(),
                Tag::parse(["run", &rec.run_id.to_string()]).unwrap(),
                Tag::parse(["step", "review"]).unwrap(),
            ],
            "reviewed",
        );
        let b = GrantBinding::from_event(&ev);
        assert_eq!(
            b.candidate.as_deref(),
            Some("abc"),
            "tag values are trimmed"
        );
        assert_eq!(b.step.as_deref(), Some("review"));
        assert!(verify_grant_binding(&b, &rec).is_ok());
    }

    // -- T4: duplicate grant is idempotent ----------------------------------

    #[test]
    fn t4a_same_signer_repeating_identical_decision_is_a_noop() {
        let signer = [0x11u8; 33];
        let rec = record(None, ApprovalStatus::Granted, Some(&signer));
        let ev = signed_event(vec![], "");
        let dup = duplicate_decision(&rec, ApprovalStatus::Granted, &signer, &ev)
            .expect("duplicate must be accepted as a no-op");
        assert!(dup.accepted);
        assert!(dup.message.starts_with("duplicate:"), "{}", dup.message);
        // A *new* decision on the same record would have been refused.
        assert!(ensure_pending_and_live(&rec, Utc::now()).is_err());
    }

    #[test]
    fn t4c_different_signer_or_contradictory_decision_is_not_a_duplicate() {
        let signer = [0x11u8; 33];
        let other = [0x22u8; 33];
        let ev = signed_event(vec![], "");
        let granted = record(None, ApprovalStatus::Granted, Some(&signer));
        assert!(duplicate_decision(&granted, ApprovalStatus::Granted, &other, &ev).is_none());
        assert!(duplicate_decision(&granted, ApprovalStatus::Denied, &signer, &ev).is_none());
        let pending = record(None, ApprovalStatus::Pending, None);
        assert!(duplicate_decision(&pending, ApprovalStatus::Granted, &signer, &ev).is_none());
    }

    #[test]
    fn t4c_expired_or_decided_records_reject_new_decisions() {
        let mut rec = record(None, ApprovalStatus::Pending, None);
        assert!(ensure_pending_and_live(&rec, Utc::now()).is_ok());
        assert!(
            ensure_pending_and_live(&rec, rec.expires_at + chrono::Duration::seconds(1)).is_err()
        );
        rec.status = ApprovalStatus::Denied;
        assert!(ensure_pending_and_live(&rec, Utc::now()).is_err());
    }

    // -- T2 (unit half): designated signer -----------------------------------

    #[test]
    fn approver_spec_pubkey_fails_closed_for_other_signers() {
        assert!(check_approver_spec(REVIEWER, REVIEWER).is_ok());
        assert!(check_approver_spec(REVIEWER, &REVIEWER.to_uppercase()).is_ok());
        assert!(check_approver_spec(REVIEWER, &"0".repeat(64)).is_err());
        assert!(check_approver_spec("@release-manager", REVIEWER).is_err());
    }

    // -- evidence ----------------------------------------------------------

    #[test]
    fn evidence_completes_the_waiting_entry_in_place_and_keeps_request_fields() {
        let rec = record(Some("abc"), ApprovalStatus::Pending, None);
        let mut trace = vec![
            serde_json::json!({"step_id": "notify", "status": "completed", "output": {}}),
            serde_json::json!({
                "step_id": "review",
                "status": "waiting_approval",
                "output": {"approval_ref": "ab", "requested_at": "2026-09-18T00:00:00+00:00"}
            }),
        ];
        let ev = approval_decision_evidence(
            &rec,
            ApprovalStatus::Granted,
            REVIEWER,
            "tests green at 0123abcd",
            "eventid",
            Utc::now(),
        );
        assert!(record_approval_evidence(&mut trace, "review", &ev));
        assert_eq!(trace.len(), 2, "no new entry when the waiting entry exists");
        let gate = &trace[1];
        assert_eq!(gate["status"], "completed");
        assert_eq!(gate["output"]["decision"], "granted");
        assert_eq!(gate["output"]["approver"], REVIEWER);
        assert_eq!(gate["output"]["note"], "tests green at 0123abcd");
        assert_eq!(gate["output"]["candidate_ref"], "abc");
        assert_eq!(gate["output"]["decision_event_id"], "eventid");
        assert_eq!(gate["output"]["requested_at"], "2026-09-18T00:00:00+00:00");
        assert!(gate["completed_at"].is_number());
        assert!(
            !gate.to_string().contains("\"token\""),
            "raw token never enters the trace"
        );
    }

    #[test]
    fn denial_evidence_marks_the_gate_denied() {
        let rec = record(None, ApprovalStatus::Pending, None);
        let mut trace = vec![];
        let ev =
            approval_decision_evidence(&rec, ApprovalStatus::Denied, REVIEWER, "", "e", Utc::now());
        assert!(
            !record_approval_evidence(&mut trace, "review", &ev),
            "legacy run: entry appended"
        );
        assert_eq!(trace[0]["status"], "denied");
        assert!(trace[0]["output"]["note"].is_null());
    }
}
