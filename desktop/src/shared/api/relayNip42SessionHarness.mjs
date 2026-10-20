/**
 * Shared test infrastructure for the NIP-42 connection/session tests
 * (relayClientSession.test.mjs + readOnlyRelayClient.test.mjs).
 *
 * Exported from a non-test file so the `src/**\/*.test.mjs` glob never picks
 * this up as a test suite (same pattern as observedUnreadTestHarness.mjs).
 *
 * Two pieces:
 *
 * 1. `startInfoServer` — a REAL loopback node:http metadata fixture that
 *    serves 200 at exactly `/info` (404 elsewhere) and records every
 *    request target, so a transport path/query leaking into the metadata
 *    request fails loudly instead of silently succeeding.
 * 2. `installTauriFixture` — replaces `globalThis.window` with
 *    `__TAURI_INTERNALS__` driving the REAL @tauri-apps/api/core
 *    Channel + invoke against an in-memory WebSocket fixture. The
 *    `create_auth_event` handler reproduces the Rust command's observable
 *    contract (structural origin parse → real GET {origin}/info → strict
 *    fail-closed decision → kind-22242 event carrying the canonical relay
 *    tag and the ORIGINAL challenge). The Rust implementation of that
 *    contract is pinned by the buzz-desktop `nip42_canonical_auth_tests`;
 *    mirroring it here lets the session tests exercise the production
 *    client wiring (its own supplied URL + original challenge reach the
 *    signer; the signer's event becomes the AUTH frame; REQ stays gated)
 *    against realistic signer behavior, with synthetic keys only.
 */

import assert from "node:assert/strict";
import http from "node:http";

// ── loopback /info fixture ──────────────────────────────────────────────────

/**
 * Serve `body` (string or addr→string builder) with `status` for a request
 * target of exactly `/info`; 404 for anything else. Records `{ method, url }`
 * per request. A server answering every path with the same JSON would hide
 * request-target regressions; this one does not.
 */
export async function startInfoServer({ body, status = 200 } = {}) {
  const requests = [];
  const connections = new Set();
  const server = http.createServer((req, res) => {
    requests.push({ method: req.method, url: req.url });
    const atInfo = req.url === "/info";
    const payload = atInfo
      ? typeof body === "function"
        ? body(server.address?.address ?? "127.0.0.1")
        : body
      : "{}";
    const text =
      typeof payload === "string" ? payload : JSON.stringify(payload);
    res.statusCode = atInfo ? status : 404;
    res.setHeader("content-type", "application/json");
    res.end(text);
  });
  // fetch's undici pool holds keep-alive connections open for seconds after
  // a test finishes; server.close() alone would wait them out and hang the
  // test process. Destroy tracked connections, then close.
  server.on("connection", (socket) => {
    connections.add(socket);
    socket.on("close", () => connections.delete(socket));
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address();
  return {
    port,
    requests,
    close: () => {
      for (const socket of connections) socket.destroy();
      return new Promise((resolve) => server.close(resolve));
    },
  };
}

// ── create_auth_event contract mirror ───────────────────────────────────────

/**
 * Mirror of the desktop Rust `create_auth_event` contract (see
 * create_auth_event_impl + canonical_ws_signing_url in buzz-desktop):
 * structurally parse the supplied ws/wss URL, GET {http-origin}/info,
 * apply the same strict fail-closed advertisement rules, and return a
 * kind-22242 event carrying the canonical relay tag + ORIGINAL challenge.
 * Signatures are synthetic — the session under test never verifies them;
 * real signing is pinned Rust-side.
 */
export async function faithfulCreateAuthEvent({ challenge, relayUrl }) {
  const parsed = new URL(relayUrl);
  const httpScheme =
    parsed.protocol === "wss:"
      ? "https"
      : parsed.protocol === "ws:"
        ? "http"
        : null;
  if (!httpScheme) {
    throw new Error(`supplied relay URL is not ws/wss: ${relayUrl}`);
  }
  // `parsed.host` is host[:port] with IPv6 brackets and default ports
  // normalized away — the structural mirror of the Rust origin base.
  const base = `${httpScheme}://${parsed.host}`;

  let doc;
  try {
    const response = await fetch(`${base}/info`);
    if (!response.ok) {
      throw new Error(`relay /info HTTP ${response.status}`);
    }
    doc = await response.json();
  } catch (error) {
    if (error instanceof SyntaxError) {
      throw new Error("relay /info returned a malformed document");
    }
    throw error;
  }
  if (doc === null || typeof doc !== "object" || Array.isArray(doc)) {
    throw new Error("relay /info returned a malformed document");
  }

  let canonical = relayUrl;
  if (Object.hasOwn(doc, "push")) {
    const push = doc.push;
    if (push === null || typeof push !== "object" || Array.isArray(push)) {
      throw new Error("relay /info returned a malformed push descriptor");
    }
    if (Object.hasOwn(push, "origin")) {
      const origin = push.origin;
      if (typeof origin !== "string") {
        throw new Error("relay /info advertises a malformed canonical origin");
      }
      canonical = validateAdvertisedOrigin(origin);
    }
  }

  return syntheticAuthEvent(challenge, canonical);
}

/** Mirror of the Rust validate_advertised_origin strictness. */
function validateAdvertisedOrigin(advertised) {
  let url;
  try {
    url = new URL(advertised);
  } catch {
    throw new Error("relay /info canonical origin failed URL verification");
  }
  if (url.protocol !== "ws:" && url.protocol !== "wss:") {
    throw new Error("relay /info canonical origin failed URL verification");
  }
  if (url.username || url.password || url.search || url.hash) {
    throw new Error("relay /info canonical origin failed URL verification");
  }
  if (url.pathname !== "/" && url.pathname !== "") {
    throw new Error("relay /info canonical origin failed URL verification");
  }
  if (!url.host) {
    throw new Error("relay /info canonical origin failed URL verification");
  }
  // Protocol//host normalizes default ports exactly like the Rust
  // parse→HTTP-base→WS-form round trip.
  return `${url.protocol}//${url.host}`;
}

let syntheticEventCounter = 0;

/** A kind-22242 event with a unique id and synthetic signature. */
export function syntheticAuthEvent(challenge, relayTag) {
  syntheticEventCounter += 1;
  const counterHex = syntheticEventCounter.toString(16).padStart(4, "0");
  return {
    id: (counterHex + "0".repeat(60)).slice(0, 64),
    pubkey: "ab".repeat(32),
    created_at: Math.floor(Date.now() / 1000),
    kind: 22242,
    tags: [
      ["relay", relayTag],
      ["challenge", challenge],
    ],
    content: "",
    sig: "cd".repeat(32),
  };
}

/**
 * The bug-equivalent degraded signer for mutation controls: signs the
 * supplied transport URL verbatim WITHOUT consulting /info (the pre-fix
 * behavior). Tests assert the primary canonical-tag assertions would catch
 * exactly this shape.
 */
export async function transportSigningCreateAuthEvent({ challenge, relayUrl }) {
  return syntheticAuthEvent(challenge, relayUrl);
}

// ── Tauri window fixture ────────────────────────────────────────────────────

/**
 * Install a `window` whose `__TAURI_INTERNALS__` drives the real
 * @tauri-apps/api/core Channel/invoke against an in-memory WebSocket
 * fixture. `get_relay_ws_url` answers `relayWsUrl`. `create_auth_event`
 * delegates to `signer` (default: the faithful contract mirror above) and
 * records every `{ challenge, relayUrl }` input.
 *
 * Returns a handle with the sockets opened (`{ id, url, channel }`), the
 * frames sent (`{ socketId, payload }`), `deliver(socketId, payload)` to
 * inject server frames through the real Channel, `waitUntil`, the recorded
 * signer inputs, unhandled rejections swallowed while installed (the
 * read-only client voids its AUTH-challenge handler promise), and
 * `restore()` which uninstalls everything and clears tracked timers so
 * pending op-timeouts never hang the test process.
 */
export function installTauriFixture({
  relayWsUrl = "ws://127.0.0.1:3000",
  signer = faithfulCreateAuthEvent,
} = {}) {
  const sockets = [];
  const sent = [];
  const signerInputs = [];
  const rejections = [];
  // Mutable so a test can change the workspace relay between sessions —
  // exactly what a community switch does to `get_relay_ws_url`.
  let currentRelayWsUrl = relayWsUrl;

  const callbacks = new Map();
  let nextCallbackId = 1;
  let nextSocketId = 1;

  const outstandingTimers = new Set();
  const outstandingIntervals = new Set();
  let nextTimerHandle = 1;
  const realSetTimeout = globalThis.setTimeout.bind(globalThis);
  const realSetInterval = globalThis.setInterval.bind(globalThis);

  const internals = {
    transformCallback(callback) {
      const id = nextCallbackId++;
      callbacks.set(id, callback);
      return id;
    },
    unregisterCallback(id) {
      callbacks.delete(id);
    },
    async invoke(cmd, args) {
      switch (cmd) {
        case "plugin:websocket|connect": {
          const id = nextSocketId++;
          sockets.push({
            id,
            url: args.url,
            channel: args.onMessage,
            nextIndex: 0,
          });
          return id;
        }
        case "plugin:websocket|send": {
          sent.push({
            socketId: args.id,
            payload: JSON.parse(args.message.data),
          });
          return null;
        }
        case "plugin:websocket|disconnect":
        case "plugin:websocket|disconnect_all":
          return null;
        case "get_relay_ws_url":
          return currentRelayWsUrl;
        case "create_auth_event": {
          const input = { challenge: args.challenge, relayUrl: args.relayUrl };
          signerInputs.push(input);
          const event = await signer(input);
          return JSON.stringify(event);
        }
        default:
          throw new Error(`unexpected invoke in NIP-42 fixture: ${cmd}`);
      }
    },
  };

  const previousWindow = globalThis.window;
  globalThis.window = {
    __TAURI_INTERNALS__: internals,
    setTimeout(fn, ms, ...rest) {
      const handle = { id: nextTimerHandle++ };
      handle.native = realSetTimeout(() => {
        outstandingTimers.delete(handle);
        fn(...rest);
      }, ms);
      outstandingTimers.add(handle);
      return handle;
    },
    clearTimeout(handle) {
      if (!handle?.native) return;
      outstandingTimers.delete(handle);
      clearTimeout(handle.native);
    },
    setInterval(fn, ms, ...rest) {
      const handle = { id: nextTimerHandle++ };
      handle.native = realSetInterval(fn, ms, ...rest);
      outstandingIntervals.add(handle);
      return handle;
    },
    clearInterval(handle) {
      if (!handle?.native) return;
      outstandingIntervals.delete(handle);
      clearInterval(handle.native);
    },
  };

  const onUnhandledRejection = (reason) => {
    rejections.push(reason);
  };
  process.on("unhandledRejection", onUnhandledRejection);

  return {
    sockets,
    sent,
    signerInputs,
    rejections,
    setRelayWsUrl(url) {
      currentRelayWsUrl = url;
    },
    deliver(socketId, payload) {
      const socket = sockets.find((s) => s.id === socketId);
      assert.ok(socket, `fixture socket ${socketId} exists`);
      const callback = callbacks.get(socket.channel.id);
      assert.ok(callback, "socket channel callback registered");
      callback({
        index: socket.nextIndex++,
        message: { type: "Text", data: JSON.stringify(payload) },
      });
    },
    framesFrom(socketId, type) {
      return this.sent.filter(
        (f) => f.socketId === socketId && f.payload[0] === type,
      );
    },
    async waitUntil(predicate, { timeoutMs = 3000, what = "condition" } = {}) {
      const deadline = Date.now() + timeoutMs;
      for (;;) {
        const value = predicate();
        if (value) return value;
        if (Date.now() > deadline) {
          throw new Error(`timed out waiting for ${what}`);
        }
        await new Promise((resolve) => realSetTimeout(resolve, 5));
      }
    },
    restore() {
      process.off("unhandledRejection", onUnhandledRejection);
      for (const handle of outstandingTimers) clearTimeout(handle.native);
      for (const handle of outstandingIntervals) {
        clearInterval(handle.native);
      }
      // Restore the previous window when there was one; otherwise leave an
      // inert stub rather than undefined — async tails that are still in
      // flight (e.g. closeWebSocket after a disconnect) resolve their
      // invokes against it as no-ops instead of crashing on undefined.
      globalThis.window = previousWindow ?? {
        __TAURI_INTERNALS__: {
          invoke: async () => null,
          transformCallback: () => 0,
          unregisterCallback() {},
        },
        setTimeout: realSetTimeout,
        clearTimeout,
        setInterval: realSetInterval,
        clearInterval,
      };
    },
  };
}

/** Decode the relay/challenge tags of a signed AUTH event object. */
export function authEventTags(event) {
  const tags = Object.fromEntries(
    event.tags
      .filter((t) => t[0] === "relay" || t[0] === "challenge")
      .map((t) => [t[0], t[1]]),
  );
  assert.ok(tags.relay, "relay tag present");
  assert.ok(tags.challenge, "challenge tag present");
  return tags;
}
