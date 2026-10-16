import assert from "node:assert/strict";
import { test } from "node:test";

import {
  beginRelayOriginFetch,
  getCachedRelayOrigin,
  mediaProxyUrl,
  resetMediaCaches,
  subscribeRelayOrigin,
  withDeadline,
} from "./mediaUrl.ts";

const HASH = "a".repeat(64);

function deferred() {
  let resolve;
  const promise = new Promise((resolvePromise) => {
    resolve = resolvePromise;
  });
  return { promise, resolve };
}

test("mediaProxyUrl: uses the IPv4 loopback literal for the localhost proxy", () => {
  assert.equal(
    mediaProxyUrl(54321, `${HASH}.png`),
    `http://127.0.0.1:54321/media/${HASH}.png`,
  );
});

test("media-proxy port store: resolved port publishes and reset notifies subscribers", async () => {
  const previousWindow = globalThis.window;
  let notifications = 0;

  globalThis.window = {
    __TAURI_INTERNALS__: {
      invoke(command) {
        if (command === "get_media_proxy_port") return Promise.resolve(54321);
        if (command === "get_relay_http_url") {
          return Promise.resolve("https://relay.example");
        }
        return Promise.reject(new Error(`Unexpected command: ${command}`));
      },
    },
  };

  try {
    const mediaUrl = await import(`./mediaUrl.ts?portStore=${Date.now()}`);
    const unsubscribe = mediaUrl.subscribeMediaProxyPort(() => notifications++);

    mediaUrl.rewriteRelayUrl(`https://relay.example/media/${HASH}.png`);
    await new Promise((resolve) => setTimeout(resolve, 0));

    assert.equal(mediaUrl.getCachedMediaProxyPort(), 54321);
    assert.equal(notifications, 1);

    mediaUrl.resetMediaCaches();
    assert.equal(mediaUrl.getCachedMediaProxyPort(), null);
    assert.equal(notifications, 2);

    unsubscribe();
  } finally {
    globalThis.window = previousWindow;
  }
});

test("relay-origin store: publishes are canonicalized at the store boundary", () => {
  // The store must hold the invariant that `cachedRelayOrigin` is always a
  // canonical URL origin — consumers compare `new URL(src).origin` against it
  // (isRelayDownloadable), which is safe only by this construction, not by
  // call-site convention.
  resetMediaCaches();
  let notifications = 0;
  const unsubscribe = subscribeRelayOrigin(() => notifications++);

  // Uppercase host + trailing slash + explicit default port → canonical form.
  beginRelayOriginFetch()("https://RELAY.Example:443/");
  assert.equal(getCachedRelayOrigin(), "https://relay.example");
  assert.equal(notifications, 1);

  // A differently-shaped publish of the same canonical origin is a no-op for
  // both the snapshot and the listeners.
  beginRelayOriginFetch()("https://relay.example");
  assert.equal(getCachedRelayOrigin(), "https://relay.example");
  assert.equal(notifications, 1);

  // Invalid non-null input fails closed to null (does not retain the previous
  // origin, does not publish an arbitrary string) and notifies the change.
  beginRelayOriginFetch()("not a url");
  assert.equal(getCachedRelayOrigin(), null);
  assert.equal(notifications, 2);

  // Stale-generation publishes are still rejected before canonicalization.
  const stale = beginRelayOriginFetch();
  resetMediaCaches();
  stale("https://STALE.example/");
  assert.equal(getCachedRelayOrigin(), null);

  unsubscribe();
  resetMediaCaches();
});

test("relay-origin store: a resolved origin publishes and notifies subscribers", () => {
  resetMediaCaches();
  let notifications = 0;
  const unsubscribe = subscribeRelayOrigin(() => notifications++);

  const publish = beginRelayOriginFetch();
  publish("https://relay.example");

  assert.equal(getCachedRelayOrigin(), "https://relay.example");
  assert.equal(notifications, 1);

  unsubscribe();
  resetMediaCaches();
});

test("relay-origin store: unsubscribe removes exactly its own listener", () => {
  resetMediaCaches();
  let kept = 0;
  let dropped = 0;
  const unsubscribeKept = subscribeRelayOrigin(() => kept++);
  const unsubscribeDropped = subscribeRelayOrigin(() => dropped++);

  // Dropping one listener must not affect the other.
  unsubscribeDropped();
  beginRelayOriginFetch()("https://relay.example");
  assert.equal(kept, 1);
  assert.equal(dropped, 0);

  unsubscribeKept();
  resetMediaCaches();
});

test("relay-origin store: reset notifies only on an actual snapshot change", () => {
  resetMediaCaches();
  let notifications = 0;
  const unsubscribe = subscribeRelayOrigin(() => notifications++);

  // Origin already null → reset is a no-op for listeners.
  resetMediaCaches();
  assert.equal(notifications, 0);

  // Now resolve, then reset: the reset clears a non-null origin, so it fires.
  beginRelayOriginFetch()("https://relay.example");
  assert.equal(notifications, 1);
  resetMediaCaches();
  assert.equal(getCachedRelayOrigin(), null);
  assert.equal(notifications, 2);

  unsubscribe();
});

test("relay-origin store: a late fetch from the previous community never regresses the snapshot", () => {
  resetMediaCaches();
  const unsubscribe = subscribeRelayOrigin(() => {});

  // Community A starts a fetch, then the user switches workspaces (reset),
  // then community B starts its own fetch.
  const publishA = beginRelayOriginFetch();
  resetMediaCaches();
  const publishB = beginRelayOriginFetch();

  // A resolves late — its generation is stale, so it must be discarded.
  publishA("https://relay-a.example");
  assert.equal(getCachedRelayOrigin(), null);

  // B resolves — it is current, so it wins.
  publishB("https://relay-b.example");
  assert.equal(getCachedRelayOrigin(), "https://relay-b.example");

  // A late duplicate from A after B must still not clobber B.
  publishA("https://relay-a.example");
  assert.equal(getCachedRelayOrigin(), "https://relay-b.example");

  unsubscribe();
  resetMediaCaches();
});

test("relay-origin store: a failed attempt then a later success publishes exactly once", () => {
  // Mirrors the `fetchProxyPort` retry loop: each poll attempt calls
  // `beginRelayOriginFetch()` and only publishes if the invoke resolves. An
  // early attempt whose invoke rejects (Tauri bridge not ready) never calls its
  // publisher, so nothing is published; a later attempt succeeds and publishes.
  resetMediaCaches();
  let notifications = 0;
  const unsubscribe = subscribeRelayOrigin(() => notifications++);

  // Attempt 1: invoke rejects — publisher is never invoked.
  beginRelayOriginFetch();
  assert.equal(getCachedRelayOrigin(), null);
  assert.equal(notifications, 0);

  // Attempt 2: invoke resolves — publishes once, notifies once.
  beginRelayOriginFetch()("https://relay.example");
  assert.equal(getCachedRelayOrigin(), "https://relay.example");
  assert.equal(notifications, 1);

  unsubscribe();
  resetMediaCaches();
});

test("relay-origin store: a reset between a failed attempt and its late success discards the stale result", () => {
  // A workspace switch (reset) during the retry sequence must invalidate a
  // still-in-flight attempt from the previous community, even if that attempt
  // eventually resolves after the switch.
  resetMediaCaches();
  const unsubscribe = subscribeRelayOrigin(() => {});

  // Attempt from community A begins, then the user switches (reset), then a
  // fresh attempt from community B begins and succeeds.
  const publishA = beginRelayOriginFetch();
  resetMediaCaches();
  beginRelayOriginFetch()("https://relay-b.example");
  assert.equal(getCachedRelayOrigin(), "https://relay-b.example");

  // A's invoke finally resolves late — stale generation, so it is dropped.
  publishA("https://relay-a.example");
  assert.equal(getCachedRelayOrigin(), "https://relay-b.example");

  unsubscribe();
  resetMediaCaches();
});

test("withDeadline: a never-settling invoke resolves to null at the deadline", async () => {
  // The poll loop bounds each invoke by the remaining budget so a wedged IPC
  // bridge (a promise that never settles) can't hang startup past the timeout.
  const neverSettles = new Promise(() => {});
  const result = await withDeadline(neverSettles, Date.now() + 20);
  assert.equal(result, null);
});

test("withDeadline: a value that settles before the deadline is returned", async () => {
  const result = await withDeadline(
    Promise.resolve("http://relay.example"),
    Date.now() + 1000,
  );
  assert.equal(result, "http://relay.example");
});

test("withDeadline: an already-passed deadline resolves to null without awaiting", async () => {
  const result = await withDeadline(new Promise(() => {}), Date.now() - 1);
  assert.equal(result, null);
});

test("withDeadline: a rejection before the deadline propagates to the caller", async () => {
  await assert.rejects(
    withDeadline(Promise.reject(new Error("ipc failed")), Date.now() + 1000),
    /ipc failed/,
  );
});

test("withDeadline: a rejection after the deadline is observed, not unhandled", async () => {
  // The timeout wins first (deadline ~10ms), then the invoke rejects ~30ms
  // later. That late rejection must be swallowed by the internal no-op catch,
  // not surface as an unhandled rejection. Register a listener to prove it.
  let unhandled;
  const onUnhandled = (reason) => {
    unhandled = reason;
  };
  process.on("unhandledRejection", onUnhandled);
  try {
    const slowReject = new Promise((_resolve, reject) =>
      setTimeout(() => reject(new Error("late ipc failure")), 30),
    );
    const result = await withDeadline(slowReject, Date.now() + 10);
    assert.equal(result, null);
    // Give the late rejection time to fire and be (not) reported.
    await new Promise((r) => setTimeout(r, 40));
    assert.equal(unhandled, undefined);
  } finally {
    process.off("unhandledRejection", onUnhandled);
  }
});

test("resetMediaCaches: ignores relay origin lookups from the previous generation", async () => {
  const previousWindow = globalThis.window;
  const staleOrigin = deferred();
  let relayOriginCalls = 0;

  globalThis.window = {
    __TAURI_INTERNALS__: {
      invoke(command) {
        if (command === "get_media_proxy_port") return Promise.resolve(54321);
        if (command === "get_relay_http_url") {
          relayOriginCalls += 1;
          return relayOriginCalls === 1
            ? staleOrigin.promise
            : Promise.resolve("https://active.example");
        }
        return Promise.reject(new Error(`Unexpected command: ${command}`));
      },
    },
  };

  try {
    // A unique URL triggers module-load fetching with the stale relay lookup
    // still unresolved, matching a cold launch before applyCommunity finishes.
    const mediaUrl = await import(`./mediaUrl.ts?race=${Date.now()}`);
    await new Promise((resolve) => setTimeout(resolve, 0));

    mediaUrl.resetMediaCaches();
    const activeUrl = `https://active.example/media/${HASH}.png`;
    mediaUrl.rewriteRelayUrl(activeUrl);
    await new Promise((resolve) => setTimeout(resolve, 0));

    // Complete the old lookup after reset. It must not overwrite the active
    // community origin fetched by the new generation.
    staleOrigin.resolve("https://stale.example");
    await new Promise((resolve) => setTimeout(resolve, 0));

    assert.equal(
      mediaUrl.rewriteRelayUrl(activeUrl),
      `http://127.0.0.1:54321/media/${HASH}.png`,
    );
  } finally {
    globalThis.window = previousWindow;
  }
});

test("rewriteRelayUrl: matches relay origin case-insensitively (uppercase saved community URL)", async () => {
  const previousWindow = globalThis.window;

  globalThis.window = {
    __TAURI_INTERNALS__: {
      invoke(command) {
        if (command === "get_media_proxy_port") return Promise.resolve(54321);
        if (command === "get_relay_http_url") {
          // Saved community URLs keep the user's casing; the relay always
          // emits lowercased media URLs (normalize_host in buzz-core).
          return Promise.resolve("https://PENDING-SEED.communities.buzz.xyz");
        }
        return Promise.reject(new Error(`Unexpected command: ${command}`));
      },
    },
  };

  try {
    const mediaUrl = await import(`./mediaUrl.ts?case=${Date.now()}`);
    await new Promise((resolve) => setTimeout(resolve, 0));

    const relayMediaUrl = `https://pending-seed.communities.buzz.xyz/media/${HASH}.png`;
    assert.equal(
      mediaUrl.rewriteRelayUrl(relayMediaUrl),
      `http://127.0.0.1:54321/media/${HASH}.png`,
    );
  } finally {
    globalThis.window = previousWindow;
  }
});

test("rewriteRelayUrl: still passes external Blossom URLs through unchanged", async () => {
  const previousWindow = globalThis.window;

  globalThis.window = {
    __TAURI_INTERNALS__: {
      invoke(command) {
        if (command === "get_media_proxy_port") return Promise.resolve(54321);
        if (command === "get_relay_http_url") {
          return Promise.resolve("https://relay.example");
        }
        return Promise.reject(new Error(`Unexpected command: ${command}`));
      },
    },
  };

  try {
    const mediaUrl = await import(`./mediaUrl.ts?external=${Date.now()}`);
    await new Promise((resolve) => setTimeout(resolve, 0));

    const externalUrl = `https://nostr.build/media/${HASH}.png`;
    assert.equal(mediaUrl.rewriteRelayUrl(externalUrl), externalUrl);
  } finally {
    globalThis.window = previousWindow;
  }
});

async function waitFor(predicate, timeoutMs = 3000) {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() > deadline) return false;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  return true;
}

const MEMO_KEY = "buzz:relay-canonical-origin:https://relay.alias.example";

function memoryStorage(initial = {}) {
  const items = new Map(Object.entries(initial));
  return {
    items,
    getItem: (key) => (items.has(key) ? items.get(key) : null),
    setItem: (key, value) => items.set(key, String(value)),
    removeItem: (key) => items.delete(key),
  };
}

function aliasBridge({ canonical, storage }) {
  const calls = { canonical: 0 };
  const bridge = {
    __TAURI_INTERNALS__: {
      invoke(command) {
        if (command === "get_media_proxy_port") return Promise.resolve(54321);
        if (command === "get_relay_http_url") {
          return Promise.resolve("https://relay.alias.example");
        }
        if (command === "get_relay_canonical_http_url") {
          calls.canonical += 1;
          return canonical(calls.canonical);
        }
        return Promise.reject(new Error(`Unexpected command: ${command}`));
      },
    },
  };
  if (storage) bridge.localStorage = storage;
  return { bridge, calls };
}

/**
 * Simulate useCommunityInit (caches reset once the community relay is
 * installed) and a mounted canonical-origin subscriber (useRelayCanonicalOrigin).
 */
async function applyCommunity(mediaUrl) {
  mediaUrl.resetMediaCaches();
  mediaUrl.ensureRelayOriginFetch();
  await waitFor(
    () =>
      mediaUrl.getCachedMediaProxyPort() !== null &&
      mediaUrl.getCachedRelayOrigin() !== null,
  );
  mediaUrl.ensureCanonicalOriginFetch();
}

test("rewriteRelayUrl: alias road proxies media stored under the relay's canonical origin", async () => {
  // The workspace connects through an alias road while the relay advertises
  // its canonical community origin in /info and stamps every stored media URL
  // with it. Those URLs are the relay's own media: a direct WebView fetch is
  // unauthenticated (401) and may be filtered by name on the user's network.
  const previousWindow = globalThis.window;
  const storage = memoryStorage();
  const { bridge } = aliasBridge({
    canonical: () =>
      Promise.resolve({
        transport: "https://relay.alias.example",
        canonical: "https://canonical.example",
      }),
    storage,
  });
  globalThis.window = bridge;

  try {
    const mediaUrl = await import(`./mediaUrl.ts?canonicalAlias=${Date.now()}`);
    await applyCommunity(mediaUrl);
    assert.ok(
      await waitFor(
        () =>
          mediaUrl.getCachedRelayCanonicalOrigin() ===
          "https://canonical.example",
      ),
      "canonical origin must resolve once the relay origin is known",
    );

    assert.equal(
      mediaUrl.rewriteRelayUrl(`https://canonical.example/media/${HASH}.png`),
      `http://127.0.0.1:54321/media/${HASH}.png`,
      "canonical-origin media must be proxied",
    );
    assert.equal(
      mediaUrl.rewriteRelayUrl(`https://relay.alias.example/media/${HASH}.jpg`),
      `http://127.0.0.1:54321/media/${HASH}.jpg`,
      "transport-origin media must still be proxied",
    );
    assert.equal(
      mediaUrl.rewriteRelayUrl(`https://other.example/media/${HASH}.png`),
      `https://other.example/media/${HASH}.png`,
      "external Blossom URLs must pass through unchanged",
    );
    assert.equal(storage.getItem(MEMO_KEY), "https://canonical.example");
    mediaUrl.resetMediaCaches();
  } finally {
    globalThis.window = previousWindow;
  }
});

test("canonical lookup: a slow /info does not delay the proxy port or relay origin", async () => {
  const previousWindow = globalThis.window;
  const { bridge } = aliasBridge({
    canonical: () =>
      new Promise((resolve) =>
        setTimeout(
          () =>
            resolve({
              transport: "https://relay.alias.example",
              canonical: "https://canonical.example",
            }),
          1500,
        ),
      ),
  });
  globalThis.window = bridge;

  try {
    const mediaUrl = await import(`./mediaUrl.ts?canonicalSlow=${Date.now()}`);
    mediaUrl.resetMediaCaches();
    mediaUrl.ensureRelayOriginFetch();
    assert.ok(
      await waitFor(
        () =>
          mediaUrl.getCachedMediaProxyPort() === 54321 &&
          mediaUrl.getCachedRelayOrigin() === "https://relay.alias.example",
        1000,
      ),
      "port and origin must resolve while the canonical lookup is in flight",
    );
    assert.equal(mediaUrl.getCachedRelayCanonicalOrigin(), null);
    // Nothing remembered for this relay yet and the lookup can still land:
    // relay-shaped media stays on the proxy (safe default).
    assert.equal(
      mediaUrl.rewriteRelayUrl(`https://canonical.example/media/${HASH}.png`),
      `http://127.0.0.1:54321/media/${HASH}.png`,
    );
    mediaUrl.resetMediaCaches();
  } finally {
    globalThis.window = previousWindow;
  }
});

test("canonical lookup: the remembered canonical serves decisions before the lookup lands", async () => {
  const previousWindow = globalThis.window;
  const { bridge } = aliasBridge({
    canonical: () =>
      new Promise((resolve) =>
        setTimeout(
          () =>
            resolve({
              transport: "https://relay.alias.example",
              canonical: "https://canonical.example",
            }),
          1500,
        ),
      ),
    storage: memoryStorage({ [MEMO_KEY]: "https://canonical.example" }),
  });
  globalThis.window = bridge;

  try {
    const mediaUrl = await import(`./mediaUrl.ts?canonicalMemo=${Date.now()}`);
    await applyCommunity(mediaUrl);
    assert.equal(
      mediaUrl.getCachedRelayCanonicalOrigin(),
      "https://canonical.example",
    );
    assert.equal(
      mediaUrl.rewriteRelayUrl(`https://canonical.example/media/${HASH}.png`),
      `http://127.0.0.1:54321/media/${HASH}.png`,
    );
    assert.equal(
      mediaUrl.rewriteRelayUrl(`https://other.example/media/${HASH}.png`),
      `https://other.example/media/${HASH}.png`,
    );
    mediaUrl.resetMediaCaches();
  } finally {
    globalThis.window = previousWindow;
  }
});

test("canonical lookup: a backend error is retried instead of cached", async () => {
  const previousWindow = globalThis.window;
  const { bridge, calls } = aliasBridge({
    canonical: (n) =>
      n === 1
        ? Promise.reject(new Error("relay /info timed out"))
        : Promise.resolve({
            transport: "https://relay.alias.example",
            canonical: "https://canonical.example",
          }),
  });
  globalThis.window = bridge;

  try {
    const mediaUrl = await import(`./mediaUrl.ts?canonicalRetry=${Date.now()}`);
    mediaUrl.resetMediaCaches();
    mediaUrl.ensureCanonicalOriginFetch();
    assert.ok(
      await waitFor(
        () =>
          mediaUrl.getCachedRelayCanonicalOrigin() ===
          "https://canonical.example",
      ),
      "a later attempt must publish the canonical origin",
    );
    assert.ok(
      calls.canonical >= 2,
      `expected a retry, saw ${calls.canonical} call(s)`,
    );
    mediaUrl.resetMediaCaches();
  } finally {
    globalThis.window = previousWindow;
  }
});

test("canonical lookup: never contacts the cold-launch default relay (generation 0)", async () => {
  const previousWindow = globalThis.window;
  const { bridge, calls } = aliasBridge({
    canonical: () =>
      Promise.resolve({
        transport: "https://relay.alias.example",
        canonical: "https://canonical.example",
      }),
  });
  globalThis.window = bridge;

  try {
    const mediaUrl = await import(`./mediaUrl.ts?canonicalGen0=${Date.now()}`);
    mediaUrl.ensureRelayOriginFetch();
    mediaUrl.ensureCanonicalOriginFetch();
    await waitFor(() => mediaUrl.getCachedRelayOrigin() !== null);
    mediaUrl.rewriteRelayUrl(`https://canonical.example/media/${HASH}.png`);
    await new Promise((resolve) => setTimeout(resolve, 50));
    assert.equal(calls.canonical, 0);
  } finally {
    globalThis.window = previousWindow;
  }
});

test("resetMediaCaches: clears the canonical origin and notifies subscribers", async () => {
  const previousWindow = globalThis.window;
  const { bridge } = aliasBridge({
    canonical: () =>
      Promise.resolve({
        transport: "https://relay.alias.example",
        canonical: "https://canonical.example",
      }),
  });
  globalThis.window = bridge;

  try {
    const mediaUrl = await import(`./mediaUrl.ts?canonicalReset=${Date.now()}`);
    let notifications = 0;
    const unsubscribe = mediaUrl.subscribeRelayOrigin(() => notifications++);
    mediaUrl.resetMediaCaches();
    mediaUrl.ensureCanonicalOriginFetch();
    await waitFor(() => mediaUrl.getCachedRelayCanonicalOrigin() !== null);
    assert.equal(
      mediaUrl.getCachedRelayCanonicalOrigin(),
      "https://canonical.example",
    );
    const afterResolve = notifications;
    assert.ok(afterResolve >= 1, "resolution notifies subscribers");

    mediaUrl.resetMediaCaches();
    assert.equal(mediaUrl.getCachedRelayCanonicalOrigin(), null);
    assert.ok(notifications > afterResolve, "reset notifies subscribers");
    unsubscribe();
  } finally {
    globalThis.window = previousWindow;
  }
});

test("canonical lookup: an answer for another relay is neither published nor remembered", async () => {
  const previousWindow = globalThis.window;
  const storage = memoryStorage();
  const { bridge } = aliasBridge({
    canonical: () =>
      Promise.resolve({
        transport: "https://other.example",
        canonical: "https://canonical.example",
      }),
    storage,
  });
  globalThis.window = bridge;

  try {
    const mediaUrl = await import(
      `./mediaUrl.ts?canonicalMismatch=${Date.now()}`
    );
    await applyCommunity(mediaUrl);
    await new Promise((resolve) => setTimeout(resolve, 100));
    assert.equal(mediaUrl.getCachedRelayCanonicalOrigin(), null);
    assert.equal(storage.getItem(MEMO_KEY), null);
    mediaUrl.resetMediaCaches();
  } finally {
    globalThis.window = previousWindow;
  }
});

test("canonical road: external Blossom URLs pass through once the lookup lands (generation > 0)", async () => {
  // Production always runs at generation >= 1 (useCommunityInit resets the
  // caches after applyCommunity). Until the first lookup lands, foreign
  // relay-shaped media rides the proxy (see isRelayOwnedMediaUrl); once the
  // relay answers that it is its own identity, foreign origins are external.
  const previousWindow = globalThis.window;
  globalThis.window = {
    __TAURI_INTERNALS__: {
      invoke(command) {
        if (command === "get_media_proxy_port") return Promise.resolve(54321);
        if (command === "get_relay_http_url") {
          return Promise.resolve("https://relay.example");
        }
        if (command === "get_relay_canonical_http_url") {
          return Promise.resolve({
            transport: "https://relay.example",
            canonical: "https://relay.example",
          });
        }
        return Promise.reject(new Error(`Unexpected command: ${command}`));
      },
    },
  };

  try {
    const mediaUrl = await import(`./mediaUrl.ts?canonicalRoad=${Date.now()}`);
    await applyCommunity(mediaUrl);
    assert.ok(
      await waitFor(
        () =>
          mediaUrl.getCachedRelayCanonicalOrigin() === "https://relay.example",
      ),
    );
    const externalUrl = `https://nostr.build/media/${HASH}.png`;
    assert.equal(mediaUrl.rewriteRelayUrl(externalUrl), externalUrl);
    assert.equal(
      mediaUrl.rewriteRelayUrl(`https://relay.example/media/${HASH}.png`),
      `http://127.0.0.1:54321/media/${HASH}.png`,
    );
    mediaUrl.resetMediaCaches();
  } finally {
    globalThis.window = previousWindow;
  }
});

test("canonical lookup: an answer that lands before the relay origin is remembered once it publishes", async () => {
  const previousWindow = globalThis.window;
  const storage = memoryStorage();
  let releaseOrigin;
  const originGate = new Promise((resolve) => {
    releaseOrigin = resolve;
  });
  globalThis.window = {
    localStorage: storage,
    __TAURI_INTERNALS__: {
      invoke(command) {
        if (command === "get_media_proxy_port") return Promise.resolve(54321);
        if (command === "get_relay_http_url") {
          return originGate.then(() => "https://relay.alias.example");
        }
        if (command === "get_relay_canonical_http_url") {
          return Promise.resolve({
            transport: "https://relay.alias.example",
            canonical: "https://canonical.example",
          });
        }
        return Promise.reject(new Error(`Unexpected command: ${command}`));
      },
    },
  };

  try {
    const mediaUrl = await import(`./mediaUrl.ts?canonicalEarly=${Date.now()}`);
    mediaUrl.resetMediaCaches();
    mediaUrl.ensureCanonicalOriginFetch();
    mediaUrl.ensureRelayOriginFetch();
    await new Promise((resolve) => setTimeout(resolve, 50));
    assert.equal(mediaUrl.getCachedRelayOrigin(), null);
    assert.equal(storage.getItem(MEMO_KEY), null);

    releaseOrigin();
    assert.ok(
      await waitFor(() => mediaUrl.getCachedRelayOrigin() !== null),
      "relay origin must publish",
    );
    assert.equal(storage.getItem(MEMO_KEY), "https://canonical.example");
    assert.equal(
      mediaUrl.getCachedRelayCanonicalOrigin(),
      "https://canonical.example",
    );
    mediaUrl.resetMediaCaches();
  } finally {
    globalThis.window = previousWindow;
  }
});
