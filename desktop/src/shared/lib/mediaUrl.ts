/**
 * Rewrite relay media URLs to use the localhost streaming proxy.
 *
 * WKWebView's networking stack bypasses the VPN tunnel, so direct <img src> requests
 * to the relay get 403'd by Cloudflare Access. The localhost proxy routes
 * fetches through the Rust backend (via reqwest), which goes through the VPN.
 *
 * For video, the proxy streams via axum — no buffering the entire file.
 * Images and other media also benefit from this path.
 *
 * Only URLs hosted on the Buzz relay are rewritten. External Blossom URLs
 * (e.g. nostr.build, void.cat) are returned unchanged — they aren't behind
 * Cloudflare Access and can be loaded directly by WKWebView. Without this
 * origin check, external Blossom URLs would be proxied to the wrong server
 * (the Buzz relay), resulting in 404s.
 */

import { invoke } from "@tauri-apps/api/core";

// Matches: https://anything.com/media/{64-hex}.{ext}
// Also matches thumbnails: /media/{64-hex}.thumb.jpg
const RELAY_MEDIA_RE =
  /^(?:https?:\/\/[^/]+)\/media\/([\da-f]{64}(?:\.thumb)?\.(?:jpg|png|gif|webp|mp4|webm|mov)(?:\?.*)?)$/;

/** Cached proxy port — fetched once from the Tauri backend. */
let cachedPort: number | null = null;
let portPromise: Promise<number | null> | null = null;

/**
 * Cached relay origin (e.g. "https://buzz-oss.stage.blox.sqprod.co"),
 * canonicalized via {@link canonicalOrigin} so comparisons are stable.
 */
let cachedRelayOrigin: string | null = null;

/**
 * The relay's canonical origin: the community identity it advertises in
 * GET /info (`push.origin`, e.g. "https://example.buzz"), canonicalized like
 * {@link cachedRelayOrigin}. On an alias road (the app connects through
 * `relay.example.dev` while the relay's identity is `example.buzz`) the relay
 * stamps every stored media URL with this origin, so those URLs are the
 * relay's own media and must ride the authenticated proxy. Equals the relay
 * origin when the relay advertises nothing; `null` until resolved.
 */
let cachedRelayCanonicalOrigin: string | null = null;

/** The relay origin the backend resolved {@link cachedRelayCanonicalOrigin} for. */
let cachedCanonicalTransport: string | null = null;

/** In-memory copy of the remembered canonical for one relay origin. */
let canonicalMemoCache: { relay: string; canonical: string | null } | null =
  null;

/** Set once the canonical lookup gave up for the current community. */
let canonicalLookupExhausted = false;

/** The in-flight canonical lookup for the current community, if any. */
let canonicalPromise: Promise<void> | null = null;

const CANONICAL_MEMO_PREFIX = "buzz:relay-canonical-origin:";

function canonicalMemoStorage(): Storage | null {
  try {
    return typeof window !== "undefined" ? (window.localStorage ?? null) : null;
  } catch {
    return null;
  }
}

/**
 * The canonical origin last resolved for the current relay origin, so that
 * decisions made before this session's lookup lands are already right on a
 * known alias road. Unknown relays have none (unknown origins stay external).
 */
function rememberedCanonicalOrigin(): string | null {
  if (!cachedRelayOrigin) return null;
  if (canonicalMemoCache?.relay === cachedRelayOrigin) {
    return canonicalMemoCache.canonical;
  }
  let canonical: string | null = null;
  try {
    canonical =
      canonicalMemoStorage()?.getItem(
        CANONICAL_MEMO_PREFIX + cachedRelayOrigin,
      ) ?? null;
  } catch {
    canonical = null;
  }
  canonicalMemoCache = { relay: cachedRelayOrigin, canonical };
  return canonical;
}

function rememberCanonicalOrigin(relayOrigin: string, canonical: string): void {
  canonicalMemoCache = { relay: relayOrigin, canonical };
  try {
    canonicalMemoStorage()?.setItem(
      CANONICAL_MEMO_PREFIX + relayOrigin,
      canonical,
    );
  } catch {
    // Storage unavailable — the in-memory value still serves this session.
  }
}

/**
 * Canonicalize a URL to its origin with a lowercased scheme/host.
 *
 * The relay always emits media URLs with a lowercased tenant host
 * (`normalize_host` in buzz-core), but the saved community relay URL keeps
 * whatever casing the user typed (DNS is case-insensitive, so an uppercase
 * host connects fine). A raw string comparison between the two misclassifies
 * the relay's own media URLs as external and skips the authenticated proxy.
 * `new URL().origin` lowercases scheme + host and drops default ports.
 *
 * Returns null for unparseable input.
 */
function canonicalOrigin(url: string): string | null {
  try {
    return new URL(url).origin;
  } catch {
    return null;
  }
}

/**
 * Monotonic cache generation, bumped on every `resetMediaCaches` (i.e.
 * workspace switch). Async lookups capture the current generation and may
 * only publish results while it is still current, so a lookup started for the
 * previous community can never repopulate caches (port or origin) after reset.
 */
let cacheGeneration = 0;

/** `useSyncExternalStore` listeners for relay-origin changes. */
const relayOriginListeners = new Set<() => void>();

/** `useSyncExternalStore` listeners for media-proxy port changes. */
const mediaProxyPortListeners = new Set<() => void>();

function notifyRelayOriginListeners(): void {
  for (const listener of relayOriginListeners) listener();
}

function notifyMediaProxyPortListeners(): void {
  for (const listener of mediaProxyPortListeners) listener();
}

/**
 * Publish a resolved relay origin, but only if `generation` is still current
 * (the fetch wasn't superseded by a workspace switch). The stored snapshot is
 * always canonicalized (see `canonicalOrigin`) regardless of what shape the
 * publisher was handed, so consumers comparing `URL.origin === relayOrigin`
 * (e.g. `isRelayDownloadable`) hold by construction, not by call-site
 * convention. Notifies subscribers only on an actual snapshot change so
 * `useSyncExternalStore` doesn't churn.
 */
function setRelayOrigin(origin: string | null, generation: number): void {
  if (generation !== cacheGeneration) return;
  const canonical = origin === null ? null : canonicalOrigin(origin);
  if (cachedRelayOrigin === canonical) return;
  cachedRelayOrigin = canonical;
  // A canonical answer that landed before the relay origin is settled now:
  // remembered when it was resolved for this relay, otherwise discarded so
  // the lookup can run again for the relay actually published.
  if (canonical !== null && cachedRelayCanonicalOrigin !== null) {
    if (cachedCanonicalTransport === canonical) {
      rememberCanonicalOrigin(canonical, cachedRelayCanonicalOrigin);
    } else {
      cachedRelayCanonicalOrigin = null;
      cachedCanonicalTransport = null;
      canonicalPromise = null;
    }
  }
  notifyRelayOriginListeners();
}

/**
 * Begin a relay-origin fetch: captures the current generation and returns a
 * publisher bound to it. The publisher applies the resolved origin only if no
 * workspace switch (`resetMediaCaches`) has happened in the meantime, so a
 * fetch started for community A can never publish A's origin after a switch to
 * community B. Callers invoke the returned function once the origin resolves.
 */
export function beginRelayOriginFetch(): (origin: string | null) => void {
  const generation = cacheGeneration;
  return (origin) => setRelayOrigin(origin, generation);
}

/**
 * Subscribe to relay-origin changes. Returns a stable unsubscribe function
 * (the same closure identity for the life of the subscription), as
 * `useSyncExternalStore` requires.
 */
export function subscribeRelayOrigin(listener: () => void): () => void {
  relayOriginListeners.add(listener);
  return () => {
    relayOriginListeners.delete(listener);
  };
}

/**
 * Subscribe to media-proxy port changes. Returns a stable unsubscribe function
 * for `useSyncExternalStore` consumers that need to re-run `rewriteRelayUrl`
 * once the async Tauri proxy lookup has resolved.
 */
export function subscribeMediaProxyPort(listener: () => void): () => void {
  mediaProxyPortListeners.add(listener);
  return () => {
    mediaProxyPortListeners.delete(listener);
  };
}

const POLL_INTERVAL_MS = 100;
const POLL_TIMEOUT_MS = 5000;

const CANONICAL_RETRY_BASE_MS = 250;
const CANONICAL_RETRY_MAX_MS = 8000;
const CANONICAL_BUDGET_MS = 60_000;
const CANONICAL_ATTEMPT_TIMEOUT_MS = 12_000;

/**
 * Race `promise` against the remaining time until `deadline`, resolving to
 * `null` if the deadline passes first. Bounds each poll invoke so a Tauri IPC
 * call that never settles (bridge wedged, not merely unavailable) cannot hang
 * the poll loop past its budget. The underlying invoke isn't cancellable, but
 * abandoning its result is safe here: a late origin resolution is
 * generation-guarded, and a late port is simply ignored once the loop returns.
 *
 * Exported for unit tests (the never-settling case) — it is a self-contained,
 * generally-useful timeout primitive, not a seam into `fetchProxyPort`.
 */
export function withDeadline<T>(
  promise: Promise<T>,
  deadline: number,
): Promise<T | null> {
  const remaining = deadline - Date.now();
  // A late rejection (the invoke rejects after the timeout already won the
  // race) would otherwise surface as an unhandled rejection, so attach a
  // no-op catch that keeps it observed regardless of which side wins.
  promise.catch(() => {});
  if (remaining <= 0) return Promise.resolve(null);
  return new Promise<T | null>((resolve, reject) => {
    // Clear the timer once the promise settles so a settled race doesn't leave
    // a dangling timeout for the rest of the poll budget.
    const timer = setTimeout(() => resolve(null), remaining);
    promise.then(
      (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      (error) => {
        clearTimeout(timer);
        reject(error);
      },
    );
  });
}

/**
 * Poll `get_media_proxy_port` until we get a non-zero port or timeout.
 * Also resolves the relay HTTP base URL for origin-checking.
 * Returns the port, or null if the proxy never came up.
 */
async function fetchProxyPort(): Promise<number | null> {
  // Resolve the relay origin alongside the port, retried inside the same poll
  // loop. Both invokes can reject early (e.g. Tauri IPC not ready at module
  // load); the port already retries, and the origin must too — a single
  // fire-and-forget attempt that fails before the bridge is up would leave the
  // origin unresolved forever, hiding relay Download eligibility. The publisher
  // captures the generation at each attempt so a resolution that lands after a
  // workspace switch is discarded rather than publishing a stale origin. The
  // loop itself also captures the generation so a still-running poll from the
  // previous community can neither keep spinning nor cache its stale port
  // after a reset.
  const generation = cacheGeneration;
  const deadline = Date.now() + POLL_TIMEOUT_MS;
  while (Date.now() < deadline && generation === cacheGeneration) {
    if (!cachedRelayOrigin) {
      const publishRelayOrigin = beginRelayOriginFetch();
      try {
        const url = await withDeadline(
          invoke<string>("get_relay_http_url"),
          deadline,
        );
        if (url !== null) publishRelayOrigin(canonicalOrigin(url));
      } catch {
        // invoke failed (e.g. Tauri IPC not ready yet) — keep retrying
      }
    }

    if (!cachedPort) {
      try {
        const port = await withDeadline(
          invoke<number>("get_media_proxy_port"),
          deadline,
        );
        if (port !== null && port > 0 && generation === cacheGeneration) {
          if (cachedPort !== port) {
            cachedPort = port;
            notifyMediaProxyPortListeners();
          }
        }
      } catch {
        // invoke failed (e.g. Tauri IPC not ready yet) — keep retrying
      }
    }

    // Both readiness results complete independently. Keep polling until BOTH
    // land: a resolved port lets URL rewriting proceed, but relay-origin
    // resolution gates Download eligibility, so we must not stop retrying the
    // origin just because the port is ready (nothing else re-enters this loop
    // once the port is cached). Each invoke is bounded by the remaining
    // deadline (`withDeadline`) so a never-settling IPC call can't hang the
    // loop; every late origin result is generation-guarded.
    if (cachedPort && cachedRelayOrigin) return cachedPort;

    await new Promise((r) => setTimeout(r, POLL_INTERVAL_MS));
  }
  return cachedPort;
}

/**
 * Resolve the canonical origin in its own task, independent of the proxy port
 * and relay origin (local IPC with no network dependency). The backend errors
 * while the relay's /info is unreadable, so this retries with backoff inside
 * a bounded budget; a workspace switch abandons it. On success subscribers
 * are notified so canonical-aware consumers re-render.
 */
async function fetchCanonicalOrigin(): Promise<void> {
  const generation = cacheGeneration;
  const deadline = Date.now() + CANONICAL_BUDGET_MS;
  let delay = CANONICAL_RETRY_BASE_MS;
  while (generation === cacheGeneration && Date.now() < deadline) {
    try {
      const answer = await withDeadline(
        invoke<{ transport: string; canonical: string }>(
          "get_relay_canonical_http_url",
        ),
        Math.min(deadline, Date.now() + CANONICAL_ATTEMPT_TIMEOUT_MS),
      );
      const transport = answer ? canonicalOrigin(answer.transport) : null;
      const canonical = answer ? canonicalOrigin(answer.canonical) : null;
      if (generation !== cacheGeneration) return;
      // An answer for another relay (a lookup racing a workspace switch) is
      // neither published nor remembered; the next attempt or reset settles it.
      if (
        transport !== null &&
        canonical !== null &&
        (cachedRelayOrigin === null || transport === cachedRelayOrigin)
      ) {
        if (cachedRelayOrigin) rememberCanonicalOrigin(transport, canonical);
        if (
          cachedRelayCanonicalOrigin !== canonical ||
          cachedCanonicalTransport !== transport
        ) {
          cachedRelayCanonicalOrigin = canonical;
          cachedCanonicalTransport = transport;
          notifyRelayOriginListeners();
        }
        return;
      }
    } catch {
      // Lookup failed or IPC not ready — retry after backoff.
    }
    await new Promise((r) => setTimeout(r, delay));
    delay = Math.min(delay * 2, CANONICAL_RETRY_MAX_MS);
  }
  if (generation === cacheGeneration) canonicalLookupExhausted = true;
}

/**
 * Start the canonical-origin lookup for the current community if none is in
 * flight. Safe to call from render paths and effects.
 */
export function ensureCanonicalOriginFetch(): void {
  // Generation 0 is the cold-launch default relay, before the active
  // community's relay override is installed (useCommunityInit resets the
  // caches after applyCommunity): never contact it for /info.
  if (cacheGeneration === 0) return;
  if (!canonicalPromise && typeof window !== "undefined") {
    canonicalPromise = fetchCanonicalOrigin();
  }
}

/**
 * Ensure the relay-origin lookup is active after a community cache reset.
 *
 * Consumers that classify relay-owned URLs may render before any media URL
 * asks for the proxy port. Starting the shared fetch here keeps host
 * classification independent from incidental image/video rendering.
 */
export function ensureRelayOriginFetch(): void {
  if (!portPromise && typeof window !== "undefined") {
    portPromise = fetchProxyPort();
  }
}

/** Eagerly fetch the port at module load so it's ready by first render. */
// The try/catch inside fetchProxyPort handles non-Tauri environments gracefully
// (invoke will throw, we retry until timeout, then give up — no side effects).
if (typeof window !== "undefined") {
  portPromise = fetchProxyPort();
}

/**
 * Reset module-level caches so the next render re-fetches the proxy port
 * and relay origin for the new community.
 *
 * Bumps the origin generation so any in-flight fetch from the previous
 * community is discarded on resolution, and notifies subscribers only if the
 * origin actually changes (a reset from an already-null origin is a no-op for
 * listeners).
 */
export function resetMediaCaches(): void {
  cacheGeneration += 1;
  const hadCachedPort = cachedPort !== null;
  cachedPort = null;
  portPromise = null;
  if (hadCachedPort) {
    notifyMediaProxyPortListeners();
  }
  const hadCanonical = cachedRelayCanonicalOrigin !== null;
  cachedRelayCanonicalOrigin = null;
  cachedCanonicalTransport = null;
  canonicalMemoCache = null;
  canonicalLookupExhausted = false;
  canonicalPromise = null;
  if (cachedRelayOrigin !== null || hadCanonical) {
    cachedRelayOrigin = null;
    notifyRelayOriginListeners();
  }
}

/**
 * The relay origin (e.g. `https://buzz-oss.stage.blox.sqprod.co`) if it has
 * been resolved, else `null`. Synchronous best-effort read of the same cache
 * `rewriteRelayUrl` uses. Callers that need a hard SSRF guarantee must still
 * rely on the Rust `validate_download_url` gate; this only drives UX (e.g.
 * whether to offer a Download action that could otherwise only error).
 */
export function getCachedRelayOrigin(): string | null {
  return cachedRelayOrigin;
}

/**
 * The relay's canonical origin (see {@link cachedRelayCanonicalOrigin}): this
 * session's resolved value, else the one last resolved for this relay origin,
 * else `null`. Subscribe via `subscribeRelayOrigin`.
 */
export function getCachedRelayCanonicalOrigin(): string | null {
  return resolvedCanonicalOrigin() ?? rememberedCanonicalOrigin();
}

/** This session's canonical answer, when it belongs to the current relay. */
function resolvedCanonicalOrigin(): string | null {
  if (cachedRelayCanonicalOrigin === null) return null;
  if (
    cachedRelayOrigin !== null &&
    cachedCanonicalTransport !== cachedRelayOrigin
  ) {
    return null;
  }
  return cachedRelayCanonicalOrigin;
}

/**
 * Whether a relay-shaped media URL is our relay's own media: on the relay
 * origin, or on its canonical origin (alias roads). An unresolved relay
 * origin keeps the existing safe default ("ours"). While this session's
 * canonical lookup is pending, the canonical last resolved for this relay is
 * used; with none remembered, relay-shaped media stays on the proxy until the
 * lookup lands or gives up, after which unknown origins are external.
 */
function isRelayOwnedMediaUrl(url: string): boolean {
  if (!cachedRelayOrigin) return true;
  const urlOrigin = canonicalOrigin(url);
  if (urlOrigin === null) return false;
  if (urlOrigin === cachedRelayOrigin) return true;
  const resolved = resolvedCanonicalOrigin();
  if (resolved !== null) return urlOrigin === resolved;
  if (!canonicalLookupExhausted) ensureCanonicalOriginFetch();
  const remembered = rememberedCanonicalOrigin();
  if (remembered !== null) return urlOrigin === remembered;
  // No answer yet for this relay: while a lookup can still land, keep
  // relay-shaped media on the proxy — the same safe default as an unresolved
  // relay origin. Generation 0 (cold-launch default relay) never looks up.
  return cacheGeneration > 0 && !canonicalLookupExhausted;
}

/**
 * The localhost proxy port if it has been resolved, else `null`. Synchronous
 * best-effort read of the same cache `rewriteRelayUrl` uses.
 */
export function getCachedMediaProxyPort(): number | null {
  return cachedPort;
}

/**
 * Build the local proxy URL with an IPv4 literal. The Rust proxy binds
 * `127.0.0.1:0`, not `::1`, and some WebViews resolve `localhost` to IPv6
 * first. Matching the bind address avoids machine-dependent image failures.
 */
export function mediaProxyUrl(port: number, mediaPath: string): string {
  return `http://127.0.0.1:${port}/media/${mediaPath}`;
}

/**
 * If `url` is a Blossom media URL hosted on the Buzz relay, rewrite it
 * to go through the local streaming proxy. External Blossom URLs and
 * non-Blossom URLs are returned unchanged.
 *
 * Falls back to buzz-media:// if the proxy port isn't available yet.
 */
export function rewriteRelayUrl(url: string): string {
  const m = RELAY_MEDIA_RE.exec(url);
  if (!m) return url;

  // Only proxy URLs that belong to our relay. External Blossom URLs
  // (different origin) pass through unchanged — they work fine via WKWebView.
  // If the relay origin isn't cached yet, fall through to the rewrite path
  // as a safe default (relay URLs need the proxy to avoid Cloudflare 403s).
  // Compare canonicalized origins: hosts are case-insensitive, and the relay
  // always returns lowercased media URLs even when the saved community URL
  // was typed with uppercase (e.g. wss://PENDING-SEED.communities.buzz.xyz).
  if (!isRelayOwnedMediaUrl(url)) {
    return url;
  }

  if (cachedPort && cachedPort > 0) {
    return mediaProxyUrl(cachedPort, m[1]);
  }

  if (!portPromise && typeof window !== "undefined") {
    ensureRelayOriginFetch();
  }

  return `buzz-media://localhost/media/${m[1]}`;
}
