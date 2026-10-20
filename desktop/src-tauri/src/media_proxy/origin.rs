//! Relay media on alias roads.
//!
//! A relay reached through an alias road (e.g. `relay2.skaists.dev`, with Caddy
//! rewriting Host to the canonical `beehivenature.buzz`) stamps every stored
//! media URL with its canonical community origin and checks Blossom `server`
//! tags (BUD-11) against the canonical host. This module resolves that
//! canonical base (GET /info `push.origin`, via the NIP-98 resolver in
//! `relay.rs`), maps canonical-origin media URLs onto the transport road, and
//! mints media GET tokens for requests that always travel to the transport.
//!
//! Token scope on an alias road: the relay accepts a GET token whose `x` tag
//! names the requested blob OR whose `server` tag names its host, so any
//! `server` tag would make the token a bearer for the whole server named.
//! Alias-road GET tokens therefore carry only the blob (`x`) and no `server`
//! tag: they authorise that one blob, and do not depend on what the transport
//! relay advertises about its identity. Canonical roads keep the existing
//! server-scoped token unchanged.
//!
//! Trusted aliases: an advertised canonical origin is honoured only when it
//! matches an explicit pin for that transport in `relay-alias-pins.json` in
//! the app data directory. The file is owner-edited; any process running as
//! the same Windows user can write it (as it can the app itself). An
//! unpinned or mismatched advertisement is ignored and the road behaves as a
//! canonical road: transport only, as before alias support.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, OnceLock, RwLock};
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use nostr::{EventBuilder, JsonUtil, Keys, Kind, Tag, Timestamp};
use serde::Serialize;
use tauri::State;

use crate::app_state::AppState;
use crate::commands::media::{mint_media_get_auth, MEDIA_GET_AUTH_EXPIRY_SECS};
use crate::commands::media_download::validate_download_url;
use crate::relay::{
    canonical_signing_base_with_client, relay_api_base_url_with_override, CANONICAL_SIGNING_BASES,
};

/// Upper bound on one `/info` lookup made on behalf of a media request.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

/// How long a failed lookup is remembered, so a broken `/info` costs one
/// bounded lookup per window instead of one per image or range request.
const FAILURE_TTL: Duration = Duration::from_secs(60);

/// The pin file name inside the app data directory.
pub(crate) const PIN_FILE: &str = "relay-alias-pins.json";

/// Directory holding [`PIN_FILE`]; set once at startup by the media proxy.
static PIN_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Loaded pins (transport origin -> canonical origin); `None` until loaded.
static PINS: LazyLock<RwLock<Option<HashMap<String, String>>>> =
    LazyLock::new(|| RwLock::new(None));

/// Record where the pin file lives (the app data directory).
pub(crate) fn set_pin_dir(dir: Option<PathBuf>) {
    if let Some(dir) = dir {
        let _ = PIN_DIR.set(dir);
    }
}

fn is_web_origin(origin: &str) -> bool {
    url::Url::parse(origin).is_ok_and(|u| matches!(u.scheme(), "https" | "http") && u.has_host())
}

/// Parse a pin file: `{ "<transport>": "<canonical>" }`. Entries are
/// normalised to origins; malformed entries and self-pins are dropped.
pub(crate) fn parse_pins(text: &str) -> HashMap<String, String> {
    let text = text.trim_start_matches('\u{feff}');
    let raw: HashMap<String, String> = match serde_json::from_str(text) {
        Ok(raw) => raw,
        Err(error) => {
            eprintln!("buzz-desktop: {PIN_FILE} unreadable, no aliases trusted: {error}");
            return HashMap::new();
        }
    };
    let total = raw.len();
    let pins: HashMap<String, String> = raw
        .into_iter()
        .map(|(transport, canonical)| (origin_of(&transport), origin_of(&canonical)))
        .filter(|(transport, canonical)| {
            is_web_origin(transport) && is_web_origin(canonical) && transport != canonical
        })
        .collect();
    if pins.len() != total {
        eprintln!(
            "buzz-desktop: {PIN_FILE}: ignored {} malformed entr(y/ies)",
            total - pins.len()
        );
    }
    pins
}

fn load_pins() -> HashMap<String, String> {
    PIN_DIR
        .get()
        .and_then(|dir| std::fs::read_to_string(dir.join(PIN_FILE)).ok())
        .map(|text| parse_pins(&text))
        .unwrap_or_default()
}

fn pins() -> HashMap<String, String> {
    if let Some(loaded) = PINS.read().ok().and_then(|pins| pins.clone()) {
        return loaded;
    }
    // Before the app data directory is known nothing is cached, so an early
    // read cannot pin "no aliases" for the rest of the session.
    if PIN_DIR.get().is_none() {
        return HashMap::new();
    }
    let Ok(mut guard) = PINS.write() else {
        return load_pins();
    };
    guard.get_or_insert_with(load_pins).clone()
}

#[cfg(test)]
/// Explicitly trust `canonical` as the identity of `transport` (tests only; pins
/// are edited by hand in the file): kept in
/// memory and, when the app data directory is known, written to the pin file.
pub(crate) fn pin_alias(transport: &str, canonical: &str) -> Result<(), String> {
    let (transport, canonical) = (origin_of(transport), origin_of(canonical));
    if !is_web_origin(&transport) || !is_web_origin(&canonical) || transport == canonical {
        return Err("a pin needs two different http(s) origins".to_string());
    }
    let mut guard = PINS.write().map_err(|_| "pin store poisoned".to_string())?;
    let all = guard.get_or_insert_with(load_pins);
    all.insert(transport, canonical);
    if let Some(dir) = PIN_DIR.get() {
        let text = serde_json::to_string_pretty(&*all).map_err(|e| e.to_string())?;
        let tmp = dir.join(format!("{PIN_FILE}.tmp"));
        std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, dir.join(PIN_FILE)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The identity to honour for `transport` given what its `/info` advertises:
/// the advertisement only when it is the transport itself or matches the
/// explicit pin for this transport; otherwise the transport.
pub(crate) fn trusted_identity(
    transport: &str,
    advertised: &str,
    pins: &HashMap<String, String>,
) -> String {
    let (transport, advertised) = (origin_of(transport), origin_of(advertised));
    if advertised == transport {
        return transport;
    }
    match pins.get(&transport) {
        Some(pinned) if *pinned == advertised => advertised,
        _ => {
            eprintln!(
                "buzz-desktop: ignoring unpinned relay identity {advertised} advertised by {transport}"
            );
            transport
        }
    }
}

/// Transport origins whose `/info` lookup failed recently.
static FAILED_LOOKUPS: LazyLock<RwLock<HashMap<String, Instant>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Single-flight gate: a cold cache with many media requests in flight makes
/// one `/info` lookup, not one per request.
static LOOKUP_GATE: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

/// The origin of `base` (`scheme://host[:port]`, lowercased, no path or
/// trailing slash), or `base` trimmed when it does not parse. Used for cache
/// keys and every alias-road comparison so casing and paths cannot make a
/// canonical road look like an alias road.
pub(crate) fn origin_of(base: &str) -> String {
    match url::Url::parse(base) {
        Ok(parsed) if parsed.has_host() => parsed.origin().ascii_serialization(),
        _ => base.trim_end_matches('/').to_string(),
    }
}

fn cached_canonical(transport: &str) -> Option<String> {
    CANONICAL_SIGNING_BASES
        .read()
        .ok()
        .and_then(|cache| cache.get(transport).cloned())
}

fn recently_failed(transport: &str) -> bool {
    FAILED_LOOKUPS
        .read()
        .ok()
        .and_then(|failures| failures.get(transport).copied())
        .is_some_and(|at| at.elapsed() < FAILURE_TTL)
}

fn record_failure(transport: &str) {
    if let Ok(mut failures) = FAILED_LOOKUPS.write() {
        failures.insert(transport.to_string(), Instant::now());
    }
}

/// Strict resolution: `Ok(canonical base)` once `/info` was read (the
/// transport origin itself when nothing is advertised), `Err` while it is not.
pub(crate) async fn resolve_canonical_base(
    client: &reqwest::Client,
    transport_base: &str,
) -> Result<String, String> {
    let transport = origin_of(transport_base);
    match tokio::time::timeout(
        LOOKUP_TIMEOUT,
        canonical_signing_base_with_client(client, &transport),
    )
    .await
    {
        Ok(result) => result.map(|advertised| trusted_identity(&transport, &advertised, &pins())),
        Err(_) => Err("relay /info timed out".to_string()),
    }
}

/// The base relay media is named by: the canonical community origin, else
/// the transport origin. Lenient by design: an unreadable `/info` yields the
/// transport, which is exactly the behaviour before alias roads were
/// understood, and requests still travel only to the transport.
pub(crate) async fn media_identity_base_with_client(
    client: &reqwest::Client,
    transport_base: &str,
) -> String {
    let transport = origin_of(transport_base);
    if let Some(hit) = cached_canonical(&transport) {
        return trusted_identity(&transport, &hit, &pins());
    }
    if recently_failed(&transport) {
        return transport;
    }
    let _gate = LOOKUP_GATE.lock().await;
    if let Some(hit) = cached_canonical(&transport) {
        return trusted_identity(&transport, &hit, &pins());
    }
    if recently_failed(&transport) {
        return transport;
    }
    match resolve_canonical_base(client, &transport).await {
        Ok(base) => base,
        Err(error) => {
            record_failure(&transport);
            eprintln!("buzz-desktop: relay media identity unavailable for {transport}: {error}");
            transport
        }
    }
}

pub(crate) async fn media_identity_base(state: &AppState, transport_base: &str) -> String {
    media_identity_base_with_client(&state.http_client, transport_base).await
}

/// Whether `identity_base` names a different origin than `transport_base`.
pub(crate) fn is_alias_road(transport_base: &str, identity_base: &str) -> bool {
    origin_of(transport_base) != origin_of(identity_base)
}

/// The relay's transport origin and the canonical origin it advertises, so the
/// frontend can tell which relay a late answer belongs to.
#[derive(Serialize)]
pub struct RelayCanonicalOrigin {
    pub transport: String,
    pub canonical: String,
}

/// The relay's canonical HTTP origin (GET /info `push.origin`; the transport
/// origin when nothing is advertised), with the transport it was resolved for.
/// Errors while `/info` is unreadable so the frontend keeps retrying instead
/// of caching a fallback for the session.
#[tauri::command]
pub async fn get_relay_canonical_http_url(
    state: State<'_, AppState>,
) -> Result<RelayCanonicalOrigin, String> {
    let transport = origin_of(&relay_api_base_url_with_override(&state));
    let canonical = resolve_canonical_base(&state.http_client, &transport).await?;
    Ok(RelayCanonicalOrigin {
        transport,
        canonical,
    })
}

/// The sha256 a relay media path names: `/media/<64-hex>[.thumb].<ext>`,
/// optionally followed by a query.
pub(crate) fn media_hash_from_path(path_and_query: &str) -> Option<&str> {
    let file = path_and_query.strip_prefix("/media/")?;
    let file = file.split(['?', '#']).next()?;
    let hash = file.split('.').next()?;
    (hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hash)
}

/// The scope of a media GET token.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MediaGetScope<'a> {
    /// Canonical road: the existing server-scoped token naming this base.
    Server(String),
    /// Alias road: a token for exactly this blob, with no `server` tag.
    Blob(&'a str),
}

/// What a media GET token for `path_and_query` must authorise. `None` means
/// mint no token (an alias-road path that names no blob).
pub(crate) fn media_get_scope<'a>(
    transport_base: &str,
    identity_base: &str,
    path_and_query: &'a str,
) -> Option<MediaGetScope<'a>> {
    if !is_alias_road(transport_base, identity_base) {
        return Some(MediaGetScope::Server(origin_of(transport_base)));
    }
    media_hash_from_path(path_and_query).map(MediaGetScope::Blob)
}

/// Sign a kind-24242 `t=get` token for exactly one blob (`x` tag, no `server`
/// tag — see the module docs for why).
pub(crate) fn sign_blob_get_auth_header(
    keys: &Keys,
    blob: &str,
    expiry_secs: u64,
) -> Result<String, String> {
    let now = Timestamp::now().as_secs();
    let tags = vec![
        Tag::parse(vec!["t", "get"]).map_err(|e| e.to_string())?,
        Tag::parse(vec!["expiration", &(now + expiry_secs).to_string()])
            .map_err(|e| e.to_string())?,
        Tag::parse(vec!["x", blob]).map_err(|e| e.to_string())?,
    ];
    let event = EventBuilder::new(Kind::from(24242), "Get buzz-media")
        .tags(tags)
        .sign_with_keys(keys)
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "Nostr {}",
        URL_SAFE_NO_PAD.encode(event.as_json().as_bytes())
    ))
}

/// Mint the Authorization header for a GET of `path_and_query` on the
/// transport relay at `transport_base`, or `None` (recovery mode, signing
/// failure, or an alias-road path naming no blob): callers then send none.
///
/// Safety contract (as `mint_media_get_auth`): attach the result only to a
/// request built from `transport_base` + `path_and_query`.
pub(crate) async fn mint_media_get_auth_for(
    state: &AppState,
    transport_base: &str,
    path_and_query: &str,
) -> Option<String> {
    let identity = media_identity_base(state, transport_base).await;
    match media_get_scope(transport_base, &identity, path_and_query)? {
        MediaGetScope::Server(base) => mint_media_get_auth(state, &base),
        MediaGetScope::Blob(blob) => {
            let keys = match state.signing_keys() {
                Ok(keys) => keys,
                Err(e) => {
                    eprintln!("buzz-desktop: media get auth unavailable (unsigned request): {e}");
                    return None;
                }
            };
            match sign_blob_get_auth_header(&keys, blob, MEDIA_GET_AUTH_EXPIRY_SECS) {
                Ok(header) => Some(header),
                Err(e) => {
                    eprintln!(
                        "buzz-desktop: media get auth signing failed (unsigned request): {e}"
                    );
                    None
                }
            }
        }
    }
}

/// [`mint_media_get_auth_for`] for an absolute URL already validated to be on
/// the transport relay: the scope is derived from the URL actually fetched.
pub(crate) async fn mint_media_get_auth_for_url(state: &AppState, url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let transport_base = parsed.origin().ascii_serialization();
    let path_and_query = match parsed.query() {
        Some(query) => format!("{}?{query}", parsed.path()),
        None => parsed.path().to_string(),
    };
    mint_media_get_auth_for(state, &transport_base, &path_and_query).await
}

/// Map a relay media URL onto the transport road the app is connected through.
///
/// A URL on the transport origin is returned unchanged; a URL on the
/// canonical origin is re-based onto the transport (same path and query),
/// because it names the same relay but may be unreachable by that name;
/// anything else is refused. Both forms must pass the full
/// `validate_download_url` gate (HTTPS, `/media/` path), and the returned URL
/// is always on the transport relay origin, so the SSRF boundary is unchanged.
pub(crate) fn map_relay_media_url(
    url: &str,
    transport_base: &str,
    canonical_base: &str,
) -> Result<String, String> {
    let transport_error = match validate_download_url(url, transport_base) {
        Ok(()) => return Ok(url.to_string()),
        Err(error) => error,
    };
    if !is_alias_road(transport_base, canonical_base) {
        return Err(transport_error);
    }
    validate_download_url(url, canonical_base).map_err(|_| transport_error)?;

    let parsed = url::Url::parse(url).map_err(|_| "invalid URL".to_string())?;
    let mut rebased =
        url::Url::parse(transport_base).map_err(|_| "invalid relay base URL".to_string())?;
    rebased.set_path(parsed.path());
    rebased.set_query(parsed.query());
    rebased.set_fragment(None);
    let rebased = rebased.to_string();
    validate_download_url(&rebased, transport_base)?;
    Ok(rebased)
}

/// Resolve a frontend-supplied media URL to the URL actually fetched: the
/// transport relay's own `/media/` URL (see [`map_relay_media_url`]). The
/// `/info` lookup runs only for URLs that are not already on the transport.
pub(crate) async fn resolve_relay_media_url(url: &str, state: &AppState) -> Result<String, String> {
    let transport_base = relay_api_base_url_with_override(state);
    if validate_download_url(url, &transport_base).is_ok() {
        return Ok(url.to_string());
    }
    let canonical_base = media_identity_base(state, &transport_base).await;
    map_relay_media_url(url, &transport_base, &canonical_base)
}

/// The URL and auth header for fetching an avatar: relay media (transport or
/// canonical origin) rides the transport with a scoped token; anything else
/// is fetched as given, unauthenticated.
pub(crate) async fn avatar_fetch(state: &AppState, url: &str) -> (String, Option<String>) {
    match resolve_relay_media_url(url, state).await {
        Ok(fetch_url) => {
            let auth = mint_media_get_auth_for_url(state, &fetch_url).await;
            (fetch_url, auth)
        }
        Err(_) => (url.to_string(), None),
    }
}

#[cfg(test)]
#[path = "origin_tests.rs"]
mod tests;
