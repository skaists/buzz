use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use nostr::{EventBuilder, JsonUtil, Keys, Kind, Tag};
use reqwest::Method;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use sha2::{Digest, Sha256};

// nostr 0.36 alias — required for cross-version bridging with buzz-sdk.

use crate::app_state::AppState;

const DEFAULT_RELAY_WS_URL: &str = "ws://localhost:3000";

// A reached-but-malformed 2xx body is NOT a connectivity failure, so this
// message must never carry the "relay unreachable:" prefix the frontend
// classifier keys on. Extracted to a const so a test can pin that contract.
const MALFORMED_RESPONSE_MESSAGE: &str = "relay returned malformed response: not valid JSON";

fn configured_env_var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn relay_ws_url() -> String {
    configured_env_var("BUZZ_RELAY_URL")
        .or_else(|| option_env!("BUZZ_DESKTOP_BUILD_RELAY_URL").map(str::to_string))
        .unwrap_or_else(|| DEFAULT_RELAY_WS_URL.to_string())
}

/// Read the workspace relay URL override, if set. Returns `None` when no
/// override is active or when the mutex is poisoned (best-effort).
fn workspace_relay_override(state: &AppState) -> Option<String> {
    state
        .relay_url_override
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
}

/// Returns the relay WebSocket URL, checking the workspace override first.
/// Precedence: workspace override > env vars > build-time vars > default.
pub fn relay_ws_url_with_override(state: &AppState) -> String {
    workspace_relay_override(state).unwrap_or_else(relay_ws_url)
}

/// Returns the relay HTTP API base URL, checking the workspace override first.
/// Precedence: workspace override > env vars > build-time vars > default.
pub fn relay_api_base_url_with_override(state: &AppState) -> String {
    match workspace_relay_override(state) {
        Some(url) => relay_http_base_url(&url),
        None => relay_api_base_url(),
    }
}

/// Selects the relay a managed agent should use for a relay operation.
///
/// Always the active workspace relay. The legacy per-record `relay_url` pin is
/// deliberately IGNORED (agents-everywhere, #2122): every agent is eligible on
/// every community, and the pair the caller is acting on is identified by the
/// workspace relay, never by a stored pin. The record field is still parsed
/// and persisted untouched — old records need no migration and a rollback to a
/// pin-honoring build reads the same file — so the parameter stays in the
/// signature as documentation of what is being ignored at the one choke point
/// all agent relay resolution flows through. Resolving at read-time also means
/// a stale stored value can never leak into reconcile, spawn, or profile sync.
/// Uniform for both Local and Provider backends.
pub fn effective_agent_relay_url(_record_relay: &str, workspace_relay: &str) -> String {
    workspace_relay.to_string()
}

pub fn relay_http_base_url(relay_url: &str) -> String {
    let trimmed = relay_url.trim().trim_end_matches('/');

    if let Some(suffix) = trimmed.strip_prefix("wss://") {
        return format!("https://{}", suffix);
    }

    if let Some(suffix) = trimmed.strip_prefix("ws://") {
        return format!("http://{}", suffix);
    }

    trimmed.to_string()
}

pub fn relay_api_base_url() -> String {
    if let Some(base) = configured_env_var("BUZZ_RELAY_HTTP") {
        return base.trim_end_matches('/').to_string();
    }

    if let Some(base) = option_env!("BUZZ_DESKTOP_BUILD_RELAY_HTTP") {
        return base.trim().trim_end_matches('/').to_string();
    }

    relay_http_base_url(&relay_ws_url())
}

// ── NIP-98 HTTP auth ────────────────────────────────────────────────────────

/// Canonical signing bases resolved from GET /info, keyed by transport base
/// (the alias road). Caches only successfully resolved decisions — a relay
/// that later changes its advertisement is picked up on the next app start.
static CANONICAL_SIGNING_BASES: std::sync::LazyLock<
    std::sync::RwLock<std::collections::HashMap<String, String>>,
> = std::sync::LazyLock::new(|| std::sync::RwLock::new(std::collections::HashMap::new()));

/// Canonical NIP-42 WebSocket signing identities resolved from GET /info.
/// Keyed by the supplied WS connection URL (the transport). The value is
/// the canonical `wss://` identity to put in the AUTH event's relay tag.
static CANONICAL_WS_IDENTITIES: std::sync::LazyLock<
    std::sync::RwLock<std::collections::HashMap<String, String>>,
> = std::sync::LazyLock::new(|| std::sync::RwLock::new(std::collections::HashMap::new()));

/// Resolve the canonical NIP-42 WebSocket signing identity for a supplied
/// connection URL. Behind the Caddy Host rewrite, the relay expects the
/// canonical community URL (`wss://beehivenature.buzz`) in the AUTH
/// event's relay tag even when the socket connects to the alias
/// (`wss://relay2.skaists.dev`). Fetches `/info` on the HTTP equivalent
/// of the transport, reads `push.origin` (already `wss://` form), and
/// applies the same strict validation as the HTTP resolver. Fail-closed on
/// unreadable/malformed metadata. A well-formed document with no
/// advertised origin retains transport compatibility (the supplied URL).
pub async fn canonical_ws_signing_url(
    client: &reqwest::Client,
    ws_url: &str,
) -> Result<String, String> {
    let cached = CANONICAL_WS_IDENTITIES
        .read()
        .ok()
        .and_then(|cache| cache.get(ws_url).cloned());
    if let Some(hit) = cached {
        return Ok(hit);
    }

    // Convert the WS URL to its HTTP equivalent for the /info fetch.
    let http_base = if let Some(rest) = ws_url.trim().strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = ws_url.trim().strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        return Err(format!("supplied relay URL is not ws/wss: {ws_url}"));
    };
    let http_base = http_base.trim_end_matches('/').to_string();

    // Reuse the existing /info fetch + strict validation (HTTP form).
    let canonical_http = canonical_signing_base_with_client(client, &http_base).await?;

    // If the resolver returned the transport unchanged (no advertisement),
    // the WS identity is the supplied URL. Otherwise convert the canonical
    // HTTP base back to WS form (https→wss, http→ws).
    let canonical_ws = if canonical_http == http_base {
        ws_url.to_string()
    } else if let Some(rest) = canonical_http.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = canonical_http.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        return Err(format!(
            "canonical resolver returned a non-HTTP base: {canonical_http}"
        ));
    };

    if let Ok(mut cache) = CANONICAL_WS_IDENTITIES.write() {
        cache.insert(ws_url.to_string(), canonical_ws.clone());
    }
    Ok(canonical_ws)
}

/// Strictly validate an advertised ws/wss ORIGIN and construct its HTTP base
/// (mirror of the desktop TS law in shared/api/invites.ts): no userinfo,
/// query, fragment, or non-root path; ports and IPv6 literals are valid; the
/// HTTP base is constructed from the PARSED url, never the raw string.
pub fn validate_advertised_origin(advertised: &str) -> Option<String> {
    let url = url::Url::parse(advertised).ok()?;
    let scheme = match url.scheme() {
        "ws" => "http",
        "wss" => "https",
        _ => return None,
    };
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    if url.query().is_some() || url.fragment().is_some() {
        return None;
    }
    let path = url.path();
    if !path.is_empty() && path != "/" {
        return None;
    }
    let host = url.host_str()?;
    if host.is_empty() {
        return None;
    }
    match url.port() {
        Some(port) => Some(format!("{scheme}://{host}:{port}")),
        None => Some(format!("{scheme}://{host}")),
    }
}

/// Resolve the canonical SIGNING base for a transport base: GET /info on the
/// transport road and honor `push.origin` (the advertised canonical
/// identity). Fail-closed rules mirror the TS law: an unreadable document
/// (HTTP error, invalid JSON, non-object root, malformed push descriptor, or
/// malformed/invalid advertisement) is an ERROR — it proves nothing about
/// what the relay advertises, and silently signing the transport host is the
/// alias-host auth bug itself. Only a well-formed document with NO
/// advertisement means "the road IS the identity" (base returned unchanged).
/// Pure decision function: given a successfully-decoded `/info` JSON value
/// and the transport base, produce the canonical signing base or refuse.
/// Only an actually ABSENT key receives the compatibility fallback; an
/// explicit JSON `null` is malformed and refuses (both `push` and
/// `push.origin`). Extracted so the decision is unit-testable without HTTP.
pub fn canonical_decision_from_info(
    doc: &serde_json::Value,
    transport_base: &str,
) -> Result<String, String> {
    let obj = doc
        .as_object()
        .ok_or("relay /info returned a malformed document")?;
    match obj.get("push") {
        None => Ok(transport_base.to_string()),
        Some(serde_json::Value::Null) => {
            Err("relay /info returned a malformed push descriptor".to_string())
        }
        Some(push) => {
            let push_obj = push
                .as_object()
                .ok_or("relay /info returned a malformed push descriptor")?;
            match push_obj.get("origin") {
                None => Ok(transport_base.to_string()),
                Some(serde_json::Value::Null) => {
                    Err("relay /info advertises a malformed canonical origin".to_string())
                }
                Some(origin) => {
                    let advertised = origin
                        .as_str()
                        .ok_or("relay /info advertises a malformed canonical origin")?;
                    validate_advertised_origin(advertised)
                        .ok_or("relay /info canonical origin failed URL verification".to_string())
                }
            }
        }
    }
}

async fn canonical_signing_base_with_client(
    client: &reqwest::Client,
    transport_base: &str,
) -> Result<String, String> {
    let transport_base = transport_base.trim_end_matches('/');
    if let Some(hit) = CANONICAL_SIGNING_BASES
        .read()
        .ok()
        .and_then(|cache| cache.get(transport_base).cloned())
    {
        return Ok(hit);
    }
    let info_url = format!("{transport_base}/info");
    let response = client
        .get(&info_url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|error| {
            format!(
                "relay /info unreachable: {}",
                classify_request_error(&error)
            )
        })?;
    if !response.status().is_success() {
        return Err(format!("relay /info HTTP {}", response.status().as_u16()));
    }
    let text = response
        .text()
        .await
        .map_err(|_| "relay /info returned a malformed document".to_string())?;
    let doc: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| "relay /info returned a malformed document".to_string())?;
    let decision = canonical_decision_from_info(&doc, transport_base)?;
    if let Ok(mut cache) = CANONICAL_SIGNING_BASES.write() {
        cache.insert(transport_base.to_string(), decision.clone());
    }
    Ok(decision)
}

/// The URL a NIP-98 event must SIGN: the canonical origin (per /info) with
/// the transport URL's path appended. The HTTP request itself keeps riding
/// the transport URL — sign the identity, ride the road.
pub async fn canonical_sign_url(state: &AppState, transport_url: &str) -> Result<String, String> {
    canonical_sign_url_with_client(&state.http_client, transport_url).await
}

/// Client-only variant for tasks that own a cloned `reqwest::Client` instead
/// of the `AppState` reference (e.g. 'static spawned pipelines).
pub async fn canonical_sign_url_with_client(
    client: &reqwest::Client,
    transport_url: &str,
) -> Result<String, String> {
    let (scheme, after_scheme) = transport_url
        .split_once("://")
        .ok_or("relay transport URL has no scheme")?;
    let (authority, path) = match after_scheme.find('/') {
        Some(idx) => (&after_scheme[..idx], &after_scheme[idx..]),
        None => (after_scheme, ""),
    };
    let canonical_base =
        canonical_signing_base_with_client(client, &format!("{scheme}://{authority}")).await?;
    Ok(format!("{canonical_base}{path}"))
}

pub fn build_nip98_auth_header(
    method: &Method,
    url: &str,
    body: &[u8],
    state: &AppState,
) -> Result<String, String> {
    let keys = state.keys.lock().map_err(|error| error.to_string())?;
    build_nip98_auth_header_for_keys(&keys, method, url, body)
}

pub fn build_nip98_auth_header_for_keys(
    keys: &Keys,
    method: &Method,
    url: &str,
    body: &[u8],
) -> Result<String, String> {
    let payload_hash = hex::encode(Sha256::digest(body));

    // Nonce ensures unique event IDs even for identical requests in the same second.
    // Without this, rapid-fire calls (e.g. query → submit → re-query) with the same
    // body produce identical NIP-98 event hashes and trigger relay replay detection.
    let nonce_hex = uuid::Uuid::new_v4().to_string();

    let tags = vec![
        Tag::parse(vec!["u", url]).map_err(|error| format!("url tag failed: {error}"))?,
        Tag::parse(vec!["method", method.as_str()])
            .map_err(|error| format!("method tag failed: {error}"))?,
        Tag::parse(vec!["payload", &payload_hash])
            .map_err(|error| format!("payload tag failed: {error}"))?,
        Tag::parse(vec!["nonce", &nonce_hex])
            .map_err(|error| format!("nonce tag failed: {error}"))?,
    ];

    let event = EventBuilder::new(Kind::HttpAuth, "")
        .tags(tags)
        .sign_with_keys(keys)
        .map_err(|error| format!("sign failed: {error}"))?;

    Ok(format!(
        "Nostr {}",
        BASE64.encode(event.as_json().as_bytes())
    ))
}

// ── Error handling ──────────────────────────────────────────────────────────

/// Classify a `send()` failure into a stable, URL-free error string.
///
/// The returned string always starts with `"relay unreachable:"` so the
/// frontend connectivity classifier can detect it with a simple prefix check.
pub(crate) fn classify_request_error(e: &reqwest::Error) -> String {
    let display = e.to_string().to_lowercase();
    if e.is_timeout() {
        "relay unreachable: request timed out".to_string()
    } else if e.is_connect() {
        "relay unreachable: could not connect to relay".to_string()
    } else if display.contains("dns") || display.contains("failed to lookup") {
        "relay unreachable: relay host not found".to_string()
    } else {
        "relay unreachable: network error".to_string()
    }
}

/// Detect responses that were intercepted by a captive portal or auth proxy.
///
/// Returns `Some(msg)` when the response clearly did not come from the relay:
/// - Cloudflare Access redirect (final URL on `*.cloudflareaccess.com`)
/// - Any other HTML response (proxy login page, captive portal, etc.)
///
/// Pure function: takes the already-extracted host and content-type strings so
/// it can be unit-tested without constructing a real `reqwest::Response`.
fn classify_intercepted_response(final_host: &str, content_type: &str) -> Option<String> {
    let host = final_host.to_lowercase();
    let ct = content_type.to_lowercase();

    // Cloudflare Access intercepts requests and redirects to its own domain.
    // Label-boundary check prevents `notcloudflareaccess.com.evil.example` from
    // matching.
    if host == "cloudflareaccess.com" || host.ends_with(".cloudflareaccess.com") {
        return Some(
            "relay unreachable: network sign-in required (Cloudflare Access / VPN) \
             — re-authenticate and reconnect"
                .to_string(),
        );
    }

    // Generic HTML body from any other proxy or captive portal.
    if ct.contains("text/html") {
        return Some(
            "relay unreachable: relay returned an unexpected HTML page \
             (VPN or proxy sign-in?)"
                .to_string(),
        );
    }

    None
}

/// Deserialize a successful response as JSON, guarding against intercepted pages.
///
/// Extracts the final URL host and `Content-Type` header before consuming the
/// response body. If the response looks like a captive-portal page, returns the
/// appropriate `"relay unreachable:"` message instead of attempting JSON parsing.
/// URL details are deliberately omitted from error strings so raw URLs are never
/// surfaced in the UI.
pub(crate) async fn parse_json_response<T: DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, String> {
    let final_host = response.url().host_str().unwrap_or("").to_string();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if let Some(msg) = classify_intercepted_response(&final_host, &content_type) {
        return Err(msg);
    }

    // A successful HTTP response whose body fails to deserialize means the relay
    // was reached but returned something unexpected (protocol mismatch, relay bug,
    // corrupted body) — NOT a connectivity failure. Keep it off the
    // "relay unreachable:" bucket so it surfaces loudly instead of being treated
    // as a transient unreachable-relay condition. The reqwest error detail is
    // dropped because it contains the raw URL.
    response
        .json::<T>()
        .await
        .map_err(|_| MALFORMED_RESPONSE_MESSAGE.to_string())
}

/// Extract the `retry in Ns` hint from a rate-limit error string.
///
/// Matches the canonical format emitted by the relay in both HTTP 429 bodies
/// and CLOSED/NOTICE messages: `quota exceeded; retry in 4s`.
fn extract_retry_in_hint(body: &str) -> Option<u64> {
    let re_match = body.find("retry in ")?;
    let after = &body[re_match + "retry in ".len()..];
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u64>().ok()
}

pub async fn relay_error_message(response: reqwest::Response) -> String {
    let status = response.status();

    // Check for intercepted/proxy responses before reading the body.
    let final_host = response.url().host_str().unwrap_or("").to_string();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if let Some(msg) = classify_intercepted_response(&final_host, &content_type) {
        return msg;
    }

    // Real relay error: extract the structured message field if available.
    let body = response.text().await.unwrap_or_default();

    // 429 Too Many Requests → typed `relay rate-limited:` prefix so the TS
    // client can activate the rate-limit gate without confusing it with a
    // connectivity failure (`relay unreachable:`). Also arm the Rust-side
    // admission gate here — the one place every relay HTTP error funnels
    // through — so the next relay-backed command waits out the quota window
    // instead of burning it (see `relay_admission`).
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let hint = extract_retry_in_hint(&body);
        // Clamp the hint to MAX_HINT_SECONDS before arming the Rust gate AND
        // before embedding it in the returned string. Every consumer (Rust gate
        // via `activate_rate_limit` and TS gate via `applyTauriRateLimitIfNeeded`)
        // must see the same capped value — a single policy point prevents the TS
        // gate from receiving an uncapped hint from an untrusted relay.
        let capped_hint = hint.map(|s| s.min(crate::relay_admission::MAX_HINT_SECONDS));
        crate::relay_admission::activate_rate_limit(capped_hint);
        if let Some(secs) = capped_hint {
            return format!("relay rate-limited: retry in {secs}s");
        }
        return "relay rate-limited: quota exceeded".to_string();
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) {
        if let Some(message) = value.get("message").and_then(serde_json::Value::as_str) {
            return format!("relay returned {status}: {message}");
        }

        if let Some(error) = value.get("error").and_then(serde_json::Value::as_str) {
            return format!("relay returned {status}: {error}");
        }
    }

    // Non-JSON, non-HTML body: emit status only — no raw body in the UI.
    format!("relay returned {status}")
}

// ── HTTP bridge: POST /query ────────────────────────────────────────────────

/// Execute a one-shot query via the relay's HTTP bridge (`POST /query`).
///
/// Filters are serialized as a JSON array. The request is authenticated with
/// a NIP-98 event signed by the user's keys. Returns the deserialized array of
/// events.
pub async fn query_relay(
    state: &AppState,
    filters: &[serde_json::Value],
) -> Result<Vec<nostr::Event>, String> {
    query_relay_at(state, &relay_api_base_url_with_override(state), filters).await
}

/// Like [`query_relay`] but targets an explicit HTTP API base URL instead of
/// the workspace override. Used when a query must hit a specific relay (e.g.
/// reconciling an agent's profile on the relay where it was published).
pub async fn query_relay_at(
    state: &AppState,
    api_base_url: &str,
    filters: &[serde_json::Value],
) -> Result<Vec<nostr::Event>, String> {
    crate::relay_admission::wait_for_rate_limit().await;
    let url = format!("{}/query", api_base_url);
    let body_bytes =
        serde_json::to_vec(filters).map_err(|e| format!("filter serialization failed: {e}"))?;
    let sign_url = canonical_sign_url(state, &url).await?;
    let auth = build_nip98_auth_header(&Method::POST, &sign_url, &body_bytes, state)?;

    let response = state
        .http_client
        .post(&url)
        .header("Authorization", auth)
        .header("Content-Type", "application/json")
        .body(body_bytes)
        .send()
        .await
        .map_err(|e| classify_request_error(&e))?;

    if !response.status().is_success() {
        return Err(relay_error_message(response).await);
    }

    parse_json_response(response).await
}

pub async fn query_relay_at_with_keys(
    state: &AppState,
    api_base_url: &str,
    filters: &[serde_json::Value],
    keys: &Keys,
    auth_tag: Option<&str>,
) -> Result<Vec<nostr::Event>, String> {
    crate::relay_admission::wait_for_rate_limit().await;
    let url = format!("{}/query", api_base_url);
    let body_bytes =
        serde_json::to_vec(filters).map_err(|e| format!("filter serialization failed: {e}"))?;
    let sign_url = canonical_sign_url(state, &url).await?;
    let auth = build_nip98_auth_header_for_keys(keys, &Method::POST, &sign_url, &body_bytes)?;
    let mut request = state
        .http_client
        .post(&url)
        .header("Authorization", auth)
        .header("Content-Type", "application/json");
    if let Some(tag) = auth_tag {
        request = request.header("x-auth-tag", tag);
    }
    let response = request
        .body(body_bytes)
        .send()
        .await
        .map_err(|e| classify_request_error(&e))?;
    if !response.status().is_success() {
        return Err(relay_error_message(response).await);
    }
    parse_json_response(response).await
}

// ── Command response parsing ────────────────────────────────────────────────

/// Parse a command-event OK message of the form `"response:<json>"`.
///
/// Buzz's command kinds (e.g. 41010, 30620, 46020) acknowledge writes via
/// relay OK messages whose payload is a `response:`-prefixed JSON document.
/// This helper strips the prefix and deserializes the remainder as `T`.
pub fn parse_command_response<T: DeserializeOwned>(message: &str) -> Result<T, String> {
    // Try the spec format first: "response:{...}".
    if let Some(json) = message.strip_prefix("response:") {
        return serde_json::from_str(json).map_err(|e| format!("response parse failed: {e}"));
    }
    // Fallback: raw JSON (backward compat for relays that omit the prefix).
    serde_json::from_str(message)
        .map_err(|e| format!("expected 'response:' prefix or valid JSON, got: {message} ({e})"))
}

// ── Profile event builder ───────────────────────────────────────────────────

/// Build a signed kind:0 profile event, optionally injecting a verified NIP-OA auth tag.
///
/// This is a pure function (no I/O) extracted from `sync_managed_agent_profile` so that
/// the event-building and auth-tag-injection logic can be unit tested without HTTP calls.
///
/// `buzz-sdk` uses `nostr 0.36` while the desktop crate uses `nostr 0.37`. Cross-version
/// bridging is done via hex-encoded public keys and raw tag slices — both versions share the
/// same wire format.
fn build_profile_event(
    agent_keys: &nostr::Keys,
    display_name: &str,
    avatar_url: Option<&str>,
    auth_tag_json: Option<&str>,
) -> Result<nostr::Event, String> {
    let builder = crate::events::build_profile(Some(display_name), None, avatar_url, None, None)?;

    let builder = if let Some(tag_json) = auth_tag_json {
        // Bridge nostr 0.37 PublicKey → nostr 0.36 PublicKey via hex encoding.
        let agent_pubkey_hex = agent_keys.public_key().to_hex();
        let compat_pubkey = nostr::PublicKey::from_hex(&agent_pubkey_hex)
            .map_err(|e| format!("failed to convert agent pubkey for auth verification: {e}"))?;

        // Verify Schnorr signature before injecting into profile event.
        buzz_sdk_pkg::nip_oa::verify_auth_tag(tag_json, &compat_pubkey)
            .map_err(|e| format!("auth tag verification failed for profile event: {e}"))?;

        // parse_auth_tag returns a nostr 0.36 Tag; bridge to nostr 0.37 via raw slice.
        let compat_tag = buzz_sdk_pkg::nip_oa::parse_auth_tag(tag_json)
            .map_err(|e| format!("failed to parse verified auth tag: {e}"))?;
        let tag = nostr::Tag::parse(compat_tag.as_slice())
            .map_err(|e| format!("failed to convert auth tag to nostr 0.37: {e}"))?;
        builder.tags([tag])
    } else {
        builder
    };

    builder
        .sign_with_keys(agent_keys)
        .map_err(|e| format!("failed to sign profile event: {e}"))
}

// ── Managed-agent profile sync ──────────────────────────────────────────────

/// Sync a managed agent's kind:0 profile event to the relay using NIP-98 auth.
///
/// The agent signs its own profile event and the NIP-98 HTTP-auth event, so no
/// API token is required.
pub async fn sync_managed_agent_profile(
    state: &AppState,
    relay_url: &str,
    agent_keys: &nostr::Keys,
    display_name: &str,
    avatar_url: Option<&str>,
    auth_tag: Option<&str>, // NIP-OA auth tag JSON
) -> Result<(), String> {
    crate::relay_admission::wait_for_rate_limit().await;
    // Build a signed kind:0 profile event (with optional NIP-OA auth tag).
    let event = build_profile_event(agent_keys, display_name, avatar_url, auth_tag)?;
    let event_json = event.as_json();
    let body_bytes = event_json.into_bytes();
    crate::egress_guard::assert_no_key_backup_bytes(&body_bytes, "agent profile sync")?;

    let url = format!("{}/events", relay_http_base_url(relay_url));
    let sign_url = canonical_sign_url(state, &url).await?;
    let auth = build_nip98_auth_header_for_keys(agent_keys, &Method::POST, &sign_url, &body_bytes)?;

    let mut request = state
        .http_client
        .post(&url)
        .header("Authorization", auth)
        .header("Content-Type", "application/json");
    if let Some(tag) = auth_tag {
        request = request.header("x-auth-tag", tag);
    }
    let response = request
        .body(body_bytes)
        .send()
        .await
        .map_err(|e| classify_request_error(&e))?;

    if !response.status().is_success() {
        let msg = relay_error_message(response).await;
        return Err(format!(
            "Could not sync the agent's profile metadata: {msg}"
        ));
    }

    Ok(())
}

// ── Agent profile query ─────────────────────────────────────────────────────

/// Query the relay for an agent's kind:0 profile event.
///
/// Queries the relay identified by `relay_url`. Callers uniformly pass the
/// relay resolved by `effective_agent_relay_url` for every agent regardless of
/// backend — always the active workspace relay — so the query targets the host
/// the profile is actually published to.
///
/// Returns the parsed profile content (display_name, picture) if a kind:0 event
/// exists for the given pubkey, or `None` if no profile is published.
pub async fn query_agent_profile(
    state: &AppState,
    relay_url: &str,
    agent_pubkey: &str,
) -> Result<Option<AgentProfileInfo>, String> {
    let filter = serde_json::json!({
        "authors": [agent_pubkey],
        "kinds": [0],
        "limit": 1
    });

    let events = query_relay_at(state, &relay_http_base_url(relay_url), &[filter]).await?;

    let Some(event) = events.first() else {
        return Ok(None);
    };

    let Ok(content) = serde_json::from_str::<serde_json::Value>(&event.content) else {
        return Ok(None);
    };

    Ok(Some(AgentProfileInfo {
        display_name: content
            .get("display_name")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        picture: content
            .get("picture")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    }))
}

/// Parsed fields from a kind:0 profile event.
#[derive(Debug, Clone)]
pub struct AgentProfileInfo {
    pub display_name: Option<String>,
    pub picture: Option<String>,
}

// ── Signed-event submission ─────────────────────────────────────────────────

mod get;
pub use get::get_relay_json;

mod submit;
pub use submit::{
    submit_event, submit_event_at_with_keys, submit_signed_event_at_with_keys, SubmitEventResponse,
};

/// Sign an event with explicit keys and POST it to `/events` with NIP-98 auth.
///
/// Managed-agent flows use this to publish as the agent itself while still
/// including the stored NIP-OA auth tag when the relay requires owner-backed
/// membership.
pub async fn submit_event_with_keys(
    builder: nostr::EventBuilder,
    state: &AppState,
    keys: &Keys,
    auth_tag: Option<&str>,
) -> Result<SubmitEventResponse, String> {
    let event = builder
        .sign_with_keys(keys)
        .map_err(|e| format!("failed to sign event: {e}"))?;
    submit_signed_event_with_keys(&event, state, keys, auth_tag).await
}

/// POST an already-signed event using the same explicit identity for NIP-98.
pub async fn submit_signed_event_with_keys(
    event: &nostr::Event,
    state: &AppState,
    keys: &Keys,
    auth_tag: Option<&str>,
) -> Result<SubmitEventResponse, String> {
    if event.pubkey != keys.public_key() {
        return Err("signed event does not match the publishing identity".to_string());
    }
    crate::relay_admission::wait_for_rate_limit().await;
    let url = format!("{}/events", relay_api_base_url_with_override(state));
    let body_bytes = event.as_json().into_bytes();
    crate::egress_guard::assert_no_key_backup_bytes(&body_bytes, "signed event submit (keys)")?;
    let sign_url = canonical_sign_url(state, &url).await?;
    let auth_header =
        build_nip98_auth_header_for_keys(keys, &Method::POST, &sign_url, &body_bytes)?;

    let mut request = state
        .http_client
        .post(&url)
        .header("Authorization", auth_header)
        .header("Content-Type", "application/json");
    if let Some(tag) = auth_tag {
        request = request.header("x-auth-tag", tag);
    }

    let response = request
        .body(body_bytes)
        .send()
        .await
        .map_err(|e| classify_request_error(&e))?;

    if !response.status().is_success() {
        return Err(relay_error_message(response).await);
    }

    let result: SubmitEventResponse = parse_json_response(response).await?;

    if !result.accepted {
        return Err(format!("relay rejected event: {}", result.message));
    }

    Ok(result)
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{
        build_profile_event, canonical_decision_from_info, classify_intercepted_response,
        effective_agent_relay_url, extract_retry_in_hint, parse_command_response,
        relay_http_base_url, validate_advertised_origin, MALFORMED_RESPONSE_MESSAGE,
    };
    use serde::Deserialize;

    #[test]
    fn advertised_origin_accepts_valid_origins_and_constructs_http_base() {
        assert_eq!(
            validate_advertised_origin("wss://beehivenature.buzz").as_deref(),
            Some("https://beehivenature.buzz")
        );
        // ports and IPv6 literals are valid origins
        assert_eq!(
            validate_advertised_origin("ws://[::1]:3000").as_deref(),
            Some("http://[::1]:3000")
        );
        assert_eq!(
            validate_advertised_origin("wss://relay.example:8443/").as_deref(),
            Some("https://relay.example:8443")
        );
    }

    #[test]
    fn advertised_origin_rejects_non_origin_components() {
        for bad in [
            "not a url",
            "ftp://relay.example",
            "wss://relay.example#section",
            "wss://relay.example?x=1",
            "wss://relay.example/nested",
            "wss://synthetic:synthetic@relay.example",
        ] {
            assert!(
                validate_advertised_origin(bad).is_none(),
                "{bad:?} must be refused"
            );
        }
    }

    // ── canonical decision regressions (review round 1) ─────────────────

    #[test]
    fn canonical_decision_rejects_explicit_null_push() {
        let doc = serde_json::json!({"push": null});
        assert!(
            canonical_decision_from_info(&doc, "https://relay2.skaists.dev").is_err(),
            "explicit null push is a malformed descriptor, not an absent key"
        );
    }

    #[test]
    fn canonical_decision_rejects_explicit_null_origin() {
        let doc = serde_json::json!({"push": {"origin": null}});
        assert!(
            canonical_decision_from_info(&doc, "https://relay2.skaists.dev").is_err(),
            "explicit null origin is a malformed advertisement, not an absent key"
        );
    }

    #[test]
    fn canonical_decision_compat_absent_key_is_road_is_identity() {
        // well-formed object with NO push key — the road IS the identity
        let doc = serde_json::json!({"name": "Buzz Relay", "version": "0.2.1"});
        assert_eq!(
            canonical_decision_from_info(&doc, "https://relay.example").unwrap(),
            "https://relay.example"
        );
        // push present as a valid object with NO origin key
        let doc = serde_json::json!({"push": {"keys": []}});
        assert_eq!(
            canonical_decision_from_info(&doc, "https://relay.example").unwrap(),
            "https://relay.example"
        );
    }

    #[test]
    fn canonical_decision_valid_origin_signs_canonical() {
        let doc = serde_json::json!({
            "push": {"origin": "wss://beehivenature.buzz"}
        });
        assert_eq!(
            canonical_decision_from_info(&doc, "https://relay2.skaists.dev").unwrap(),
            "https://beehivenature.buzz"
        );
    }

    #[test]
    fn canonical_decision_rejects_non_object_root() {
        for doc in [
            serde_json::json!(null),
            serde_json::json!([1, 2]),
            serde_json::json!("a string"),
            serde_json::json!(42),
        ] {
            assert!(
                canonical_decision_from_info(&doc, "https://relay.example").is_err(),
                "non-object root must be refused"
            );
        }
    }

    #[test]
    fn canonical_decision_rejects_malformed_push_shape() {
        let cases = vec![
            serde_json::json!("not-an-object"),
            serde_json::json!(42),
            serde_json::json!([1]),
        ];
        for push in cases {
            let doc = serde_json::json!({"push": push});
            assert!(canonical_decision_from_info(&doc, "https://relay.example").is_err());
        }
    }

    // ── extract_retry_in_hint ────────────────────────────────────────────────

    #[test]
    fn extracts_hint_from_429_body() {
        assert_eq!(
            extract_retry_in_hint(r#"{"error":"rate-limited: quota exceeded; retry in 4s"}"#),
            Some(4)
        );
    }

    #[test]
    fn extracts_hint_when_no_json_wrapper() {
        assert_eq!(extract_retry_in_hint("retry in 30s"), Some(30));
    }

    #[test]
    fn returns_none_when_no_hint_present() {
        assert_eq!(
            extract_retry_in_hint(r#"{"error":"rate-limited: quota exceeded"}"#),
            None
        );
        assert_eq!(extract_retry_in_hint(""), None);
    }

    #[test]
    fn overlong_digit_string_returns_none() {
        // A digit sequence that exceeds u64::MAX cannot be parsed; the function
        // must return None (→ caller uses the default) rather than panicking.
        assert_eq!(
            extract_retry_in_hint("retry in 99999999999999999999999s"),
            None
        );
    }

    // ── relay_error_message: hint capping ────────────────────────────────────
    //
    // Verify that an oversized relay hint is capped in the returned message
    // string, not just inside `activate_rate_limit()`. This guarantees every
    // consumer — including the TS gate via `applyTauriRateLimitIfNeeded` —
    // receives the capped value rather than the raw untrusted relay value.

    #[tokio::test]
    async fn oversized_hint_is_capped_in_relay_error_message_string() {
        use crate::relay_admission::{reset_rate_limit_gate, MAX_HINT_SECONDS, TEST_SERIAL};
        use std::io::{Read as _, Write as _};

        let _serial = TEST_SERIAL.lock().await;
        reset_rate_limit_gate();

        // Use a std::net listener on a std::thread — the same pattern as the
        // relay_admission loopback tests. This avoids two races that cause CI
        // failures with tokio::net + into_std():
        //  1. No request read: the client is still sending when the response
        //     arrives → hyper `UnexpectedMessage`/`Canceled` under load.
        //  2. into_std() leaves the socket in nonblocking mode → write_all
        //     may return WouldBlock and silently drop the response.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        // Serve a 429 with a hint far exceeding MAX_HINT_SECONDS (300).
        let oversized = 1_000_000u64;
        let body = format!(r#"{{"error":"rate-limited: quota exceeded; retry in {oversized}s"}}"#);
        let body_len = body.len();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // Read the request first so the client finishes sending before
                // we write the response — mirrors relay_admission.rs pattern.
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {body_len}\r\nConnection: close\r\n\r\n{body}"
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });

        let client = reqwest::Client::new();
        let response = client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .expect("request must succeed");

        let msg = super::relay_error_message(response).await;

        // The message must embed the CAPPED hint, not the raw 1 000 000.
        assert_eq!(
            msg,
            format!("relay rate-limited: retry in {MAX_HINT_SECONDS}s"),
            "relay_error_message must embed the capped hint, not the raw untrusted value"
        );
        assert!(
            !msg.contains(&oversized.to_string()),
            "raw oversized hint must not appear in the message string"
        );
        reset_rate_limit_gate();
    }

    // ── effective_agent_relay_url: legacy pin ignored ─────────────────────────

    #[test]
    fn stored_relay_pin_is_ignored() {
        // Zero-touch cutover (#2122): a creation-era per-record relay pin is
        // parsed and persisted but never consulted — the workspace relay wins.
        assert_eq!(
            effective_agent_relay_url("wss://relay.other.com", "wss://staging.example.com"),
            "wss://staging.example.com"
        );
    }

    #[test]
    fn empty_relay_resolves_to_workspace() {
        // A never-set record resolves to the active workspace relay at read-time,
        // so a stale stored default can never make it load-bearing.
        assert_eq!(
            effective_agent_relay_url("", "wss://staging.example.com"),
            "wss://staging.example.com"
        );
    }

    #[test]
    fn whitespace_only_relay_resolves_to_workspace() {
        // Whitespace-only behaves identically — no value survives.
        assert_eq!(
            effective_agent_relay_url("   ", "wss://staging.example.com"),
            "wss://staging.example.com"
        );
    }

    // ── relay_http_base_url scheme conversion ────────────────────────────────

    #[test]
    fn loopback_ws_localhost_preserves_authority() {
        // Tenant host-binding keys off the HTTP Host/authority. The desktop must
        // not rewrite localhost to 127.0.0.1, or local dev HTTP calls target a
        // different unmapped community than the WebSocket URL.
        assert_eq!(
            relay_http_base_url("ws://localhost:3000"),
            "http://localhost:3000"
        );
    }

    #[test]
    fn loopback_trailing_slash_removed_authority_preserved() {
        assert_eq!(
            relay_http_base_url("ws://localhost:3000/"),
            "http://localhost:3000"
        );
    }

    #[test]
    fn remote_wss_host_unchanged() {
        assert_eq!(
            relay_http_base_url("wss://relay.example.com"),
            "https://relay.example.com"
        );
    }

    #[test]
    fn loopback_ipv4_literal_unchanged() {
        assert_eq!(
            relay_http_base_url("ws://127.0.0.1:3000"),
            "http://127.0.0.1:3000"
        );
    }

    #[test]
    fn localhost_substring_host_unchanged() {
        assert_eq!(
            relay_http_base_url("ws://localhost.evil.com:3000"),
            "http://localhost.evil.com:3000"
        );
    }

    #[test]
    fn loopback_wss_localhost_preserves_authority() {
        assert_eq!(
            relay_http_base_url("wss://localhost:3000"),
            "https://localhost:3000"
        );
    }

    // ── classify_intercepted_response ────────────────────────────────────────

    #[test]
    fn intercepted_cloudflare_host_returns_some() {
        let result = classify_intercepted_response("sqprod.cloudflareaccess.com", "text/html");
        assert!(result.is_some());
        let msg = result.unwrap();
        assert!(
            msg.starts_with("relay unreachable:"),
            "should have unreachable prefix"
        );
        assert!(msg.contains("Cloudflare"), "should mention Cloudflare");
    }

    #[test]
    fn intercepted_cloudflare_apex_host_returns_some() {
        // The apex domain itself should also match.
        let result = classify_intercepted_response("cloudflareaccess.com", "application/json");
        assert!(result.is_some());
        let msg = result.unwrap();
        assert!(msg.starts_with("relay unreachable:"));
        assert!(msg.contains("Cloudflare"));
    }

    #[test]
    fn intercepted_non_cloudflare_html_returns_some() {
        let result =
            classify_intercepted_response("proxy.corporate.example", "text/html; charset=utf-8");
        assert!(result.is_some());
        let msg = result.unwrap();
        assert!(msg.starts_with("relay unreachable:"));
    }

    #[test]
    fn normal_relay_json_returns_none() {
        let result = classify_intercepted_response("relay.myapp.example.com", "application/json");
        assert!(result.is_none());
    }

    #[test]
    fn content_type_case_insensitive() {
        // Uppercase content-type must still be detected.
        let result = classify_intercepted_response("proxy.example.com", "TEXT/HTML");
        assert!(result.is_some());
        assert!(result.unwrap().starts_with("relay unreachable:"));
    }

    #[test]
    fn evil_suffix_does_not_match_cloudflare() {
        // A host whose suffix happens to contain the Cloudflare string but is
        // not actually a subdomain must NOT match.
        let result = classify_intercepted_response(
            "notcloudflareaccess.com.evil.example",
            "application/json",
        );
        assert!(
            result.is_none(),
            "false suffix match should not trigger Cloudflare branch"
        );
    }

    // classify_request_error requires a real reqwest::Error (not publicly
    // constructable) — tested indirectly through integration; skipped here.

    // ── parse_json_response malformed-body contract ──────────────────────────

    #[test]
    fn malformed_response_message_stays_off_unreachable_bucket() {
        // A reached-but-malformed 2xx body is not a connectivity failure. If this
        // message ever regains the "relay unreachable:" prefix, the frontend
        // classifier would misroute it as unreachable — pin that it never does.
        assert!(
            !MALFORMED_RESPONSE_MESSAGE.starts_with("relay unreachable:"),
            "malformed-response message must not match the unreachable prefix"
        );
    }

    // ── parse_command_response ───────────────────────────────────────────────

    #[derive(Debug, Deserialize, PartialEq)]
    struct ChannelCreated {
        channel_id: String,
    }

    #[test]
    fn parse_command_response_decodes_typed_payload() {
        let msg = r#"response:{"channel_id":"abc123"}"#;
        let parsed: ChannelCreated = parse_command_response(msg).expect("should parse");
        assert_eq!(
            parsed,
            ChannelCreated {
                channel_id: "abc123".to_string()
            }
        );
    }

    #[test]
    fn parse_command_response_accepts_raw_json_fallback() {
        // Backward-compat: relays that emit raw JSON (no prefix) still work.
        let msg = r#"{"channel_id":"abc"}"#;
        let parsed: ChannelCreated = parse_command_response(msg).expect("fallback parse");
        assert_eq!(
            parsed,
            ChannelCreated {
                channel_id: "abc".to_string()
            }
        );
    }

    #[test]
    fn parse_command_response_rejects_invalid_prefixed_json() {
        let msg = "response:not-json";
        let result: Result<ChannelCreated, _> = parse_command_response(msg);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("response parse failed"));
    }

    #[test]
    fn parse_command_response_rejects_garbage() {
        let msg = "totally not json or response";
        let result: Result<ChannelCreated, _> = parse_command_response(msg);
        assert!(result.is_err());
    }

    // ── build_profile_event ──────────────────────────────────────────────────

    /// Generate a valid NIP-OA auth tag JSON string signed by a fresh owner key
    /// and addressed to `agent_keys`.
    ///
    /// Uses `nostr_compat` (nostr 0.36) for the owner keys because
    /// `buzz_sdk_pkg::nip_oa::compute_auth_tag` expects nostr 0.36 types.
    /// The agent pubkey is bridged via hex encoding.
    fn make_valid_auth_tag(agent_keys: &nostr::Keys) -> String {
        let owner_keys = nostr::Keys::generate();
        let agent_pubkey_hex = agent_keys.public_key().to_hex();
        let agent_compat_pubkey =
            nostr::PublicKey::from_hex(&agent_pubkey_hex).expect("valid hex pubkey should parse");
        buzz_sdk_pkg::nip_oa::compute_auth_tag(&owner_keys, &agent_compat_pubkey, "")
            .expect("compute_auth_tag should not fail with distinct keys")
    }

    #[test]
    fn profile_event_with_valid_auth_tag() {
        let agent_keys = nostr::Keys::generate();
        let tag_json = make_valid_auth_tag(&agent_keys);
        let event = build_profile_event(&agent_keys, "TestBot", None, Some(&tag_json))
            .expect("should succeed with a valid auth tag");

        // Exactly one "auth" tag must be present.
        let auth_tags: Vec<_> = event
            .tags
            .iter()
            .filter(|t| t.as_slice().first().map(|s| s.as_str()) == Some("auth"))
            .collect();
        assert_eq!(auth_tags.len(), 1, "expected exactly 1 auth tag");

        // Must be a kind:0 (Metadata) event.
        assert_eq!(event.kind, nostr::Kind::Metadata);
    }

    #[test]
    fn profile_event_without_auth_tag() {
        let agent_keys = nostr::Keys::generate();
        let event = build_profile_event(&agent_keys, "TestBot", None, None)
            .expect("should succeed without an auth tag");

        // No "auth" tags should be present.
        let auth_tags: Vec<_> = event
            .tags
            .iter()
            .filter(|t| t.as_slice().first().map(|s| s.as_str()) == Some("auth"))
            .collect();
        assert_eq!(auth_tags.len(), 0, "expected no auth tags");

        assert_eq!(event.kind, nostr::Kind::Metadata);
    }

    #[test]
    fn profile_event_rejects_invalid_auth_tag() {
        let agent_keys = nostr::Keys::generate();
        // Structurally valid JSON array but with a bogus signature — verification must fail.
        let bad_json = format!(r#"["auth","{}","","{}"]"#, "a".repeat(64), "b".repeat(128));
        let result = build_profile_event(&agent_keys, "TestBot", None, Some(&bad_json));
        assert!(result.is_err(), "should reject an invalid auth tag");
        assert!(
            result.unwrap_err().contains("verification failed"),
            "error message should mention verification failure"
        );
    }
}
