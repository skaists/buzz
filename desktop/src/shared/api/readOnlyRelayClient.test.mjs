/**
 * NIP-42 connection/session regressions for the read-only community
 * observer (ReadOnlyRelayClient — readOnlyRelayClient.ts).
 *
 * The observer owns its own supplied connection URL (constructor argument,
 * never the active community's relay), so these tests drive the REAL
 * session against an in-memory WebSocket fixture plus a real loopback
 * /info server (see relayNip42SessionHarness): the socket must stay on the
 * supplied alias, the AUTH frame must carry the canonical relay tag and
 * the ORIGINAL challenge, metadata refusal must emit no AUTH frame, and
 * the observer's REQ must wait for this connection's matching successful
 * AUTH acknowledgement. The signer at the Tauri boundary mirrors the Rust
 * create_auth_event contract (real /info fetch, strict fail-closed
 * refusal); the Rust implementation is pinned by the buzz-desktop
 * nip42_canonical_auth_tests.
 */

import assert from "node:assert/strict";
import test from "node:test";

import { ReadOnlyRelayClient } from "./readOnlyRelayClient.ts";
import {
  authEventTags,
  installTauriFixture,
  startInfoServer,
  transportSigningCreateAuthEvent,
} from "./relayNip42SessionHarness.mjs";

const FILTER = { kinds: [41], limit: 10 };

test("observer stays on the alias, sends a canonical AUTH frame, and REQs only after the matching AUTH OK", async () => {
  const info = await startInfoServer({
    body: JSON.stringify({ push: { origin: "wss://canonical.example" } }),
  });
  const aliasUrl = `ws://127.0.0.1:${info.port}/?transport=one`;
  const fixture = installTauriFixture({ relayWsUrl: "ws://127.0.0.1:1" });
  const client = new ReadOnlyRelayClient(aliasUrl);
  try {
    const fetched = client.fetchEvents(FILTER);

    await fixture.waitUntil(() => fixture.sockets.length === 1, {
      what: "socket connect",
    });
    assert.equal(
      fixture.sockets[0].url,
      aliasUrl,
      "socket must connect to the supplied alias URL, unchanged",
    );

    fixture.deliver(1, ["AUTH", "challenge-original-789"]);

    await fixture.waitUntil(() => fixture.framesFrom(1, "AUTH").length === 1, {
      what: "AUTH frame",
    });
    assert.deepEqual(fixture.signerInputs, [
      { challenge: "challenge-original-789", relayUrl: aliasUrl },
    ]);
    assert.deepEqual(
      info.requests.map((r) => `${r.method} ${r.url}`),
      ["GET /info"],
    );

    const authEvent = fixture.framesFrom(1, "AUTH")[0].payload[1];
    const tags = authEventTags(authEvent);
    assert.equal(
      tags.relay,
      "wss://canonical.example",
      "AUTH frame relay tag names the canonical identity",
    );
    assert.equal(
      tags.challenge,
      "challenge-original-789",
      "AUTH frame preserves the original challenge",
    );

    // A successful OK for a different event id must not open the gate.
    fixture.deliver(1, ["OK", "ee".repeat(32), true, ""]);
    await new Promise((resolve) => setTimeout(resolve, 40));
    assert.equal(
      fixture.framesFrom(1, "REQ").length,
      0,
      "no REQ before this connection's matching AUTH acknowledgement",
    );

    fixture.deliver(1, ["OK", authEvent.id, true, ""]);
    await fixture.waitUntil(() => fixture.framesFrom(1, "REQ").length === 1, {
      what: "REQ after matching AUTH acknowledgement",
    });
    const subId = fixture.framesFrom(1, "REQ")[0].payload[1];

    fixture.deliver(1, ["EOSE", subId]);
    assert.deepEqual(await fetched, [], "history resolves after EOSE");
  } finally {
    client.disconnect();
    fixture.restore();
    await info.close();
  }
});

test("observer sends no AUTH and no REQ when /info metadata refuses", async () => {
  const info = await startInfoServer({ body: '{"push":' }); // malformed JSON
  const aliasUrl = `ws://127.0.0.1:${info.port}`;
  const fixture = installTauriFixture({ relayWsUrl: "ws://127.0.0.1:1" });
  const client = new ReadOnlyRelayClient(aliasUrl);
  try {
    const fetched = client.fetchEvents(FILTER);
    // Attach the rejection assertion immediately: the clean fail-closed
    // disconnect rejects this promise on the refusal path, and an unawaited
    // rejection would be flagged unhandled while the test observes frames.
    const fetchedOutcome = assert.rejects(fetched, /disconnect/i);

    await fixture.waitUntil(() => fixture.sockets.length === 1, {
      what: "socket connect",
    });
    fixture.deliver(1, ["AUTH", "challenge-refused"]);

    await fixture.waitUntil(() => fixture.signerInputs.length === 1, {
      what: "signer consulted",
    });
    // Give the refusal time to propagate.
    await new Promise((resolve) => setTimeout(resolve, 40));

    assert.equal(
      fixture.framesFrom(1, "AUTH").length,
      0,
      "metadata refusal must emit no AUTH frame",
    );
    assert.equal(
      fixture.framesFrom(1, "REQ").length,
      0,
      "metadata refusal must emit no REQ frame",
    );
    // The signer refusal tears the observer session down cleanly (the
    // Channel handler mirrors the main client's fail-closed catch): the
    // pending auth/fetch reject with the disconnect error and nothing is
    // left to surface as an unhandled rejection.
    await fetchedOutcome;
    assert.equal(
      fixture.rejections.length,
      0,
      "the refusal must not surface as an unhandled rejection",
    );
    assert.equal(
      fixture.framesFrom(1, "REQ").length,
      0,
      "still no REQ after the refusal settled",
    );
  } finally {
    client.disconnect();
    fixture.restore();
    await info.close();
  }
});

test("distinct observer communities sign their own supplied connection identities", async () => {
  const infoA = await startInfoServer({
    body: JSON.stringify({ push: { origin: "wss://canonical-a.example" } }),
  });
  const infoB = await startInfoServer({
    body: JSON.stringify({ push: { origin: "wss://canonical-b.example" } }),
  });
  // The active community's relay (never consulted by the observer): a
  // third origin whose advertisement matches neither observer.
  const infoActive = await startInfoServer({
    body: JSON.stringify({
      push: { origin: "wss://active-community.example" },
    }),
  });
  const fixture = installTauriFixture({
    relayWsUrl: `ws://127.0.0.1:${infoActive.port}`,
  });
  const clientA = new ReadOnlyRelayClient(`ws://127.0.0.1:${infoA.port}/road`);
  const clientB = new ReadOnlyRelayClient(`ws://127.0.0.1:${infoB.port}/road`);
  try {
    const fetchedA = clientA.fetchEvents(FILTER);
    const fetchedB = clientB.fetchEvents(FILTER);

    await fixture.waitUntil(() => fixture.sockets.length === 2, {
      what: "both observer sockets",
    });
    assert.equal(fixture.sockets[0].url, `ws://127.0.0.1:${infoA.port}/road`);
    assert.equal(fixture.sockets[1].url, `ws://127.0.0.1:${infoB.port}/road`);

    fixture.deliver(1, ["AUTH", "challenge-a"]);
    fixture.deliver(2, ["AUTH", "challenge-b"]);

    await fixture.waitUntil(() => fixture.signerInputs.length === 2, {
      what: "both signers consulted",
    });
    assert.deepEqual(fixture.signerInputs, [
      {
        challenge: "challenge-a",
        relayUrl: `ws://127.0.0.1:${infoA.port}/road`,
      },
      {
        challenge: "challenge-b",
        relayUrl: `ws://127.0.0.1:${infoB.port}/road`,
      },
    ]);

    await fixture.waitUntil(() => fixture.framesFrom(1, "AUTH").length === 1, {
      what: "observer A AUTH frame",
    });
    await fixture.waitUntil(() => fixture.framesFrom(2, "AUTH").length === 1, {
      what: "observer B AUTH frame",
    });
    // Request accounting AFTER both AUTH frames exist (each frame implies
    // its /info fetch completed).
    assert.equal(infoActive.requests.length, 0);
    assert.deepEqual(
      infoA.requests.map((r) => `${r.method} ${r.url}`),
      ["GET /info"],
    );
    assert.deepEqual(
      infoB.requests.map((r) => `${r.method} ${r.url}`),
      ["GET /info"],
    );
    assert.equal(
      authEventTags(fixture.framesFrom(1, "AUTH")[0].payload[1]).relay,
      "wss://canonical-a.example",
      "observer A signs its own community's advertised identity",
    );
    assert.equal(
      authEventTags(fixture.framesFrom(2, "AUTH")[0].payload[1]).relay,
      "wss://canonical-b.example",
      "observer B signs its own community's advertised identity",
    );

    // Complete both sessions.
    const eventA = fixture.framesFrom(1, "AUTH")[0].payload[1];
    const eventB = fixture.framesFrom(2, "AUTH")[0].payload[1];
    fixture.deliver(1, ["OK", eventA.id, true, ""]);
    fixture.deliver(2, ["OK", eventB.id, true, ""]);
    await fixture.waitUntil(
      () =>
        fixture.framesFrom(1, "REQ").length === 1 &&
        fixture.framesFrom(2, "REQ").length === 1,
      { what: "both REQs after matching acknowledgements" },
    );
    fixture.deliver(1, ["EOSE", fixture.framesFrom(1, "REQ")[0].payload[1]]);
    fixture.deliver(2, ["EOSE", fixture.framesFrom(2, "REQ")[0].payload[1]]);
    assert.deepEqual(await fetchedA, []);
    assert.deepEqual(await fetchedB, []);
  } finally {
    clientA.disconnect();
    clientB.disconnect();
    fixture.restore();
    await infoA.close();
    await infoB.close();
    await infoActive.close();
  }
});

test("mutation control: a transport-signing observer fails the canonical-tag assertions above", async () => {
  const info = await startInfoServer({
    body: JSON.stringify({ push: { origin: "wss://canonical.example" } }),
  });
  const aliasUrl = `ws://127.0.0.1:${info.port}`;
  // Degraded signer = the bug shape (sign the transport verbatim, skip
  // /info). Proves the happy-path tag assertions discriminate exactly the
  // regression this incident fixed.
  const fixture = installTauriFixture({
    relayWsUrl: "ws://127.0.0.1:1",
    signer: transportSigningCreateAuthEvent,
  });
  const client = new ReadOnlyRelayClient(aliasUrl);
  try {
    const fetched = client.fetchEvents(FILTER);
    await fixture.waitUntil(() => fixture.sockets.length === 1, {
      what: "socket connect",
    });
    fixture.deliver(1, ["AUTH", "challenge-control"]);

    await fixture.waitUntil(() => fixture.framesFrom(1, "AUTH").length === 1, {
      what: "AUTH frame",
    });
    const tags = authEventTags(fixture.framesFrom(1, "AUTH")[0].payload[1]);
    assert.equal(
      tags.relay,
      aliasUrl,
      "degraded signer observable: the tag names the transport",
    );
    assert.notEqual(
      tags.relay,
      "wss://canonical.example",
      "the happy-path assertion (tag === canonical) would fail here",
    );
    assert.equal(
      info.requests.length,
      0,
      "degraded signer never consults /info — the discrimination point",
    );

    const event = fixture.framesFrom(1, "AUTH")[0].payload[1];
    fixture.deliver(1, ["OK", event.id, true, ""]);
    await fixture.waitUntil(() => fixture.framesFrom(1, "REQ").length === 1, {
      what: "REQ after matching acknowledgement",
    });
    fixture.deliver(1, ["EOSE", fixture.framesFrom(1, "REQ")[0].payload[1]]);
    assert.deepEqual(await fetched, []);
  } finally {
    client.disconnect();
    fixture.restore();
    await info.close();
  }
});
