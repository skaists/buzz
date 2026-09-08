/**
 * NIP-42 connection/session regressions for the MAIN community client
 * (RelayClient — relayClientSession.ts).
 *
 * These drive the REAL production session against an in-memory WebSocket
 * fixture plus a real loopback /info server (see relayNip42SessionHarness):
 * the socket must stay on the supplied alias URL, the signer must receive
 * exactly that URL plus the ORIGINAL challenge, the resulting AUTH frame
 * must carry the canonical relay tag, and REQ frames must stay gated until
 * THIS connection's matching successful AUTH acknowledgement. The signer
 * used at the Tauri boundary mirrors the Rust create_auth_event contract
 * (real /info fetch, strict fail-closed refusal) — the Rust implementation
 * itself is pinned by the buzz-desktop nip42_canonical_auth_tests.
 */

import assert from "node:assert/strict";
import test from "node:test";

import { RelayClient } from "./relayClientSession.ts";
import {
  authEventTags,
  installTauriFixture,
  startInfoServer,
  transportSigningCreateAuthEvent,
} from "./relayNip42SessionHarness.mjs";

const FILTER = { kinds: [40002], limit: 50 };

test("main client stays on the alias and sends a canonical-tag AUTH frame before any REQ", async () => {
  const info = await startInfoServer({
    body: JSON.stringify({ push: { origin: "wss://canonical.example" } }),
  });
  // The alias road carries transport components (query) exactly like the
  // incident's supplied connection URL.
  const aliasUrl = `ws://127.0.0.1:${info.port}/?transport=one`;
  const fixture = installTauriFixture({ relayWsUrl: aliasUrl });
  const client = new RelayClient();
  try {
    const subscribed = client.subscribeLive(FILTER, () => {});

    await fixture.waitUntil(() => fixture.sockets.length === 1, {
      what: "socket connect",
    });
    assert.equal(
      fixture.sockets[0].url,
      aliasUrl,
      "socket must connect to the supplied alias URL, unchanged",
    );
    assert.equal(
      fixture.signerInputs.length,
      0,
      "no signer call before a challenge",
    );

    fixture.deliver(1, ["AUTH", "challenge-original-456"]);

    await fixture.waitUntil(() => fixture.framesFrom(1, "AUTH").length === 1, {
      what: "AUTH frame",
    });
    // The signer received THIS connection's URL and the original challenge.
    assert.deepEqual(fixture.signerInputs, [
      { challenge: "challenge-original-456", relayUrl: aliasUrl },
    ]);
    // Metadata was fetched from the origin root, not the transport road.
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
      "challenge-original-456",
      "AUTH frame preserves the original challenge",
    );

    // A successful OK for a DIFFERENT event id must not open the gate.
    fixture.deliver(1, ["OK", "ee".repeat(32), true, ""]);
    await new Promise((resolve) => setTimeout(resolve, 40));
    assert.equal(
      fixture.framesFrom(1, "REQ").length,
      0,
      "no REQ before this connection's matching AUTH acknowledgement",
    );

    // The matching acknowledgement opens the gate.
    fixture.deliver(1, ["OK", authEvent.id, true, ""]);
    await fixture.waitUntil(() => fixture.framesFrom(1, "REQ").length === 1, {
      what: "REQ after matching AUTH acknowledgement",
    });

    await subscribed;
  } finally {
    client.disconnect();
    fixture.restore();
    await info.close();
  }
});

test("main client sends no AUTH and no REQ when /info metadata refuses", async () => {
  const info = await startInfoServer({ body: '{"push":' }); // malformed JSON
  const aliasUrl = `ws://127.0.0.1:${info.port}`;
  const fixture = installTauriFixture({ relayWsUrl: aliasUrl });
  const client = new RelayClient();
  try {
    const subscribed = client.subscribeLive(FILTER, () => {});
    const outcome = assert.rejects(subscribed, /malformed/);

    await fixture.waitUntil(() => fixture.sockets.length === 1, {
      what: "socket connect",
    });
    fixture.deliver(1, ["AUTH", "challenge-refused"]);

    await fixture.waitUntil(() => fixture.signerInputs.length === 1, {
      what: "signer consulted",
    });
    // Give the refusal time to propagate through the session error path.
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
    await outcome;
  } finally {
    client.disconnect();
    fixture.restore();
    await info.close();
  }
});

test("distinct main-community sessions sign their own supplied connection identities", async () => {
  const infoA = await startInfoServer({
    body: JSON.stringify({ push: { origin: "wss://canonical-a.example" } }),
  });
  const infoB = await startInfoServer({
    body: JSON.stringify({ push: { origin: "wss://canonical-b.example" } }),
  });
  // Workspace URL changes between sessions exactly like a community switch:
  // the second client must latch the new URL, never the first one's.
  const fixture = installTauriFixture({
    relayWsUrl: `ws://127.0.0.1:${infoA.port}`,
  });
  const clientA = new RelayClient();
  const clientB = new RelayClient();
  try {
    const subscribedA = clientA.subscribeLive(FILTER, () => {});
    await fixture.waitUntil(() => fixture.sockets.length === 1, {
      what: "client A socket",
    });
    fixture.deliver(1, ["AUTH", "challenge-a"]);

    // The workspace URL changes between sessions exactly like a community
    // switch: client B must latch the new URL, never client A's.
    fixture.setRelayWsUrl(`ws://127.0.0.1:${infoB.port}`);
    const subscribedB = clientB.subscribeLive(FILTER, () => {});
    await fixture.waitUntil(() => fixture.sockets.length === 2, {
      what: "client B socket",
    });
    assert.equal(
      fixture.sockets[1].url,
      `ws://127.0.0.1:${infoB.port}`,
      "client B connects with the new workspace URL",
    );
    fixture.deliver(2, ["AUTH", "challenge-b"]);

    await fixture.waitUntil(() => fixture.signerInputs.length === 2, {
      what: "both signers consulted",
    });
    assert.deepEqual(fixture.signerInputs, [
      {
        challenge: "challenge-a",
        relayUrl: `ws://127.0.0.1:${infoA.port}`,
      },
      {
        challenge: "challenge-b",
        relayUrl: `ws://127.0.0.1:${infoB.port}`,
      },
    ]);

    await fixture.waitUntil(() => fixture.framesFrom(1, "AUTH").length === 1, {
      what: "client A AUTH frame",
    });
    await fixture.waitUntil(() => fixture.framesFrom(2, "AUTH").length === 1, {
      what: "client B AUTH frame",
    });
    assert.equal(
      authEventTags(fixture.framesFrom(1, "AUTH")[0].payload[1]).relay,
      "wss://canonical-a.example",
      "client A signs its own community's advertised identity",
    );
    assert.equal(
      authEventTags(fixture.framesFrom(2, "AUTH")[0].payload[1]).relay,
      "wss://canonical-b.example",
      "client B signs its own community's advertised identity",
    );

    // Close both sessions out.
    const eventA = fixture.framesFrom(1, "AUTH")[0].payload[1];
    const eventB = fixture.framesFrom(2, "AUTH")[0].payload[1];
    fixture.deliver(1, ["OK", eventA.id, true, ""]);
    fixture.deliver(2, ["OK", eventB.id, true, ""]);
    await subscribedA;
    await subscribedB;
  } finally {
    clientA.disconnect();
    clientB.disconnect();
    fixture.restore();
    await infoA.close();
    await infoB.close();
  }
});

test("mutation control: a transport-signing signer fails the canonical-tag assertions above", async () => {
  const info = await startInfoServer({
    body: JSON.stringify({ push: { origin: "wss://canonical.example" } }),
  });
  const aliasUrl = `ws://127.0.0.1:${info.port}`;
  // Degraded signer = the bug shape: sign the transport verbatim, never
  // consult /info. If the production session ever stopped feeding the
  // signer its own URL for canonical resolution, this is the observable
  // the happy-path assertions would have to catch.
  const fixture = installTauriFixture({
    relayWsUrl: aliasUrl,
    signer: transportSigningCreateAuthEvent,
  });
  const client = new RelayClient();
  try {
    const subscribed = client.subscribeLive(FILTER, () => {});
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

    // Still gate-compliant: close the session cleanly.
    const event = fixture.framesFrom(1, "AUTH")[0].payload[1];
    fixture.deliver(1, ["OK", event.id, true, ""]);
    await subscribed;
  } finally {
    client.disconnect();
    fixture.restore();
    await info.close();
  }
});
