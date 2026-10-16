use std::io::{Read as _, Write as _};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use nostr::{Event, EventBuilder, JsonUtil, Keys, Kind, Tag, Timestamp};

use super::*;

const HASH: &str = "abababababababababababababababababababababababababababababababab";
const OTHER_HASH: &str = "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";
const ALIAS_BASE: &str = "https://relay.alias.example";
const CANONICAL_BASE: &str = "https://canonical.example";

/// Serve `status_line` + `body` to every connection (std listener + thread,
/// the same race-free pattern as the relay loopback tests) and count them.
fn serve_info(status_line: &'static str, body: &'static str) -> (String, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            counter.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let response = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    let transport = format!("http://{addr}");
    forget(&transport);
    (transport, hits)
}

/// Drop any cached decision for `transport` so a recycled ephemeral port can
/// never inherit another test's result.
fn forget(transport: &str) {
    if let Ok(mut cache) = CANONICAL_SIGNING_BASES.write() {
        cache.remove(transport);
    }
    if let Ok(mut failures) = FAILED_LOOKUPS.write() {
        failures.remove(transport);
    }
}

fn decode_event(header: &str) -> Event {
    let b64 = header.strip_prefix("Nostr ").expect("Nostr scheme");
    let json = URL_SAFE_NO_PAD.decode(b64).expect("base64url");
    let event = Event::from_json(String::from_utf8(json).unwrap()).expect("event json");
    event.verify().expect("valid signature");
    event
}

fn tag_values(event: &Event, name: &str) -> Vec<String> {
    event
        .tags
        .iter()
        .filter_map(|tag| {
            let v = tag.as_slice();
            (v.first().map(String::as_str) == Some(name)).then(|| v[1].clone())
        })
        .collect()
}

// ── canonical base resolution ────────────────────────────────────────────────

#[tokio::test]
async fn identity_base_uses_the_advertised_origin_on_an_alias_road() {
    let (transport, _) = serve_info(
        "200 OK",
        r#"{"name":"Buzz Relay","push":{"origin":"wss://canonical.example"}}"#,
    );
    let client = reqwest::Client::new();
    assert_eq!(
        media_identity_base_with_client(&client, &transport).await,
        CANONICAL_BASE
    );
    assert_eq!(
        resolve_canonical_base(&client, &transport).await.unwrap(),
        CANONICAL_BASE
    );
    forget(&transport);
}

#[tokio::test]
async fn identity_base_keeps_the_transport_when_nothing_is_advertised() {
    let (transport, _) = serve_info("200 OK", r#"{"name":"Buzz Relay"}"#);
    let client = reqwest::Client::new();
    assert_eq!(
        media_identity_base_with_client(&client, &transport).await,
        transport
    );
    forget(&transport);
}

#[tokio::test]
async fn unreadable_info_falls_back_once_and_is_remembered() {
    // A failed lookup must not break media on canonical roads (fallback =
    // the transport itself) and must not cost a lookup per media request.
    let (transport, hits) = serve_info("500 Internal Server Error", r#"{"error":"boom"}"#);
    let client = reqwest::Client::new();
    let with_slash = format!("{transport}/");
    assert_eq!(
        media_identity_base_with_client(&client, &with_slash).await,
        transport
    );
    assert_eq!(
        media_identity_base_with_client(&client, &transport).await,
        transport
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "failure must be negatively cached"
    );
    // The strict resolver used by the frontend command reports the failure.
    assert!(resolve_canonical_base(&client, &transport).await.is_err());
    forget(&transport);
}

#[tokio::test]
async fn concurrent_cold_lookups_make_one_request() {
    let (transport, hits) =
        serve_info("200 OK", r#"{"push":{"origin":"wss://canonical.example"}}"#);
    let client = reqwest::Client::new();
    let lookups = (0..8).map(|_| media_identity_base_with_client(&client, &transport));
    let results = futures_util::future::join_all(lookups).await;
    assert!(results.iter().all(|base| base == CANONICAL_BASE));
    assert_eq!(hits.load(Ordering::SeqCst), 1, "single-flight");
    forget(&transport);
}

// ── origin comparison ────────────────────────────────────────────────────────

#[test]
fn origins_ignore_casing_paths_and_trailing_slashes() {
    assert_eq!(origin_of("https://Relay.Alias.Example/"), ALIAS_BASE);
    assert_eq!(origin_of("https://relay.alias.example/api"), ALIAS_BASE);
    assert_eq!(origin_of("https://relay.alias.example:443"), ALIAS_BASE);
    assert!(!is_alias_road("https://RELAY.alias.example/", ALIAS_BASE));
    assert!(is_alias_road(ALIAS_BASE, CANONICAL_BASE));
}

// ── token scope ──────────────────────────────────────────────────────────────

#[test]
fn media_hash_from_path_reads_blob_paths_only() {
    assert_eq!(
        media_hash_from_path(&format!("/media/{HASH}.jpg")),
        Some(HASH)
    );
    assert_eq!(
        media_hash_from_path(&format!("/media/{HASH}.thumb.jpg?w=200")),
        Some(HASH)
    );
    assert_eq!(media_hash_from_path("/media/upload"), None);
    assert_eq!(media_hash_from_path("/media/abc.jpg"), None);
    assert_eq!(media_hash_from_path(&format!("/other/{HASH}.jpg")), None);
}

#[test]
fn canonical_road_keeps_the_server_scoped_token() {
    assert_eq!(
        media_get_scope(
            ALIAS_BASE,
            "https://RELAY.alias.example/",
            &format!("/media/{HASH}.png")
        ),
        Some(MediaGetScope::Server(ALIAS_BASE.to_string()))
    );
}

#[test]
fn alias_road_scopes_the_token_to_one_blob() {
    assert_eq!(
        media_get_scope(
            ALIAS_BASE,
            CANONICAL_BASE,
            &format!("/media/{HASH}.thumb.jpg?w=1")
        ),
        Some(MediaGetScope::Blob(HASH))
    );
    // A path that names no blob gets no alias-road token at all.
    assert_eq!(
        media_get_scope(ALIAS_BASE, CANONICAL_BASE, "/media/upload"),
        None
    );
}

#[test]
fn blob_token_names_the_blob_and_no_server() {
    let keys = Keys::generate();
    let event = decode_event(&sign_blob_get_auth_header(&keys, HASH, 600).unwrap());
    assert_eq!(event.kind, Kind::from(24242));
    assert_eq!(tag_values(&event, "t"), vec!["get".to_string()]);
    assert_eq!(tag_values(&event, "x"), vec![HASH.to_string()]);
    assert!(tag_values(&event, "server").is_empty());
    assert_eq!(tag_values(&event, "expiration").len(), 1);
}

#[test]
fn relay_verifier_accepts_the_blob_token_for_that_blob_only() {
    // The relay's own verifier, with the canonical host as the server domain
    // (what the relay sees behind the alias road's Host rewrite).
    let keys = Keys::generate();
    let event = decode_event(&sign_blob_get_auth_header(&keys, HASH, 600).unwrap());
    let verify = |sha: &str| {
        buzz_media_pkg::auth::verify_blossom_get_auth(&event, sha, Some("canonical.example"), 3600)
    };
    assert!(verify(HASH).is_ok(), "the requested blob must be readable");
    assert!(verify(OTHER_HASH).is_err(), "no other blob may be readable");
}

#[test]
fn relay_verifier_accepts_any_blob_when_a_server_tag_matches() {
    // Why alias-road tokens carry no `server` tag: the verifier accepts `x`
    // OR `server`, so a `server` tag naming a host is a bearer for every blob
    // on it — even alongside an `x` pin.
    let keys = Keys::generate();
    let now = Timestamp::now().as_secs();
    let event = EventBuilder::new(Kind::from(24242), "Get buzz-media")
        .tags(vec![
            Tag::parse(["t", "get"]).unwrap(),
            Tag::parse(["expiration", &(now + 600).to_string()]).unwrap(),
            Tag::parse(["server", "canonical.example"]).unwrap(),
            Tag::parse(["x", HASH]).unwrap(),
        ])
        .sign_with_keys(&keys)
        .unwrap();
    assert!(buzz_media_pkg::auth::verify_blossom_get_auth(
        &event,
        OTHER_HASH,
        Some("canonical.example"),
        3600
    )
    .is_ok());
}

// ── map_relay_media_url: canonical-origin media rides the transport ──────────

#[test]
fn map_keeps_transport_urls_unchanged() {
    let url = format!("{ALIAS_BASE}/media/{HASH}.jpg");
    assert_eq!(
        map_relay_media_url(&url, ALIAS_BASE, CANONICAL_BASE).unwrap(),
        url
    );
}

#[test]
fn map_moves_canonical_media_onto_the_transport() {
    assert_eq!(
        map_relay_media_url(
            &format!("{CANONICAL_BASE}/media/{HASH}.thumb.jpg?w=200"),
            ALIAS_BASE,
            CANONICAL_BASE
        )
        .unwrap(),
        format!("{ALIAS_BASE}/media/{HASH}.thumb.jpg?w=200")
    );
}

#[test]
fn map_handles_ports_slashes_and_fragments() {
    assert_eq!(
        map_relay_media_url(
            &format!("{CANONICAL_BASE}/media/{HASH}.png#frag"),
            "https://relay.alias.example:8443/",
            &format!("{CANONICAL_BASE}/")
        )
        .unwrap(),
        format!("https://relay.alias.example:8443/media/{HASH}.png")
    );
}

#[test]
fn map_rejects_foreign_origins_and_traversal() {
    let err = map_relay_media_url(
        "https://evil.example.com/media/abc123.jpg",
        ALIAS_BASE,
        CANONICAL_BASE,
    )
    .unwrap_err();
    assert!(err.contains("relay origin"), "{err}");
    // `..` is normalised by the URL parser before the /media/ check.
    assert!(map_relay_media_url(
        "https://canonical.example/media/../etc/passwd",
        ALIAS_BASE,
        CANONICAL_BASE
    )
    .is_err());
}

#[test]
fn map_applies_the_full_gate_to_canonical_urls() {
    assert!(map_relay_media_url(
        "https://canonical.example/other/abc123.jpg",
        ALIAS_BASE,
        CANONICAL_BASE
    )
    .is_err());
    assert!(map_relay_media_url(
        "http://canonical.example/media/abc123.jpg",
        ALIAS_BASE,
        CANONICAL_BASE
    )
    .is_err());
}

#[test]
fn map_without_an_alias_is_the_plain_gate() {
    let base = "https://relay.example.com";
    assert_eq!(
        map_relay_media_url("https://relay.example.com/media/abc123.png", base, base).unwrap(),
        "https://relay.example.com/media/abc123.png"
    );
    assert!(map_relay_media_url("https://canonical.example/media/abc123.png", base, base).is_err());
    // Casing alone never makes a road an alias road.
    assert!(map_relay_media_url(
        "https://canonical.example/media/abc123.png",
        base,
        "https://RELAY.example.com/"
    )
    .is_err());
}
