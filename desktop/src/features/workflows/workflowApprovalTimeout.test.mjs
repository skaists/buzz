// Overdue approval decisions (Codex P1 on #10, P2s on #13, audit points).
//
// - The 20 s deadline never hands the buttons back while a submit is unresolved.
// - Epoch fencing: a late answer from an abandoned attempt never changes state.
// - No double-sign: after an unknown outcome, a second signing needs a verified
//   relay read first, and then only the same decision (never Approve → Deny).
//
// Namespace import on purpose: against an older module these tests fail on
// behaviour or a missing function, not at link time.
import assert from "node:assert/strict";
import test from "node:test";

import * as decision from "./workflowApprovalDecision.mjs";

const ME = "56694530e53104c824408896ebb78751de7d2894800308369f463feff4a8bd27";
const NOW = Date.parse("2026-09-28T12:00:00Z");
const approval = {
  status: "pending",
  expiresAt: "2026-09-28T13:00:00Z",
  approverSpec: ME,
  approvalRef: "ab".repeat(32),
  candidateRef: "commitA",
};
const TIMEOUT = 20_000;
const REFETCH = 3_000;
const UNREACHABLE = "relay unreachable: request timed out";
const REFUSED = "relay rejected event: forbidden: candidate mismatch";

// Drain pending promise callbacks (setImmediate is not mocked).
const flush = () => new Promise((resolve) => setImmediate(resolve));

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

/**
 * A card harness: every send/verify call gets its own deferred so the test
 * decides when (and in which order) the relay answers.
 */
function card({ locks = new Map(), lockKey = approval.approvalRef } = {}) {
  const h = {
    sends: [],
    verifies: [],
    refetches: 0,
    refetchResult: undefined,
    state: null,
  };
  h.controller = decision.createApprovalDecisionController({
    lockKey,
    locks,
    timeoutMs: TIMEOUT,
    refetchMs: REFETCH,
    send: ({ action, attempt }) => {
      const d = deferred();
      h.sends.push({ action, attempt, ...d });
      return d.promise;
    },
    verify: () => {
      const d = deferred();
      h.verifies.push(d);
      return d.promise;
    },
    refetch: () => {
      h.refetches++;
      return h.refetchResult;
    },
    onChange: (state) => {
      h.state = state;
    },
  });
  h.state = h.controller.getState();
  h.view = (over = {}) =>
    decision.approvalCardView({
      approval,
      myPubkey: ME,
      nowMs: NOW,
      phase: h.state.phase,
      action: h.state.action,
      errorMessage: h.state.errorMessage,
      lockedAction: h.state.lockedAction,
      settledStatus: h.state.settledStatus,
      ...over,
    });
  return h;
}

const enableTimers = (t) =>
  t.mock.timers.enable({ apis: ["setTimeout", "setInterval"] });

// ── P1: the deadline never hands the buttons back ───────────────────────────

test("P1: the timeout does not re-enable the buttons", (t) => {
  const timedOut = decision.approvalCardView({
    approval,
    myPubkey: ME,
    nowMs: NOW,
    phase: "uncertain",
    action: "grant",
  });
  assert.equal(timedOut.buttonsDisabled, true);
  assert.notEqual(timedOut.mode, "actions");
  assert.equal(timedOut.statusText, "Still waiting for the relay…");
  assert.equal(timedOut.error, null);

  enableTimers(t);
  const h = card();
  h.controller.submit("grant");
  assert.equal(h.state.phase, "sending");
  t.mock.timers.tick(TIMEOUT);
  assert.equal(h.state.phase, "uncertain");
  assert.deepEqual(h.view().disabledActions, { grant: true, deny: true });
  assert.equal(h.controller.canSubmit("grant"), false);
  assert.equal(h.controller.canSubmit("deny"), false);
  t.mock.timers.tick(10 * TIMEOUT);
  assert.equal(h.view().buttonsDisabled, true);
  h.controller.dispose();
});

test("P1: a late success settles the card", async (t) => {
  enableTimers(t);
  const h = card();
  h.controller.submit("grant");
  t.mock.timers.tick(TIMEOUT + 1);
  h.sends[0].resolve();
  await flush();
  assert.equal(h.state.phase, "sent");
  assert.equal(h.view().mode, "settled");
  assert.match(h.view().statusText, /^Approved/);
});

test("P1: a late relay refusal re-enables both buttons only after a verified read", async (t) => {
  enableTimers(t);
  const h = card();
  h.controller.submit("grant");
  t.mock.timers.tick(TIMEOUT + 1);
  h.sends[0].reject(new Error(REFUSED));
  await flush();
  assert.equal(h.state.phase, "verifying");
  assert.equal(h.view().buttonsDisabled, true);
  h.verifies.at(-1).resolve("pending");
  await flush();
  assert.equal(h.state.phase, "failed");
  const v = h.view();
  assert.equal(v.mode, "actions");
  // The relay refused the grant, so it can never land: both are safe.
  assert.deepEqual(v.disabledActions, { grant: false, deny: false });
  assert.equal(v.error, REFUSED);
});

test("P1: a late 'already settled' refusal shows the settled state, not buttons", async (t) => {
  enableTimers(t);
  const h = card();
  h.controller.submit("grant");
  t.mock.timers.tick(TIMEOUT + 1);
  h.sends[0].reject(new Error("relay rejected event: approval already denied"));
  await flush();
  assert.equal(h.view().mode, "settled");
  assert.equal(h.view().buttonsDisabled, true);
});

test("P1: the gate is re-read while waiting and polling stops on the answer", async (t) => {
  enableTimers(t);
  const h = card();
  h.controller.submit("grant");
  t.mock.timers.tick(TIMEOUT - 1);
  assert.equal(h.refetches, 0, "no extra reads before the deadline");
  assert.equal(h.verifies.length, 0);
  t.mock.timers.tick(1);
  assert.equal(h.refetches, 1, "re-read immediately at the deadline");
  assert.equal(h.verifies.length, 1);
  for (let i = 0; i < 3; i++) {
    h.verifies.at(-1).resolve("pending");
    await flush();
    t.mock.timers.tick(REFETCH);
  }
  assert.equal(h.refetches, 4);
  assert.equal(h.verifies.length, 4);
  // A verified "pending" while the submit is unresolved re-enables nothing.
  assert.equal(h.state.phase, "uncertain");
  h.sends[0].resolve();
  await flush();
  for (let i = 0; i < 5; i++) {
    await flush();
    t.mock.timers.tick(REFETCH);
  }
  assert.equal(h.refetches, 4, "no reads after the relay answered");
});

test("P1: a settle seen from the relay while waiting settles the card", async (t) => {
  enableTimers(t);
  const h = card();
  h.controller.submit("grant");
  t.mock.timers.tick(TIMEOUT);
  // Via the cached record…
  const v = h.view({ approval: { ...approval, status: "denied" } });
  assert.equal(v.mode, "settled");
  assert.equal(v.statusText, "Denied");
  // …and via the verified read.
  h.verifies[0].resolve("granted");
  await flush();
  assert.equal(h.state.phase, "settled");
  assert.equal(h.view().mode, "settled");
  assert.match(h.view().statusText, /^Approved/);
  const reads = h.refetches;
  t.mock.timers.tick(5 * REFETCH);
  assert.equal(h.refetches, reads, "polling stopped once settled");
});

test("an answer inside the deadline never shows the waiting state", async (t) => {
  enableTimers(t);
  const h = card();
  h.controller.submit("deny");
  h.sends[0].resolve();
  await flush();
  t.mock.timers.tick(2 * TIMEOUT);
  assert.equal(h.state.phase, "sent");
  assert.equal(h.refetches, 0);
  assert.equal(h.verifies.length, 0);
});

test("audit 4 unchanged: a relay refusal inside the deadline re-enables at once", async (t) => {
  enableTimers(t);
  const h = card();
  h.controller.submit("grant");
  h.sends[0].reject(new Error(REFUSED));
  await flush();
  assert.equal(h.state.phase, "failed");
  assert.deepEqual(h.view().disabledActions, { grant: false, deny: false });
  assert.equal(h.verifies.length, 0, "a definitive refusal needs no re-read");
});

// ── P2s ─────────────────────────────────────────────────────────────────────

test("P2: a slow cache refresh is never overlapped by the next tick", async (t) => {
  enableTimers(t);
  const h = card();
  const read = deferred();
  h.refetchResult = read.promise;
  h.controller.submit("grant");
  t.mock.timers.tick(TIMEOUT);
  assert.equal(h.refetches, 1);
  for (let i = 0; i < 5; i++) {
    await flush();
    t.mock.timers.tick(REFETCH);
  }
  assert.equal(h.refetches, 1, "still waiting on the first refresh");
  read.resolve();
  h.refetchResult = undefined;
  await flush();
  t.mock.timers.tick(REFETCH);
  assert.equal(h.refetches, 2, "next refresh only after the slow one ended");
  h.controller.dispose();
});

test("P2: a stop before the deadline keeps the deadline from starting a poll", async (t) => {
  enableTimers(t);
  const h = card();
  h.controller.submit("grant");
  t.mock.timers.tick(TIMEOUT / 2);
  h.controller.stopPolling(); // the ordinary refresh showed the gate settled
  t.mock.timers.tick(TIMEOUT);
  for (let i = 0; i < 5; i++) {
    await flush();
    t.mock.timers.tick(REFETCH);
  }
  assert.equal(h.refetches, 0);
  assert.equal(h.verifies.length, 0);
  h.controller.dispose();
});

test("P2: an unresolved decision on an expired gate shows Expired, not busy", () => {
  const past = Date.parse(approval.expiresAt) + 1;
  for (const phase of ["uncertain", "verifying"]) {
    const v = decision.approvalCardView({
      approval,
      myPubkey: ME,
      nowMs: past,
      phase,
      action: "grant",
    });
    assert.equal(v.mode, "settled");
    assert.equal(v.settledStatus, "expired");
    assert.equal(v.buttonsDisabled, true);
    const before = decision.approvalCardView({
      approval,
      myPubkey: ME,
      nowMs: NOW,
      phase,
      action: "grant",
    });
    assert.equal(before.mode, phase);
  }
});

// ── Audit 2: epoch fencing ──────────────────────────────────────────────────

test("audit 2: attempt ids are monotonic, and a late answer from an abandoned attempt never touches the newer one", async (t) => {
  enableTimers(t);
  const h = card();
  const first = h.controller.submit("grant");
  assert.equal(first, 1);
  // Attempt 1 goes overdue; its first verified read hangs.
  t.mock.timers.tick(TIMEOUT);
  const staleRead = h.verifies[0];
  // Attempt 1's submit then fails without a definitive refusal.
  h.sends[0].reject(new Error(UNREACHABLE));
  await flush();
  assert.equal(h.state.phase, "verifying");
  // A fresh verified read (new poll generation) says the gate is open.
  t.mock.timers.tick(REFETCH);
  h.verifies.at(-1).resolve("pending");
  await flush();
  assert.equal(h.state.phase, "failed");
  const second = h.controller.submit("grant");
  assert.equal(second, 2);
  assert.equal(h.state.phase, "sending");
  const before = { ...h.state };
  // The abandoned attempt's hung read finally answers with a stale status.
  // Honouring it would overwrite the newer attempt mid-flight. It must not be.
  staleRead.resolve("denied");
  await flush();
  assert.deepEqual(h.state, before);
  assert.equal(h.view().mode, "sending");
  assert.equal(h.controller.canSubmit("grant"), false);
  // The newer attempt's own answer still applies.
  h.sends[1].resolve();
  await flush();
  assert.equal(h.state.phase, "sent");
  assert.equal(h.state.attempt, 2);
});

test("audit 2: after the card goes away, a late answer never changes state", async (t) => {
  enableTimers(t);
  for (const late of ["success", "failure"]) {
    const h = card();
    h.controller.submit("grant");
    t.mock.timers.tick(TIMEOUT);
    const snapshot = { ...h.state };
    h.controller.dispose();
    if (late === "success") h.sends[0].resolve();
    else h.sends[0].reject(new Error(UNREACHABLE));
    h.verifies[0].resolve("granted");
    await flush();
    t.mock.timers.tick(5 * REFETCH);
    assert.deepEqual(h.state, snapshot, late);
    assert.equal(h.controller.submit("deny"), null);
  }
});

// ── Audit 3: no double-sign ─────────────────────────────────────────────────

test("audit 3: after a timeout, no second signing without a verified relay read, and never Approve then Deny", async (t) => {
  enableTimers(t);
  const h = card();
  h.controller.submit("grant");
  t.mock.timers.tick(TIMEOUT);
  // Overdue: nothing can be signed.
  assert.equal(h.controller.submit("deny"), null);
  assert.equal(h.controller.submit("grant"), null);
  // The submit fails with an unknown outcome (it may have reached the relay).
  h.sends[0].reject(new Error(UNREACHABLE));
  await flush();
  assert.equal(h.state.phase, "verifying");
  assert.equal(h.controller.submit("deny"), null);
  assert.equal(h.controller.submit("grant"), null);
  // A failed read is not a verification.
  h.verifies.at(-1).reject(new Error(UNREACHABLE));
  await flush();
  assert.equal(h.controller.submit("grant"), null);
  // An unknown status is not a verification either.
  t.mock.timers.tick(REFETCH);
  h.verifies.at(-1).resolve("unknown");
  await flush();
  assert.equal(h.controller.submit("grant"), null);
  // A verified "pending" allows a second signing — of the same decision only.
  t.mock.timers.tick(REFETCH);
  h.verifies.at(-1).resolve("pending");
  await flush();
  assert.equal(h.state.phase, "failed");
  assert.deepEqual(h.view().disabledActions, { grant: false, deny: true });
  assert.match(h.view().statusText, /only Approve can be sent again/);
  assert.equal(h.controller.submit("deny"), null);
  assert.equal(h.controller.submit("grant"), 2);
  assert.deepEqual(
    h.sends.map((s) => s.action),
    ["grant", "grant"],
    "Deny is never signed after an unresolved Approve",
  );
});

test("audit 3: a verified 'pending' while the submit is unresolved does not allow signing", async (t) => {
  enableTimers(t);
  const h = card();
  h.controller.submit("deny");
  t.mock.timers.tick(TIMEOUT);
  h.verifies[0].resolve("pending");
  await flush();
  assert.equal(h.state.phase, "uncertain");
  assert.equal(h.controller.submit("deny"), null);
  assert.equal(h.controller.submit("grant"), null);
  assert.equal(h.sends.length, 1);
  h.controller.dispose();
});

test("audit 3: the decision lock survives a card remount, which starts by verifying", async (t) => {
  enableTimers(t);
  const locks = new Map();
  const a = card({ locks });
  a.controller.submit("grant");
  a.sends[0].reject(new Error(UNREACHABLE));
  await flush();
  a.controller.dispose();
  // A fresh card for the same gate (remount, navigation) keeps the lock and
  // allows nothing until a verified read.
  const b = card({ locks });
  assert.equal(b.state.lockedAction, "grant");
  assert.equal(b.state.phase, "verifying");
  assert.equal(b.controller.submit("grant"), null);
  b.verifies[0].resolve("pending");
  await flush();
  assert.equal(b.controller.submit("deny"), null);
  assert.deepEqual(b.view().disabledActions, { grant: false, deny: true });
  assert.equal(b.controller.submit("grant"), 1);
  b.controller.dispose();
});

test("audit 3: a remount while the first submit is still in flight cannot sign the opposite decision", async (t) => {
  enableTimers(t);
  const locks = new Map();
  const a = card({ locks });
  a.controller.submit("grant");
  // Navigate away and back within the Rust deadline: the submit is unresolved.
  a.controller.dispose();
  const b = card({ locks });
  assert.equal(b.state.phase, "verifying");
  assert.equal(b.controller.submit("deny"), null);
  assert.equal(b.controller.submit("grant"), null);
  // The original lands after all; the verified read shows it.
  a.sends[0].resolve();
  b.verifies[0].resolve("granted");
  await flush();
  assert.equal(b.view().mode, "settled");
  assert.deepEqual(
    [...a.sends, ...b.sends].map((x) => x.action),
    ["grant"],
    "only the one decision was ever signed",
  );
  b.controller.dispose();
});

test("audit 3: a definitive relay refusal drops the decision record", async (t) => {
  enableTimers(t);
  const locks = new Map();
  const h = card({ locks });
  h.controller.submit("grant");
  assert.equal(locks.get(approval.approvalRef), "grant", "recorded when sent");
  h.sends[0].reject(new Error(REFUSED));
  await flush();
  assert.equal(locks.has(approval.approvalRef), false);
  assert.equal(h.state.lockedAction, null);
  assert.equal(h.controller.submit("deny"), 2);
  h.controller.dispose();
});

test("community teardown clears the decision records", () => {
  decision.UNCERTAIN_DECISION_LOCKS.set("some-gate", "grant");
  decision.resetUncertainDecisionLocks();
  assert.equal(decision.UNCERTAIN_DECISION_LOCKS.size, 0);
});

test("a hung verified read is retired and retried", async (t) => {
  enableTimers(t);
  const h = card();
  h.controller.submit("grant");
  h.sends[0].reject(new Error(UNREACHABLE));
  await flush();
  assert.equal(h.verifies.length, 1); // never answers
  t.mock.timers.tick(REFETCH);
  assert.equal(h.verifies.length, 1, "no overlap while the read is live");
  t.mock.timers.tick(10_000);
  assert.equal(h.verifies.length, 2, "retired after 10 s, then retried");
  h.verifies[1].resolve("pending");
  await flush();
  assert.equal(h.state.phase, "failed");
  h.controller.dispose();
});

test("a retired verified read's late answer is dropped", async (t) => {
  enableTimers(t);
  const h = card();
  h.controller.submit("grant");
  h.sends[0].reject(new Error(UNREACHABLE));
  await flush();
  t.mock.timers.tick(10_000 + REFETCH); // first read retired, second issued
  assert.equal(h.verifies.length, 2);
  // The retired read wakes up with "pending": it must not unlock anything
  // while the newer read is still underway.
  h.verifies[0].resolve("pending");
  await flush();
  assert.equal(h.state.phase, "verifying");
  assert.equal(h.controller.submit("grant"), null);
  h.verifies[1].resolve("pending");
  await flush();
  assert.equal(h.state.phase, "failed");
  h.controller.dispose();
});

test("a verified terminal state survives the submit's late failure", async (t) => {
  enableTimers(t);
  const h = card();
  h.controller.submit("grant");
  t.mock.timers.tick(TIMEOUT);
  h.verifies[0].resolve("granted");
  await flush();
  assert.equal(h.state.phase, "settled");
  h.sends[0].reject(new Error(UNREACHABLE));
  await flush();
  assert.equal(h.state.phase, "settled");
  t.mock.timers.tick(5 * REFETCH);
  assert.equal(h.verifies.length, 1, "no polling restarted");
});

test("overdue polling pauses while the app is not focused", async (t) => {
  enableTimers(t);
  let focused = false;
  const h = card();
  // Rebuild with an isActive gate (same harness, custom controller).
  h.controller.dispose();
  const reads = { refetch: 0, verify: 0 };
  const c = decision.createApprovalDecisionController({
    timeoutMs: TIMEOUT,
    refetchMs: REFETCH,
    locks: new Map(),
    isActive: () => focused,
    send: () => new Promise(() => {}),
    verify: () => {
      reads.verify++;
      return new Promise(() => {});
    },
    refetch: () => {
      reads.refetch++;
    },
    onChange: () => {},
  });
  c.submit("grant");
  t.mock.timers.tick(TIMEOUT + 5 * REFETCH);
  assert.deepEqual(reads, { refetch: 0, verify: 0 });
  focused = true;
  t.mock.timers.tick(REFETCH);
  assert.deepEqual(reads, { refetch: 1, verify: 1 });
  c.dispose();
});

test("identity key: a click is ignored while the attached controller belongs to another key", async (t) => {
  enableTimers(t);
  const locks = new Map();
  // The controller still attached was created before the identity loaded
  // (or before an identity switch).
  const stale = card({ locks, lockKey: `:${approval.approvalRef}` });
  const currentKey = `${ME}:${approval.approvalRef}`;
  assert.equal(stale.controller.lockKey, `:${approval.approvalRef}`);
  assert.equal(
    decision.submitForKey(stale.controller, currentKey, "deny"),
    null,
  );
  assert.equal(decision.submitForKey(null, currentKey, "grant"), null);
  assert.equal(stale.sends.length, 0, "nothing signed under the wrong key");
  assert.equal(locks.size, 0, "nothing recorded under the wrong key");
  stale.controller.dispose();
  // Once the controller for the current key is attached, clicks go through.
  const fresh = card({ locks, lockKey: currentKey });
  assert.equal(decision.submitForKey(fresh.controller, currentKey, "grant"), 1);
  assert.equal(locks.get(currentKey), "grant");
  fresh.controller.dispose();
});

test("identity key: both buttons stay disabled until the controller for the current key is attached", () => {
  const live = decision.approvalCardView({
    approval,
    myPubkey: ME,
    nowMs: NOW,
  });
  assert.deepEqual(live.disabledActions, { grant: false, deny: false });
  const currentKey = `${ME}:${approval.approvalRef}`;
  for (const controllerKey of [
    null,
    `:${approval.approvalRef}`,
    `other:${approval.approvalRef}`,
  ]) {
    const gated = decision.gateViewForKey(live, controllerKey, currentKey);
    assert.equal(gated.buttonsDisabled, true, String(controllerKey));
    assert.deepEqual(gated.disabledActions, { grant: true, deny: true });
  }
  assert.equal(decision.gateViewForKey(live, currentKey, currentKey), live);
});

test("only the relay's own accepted=false reply counts as a definitive refusal", () => {
  assert.equal(decision.isDefinitiveRelayRefusal(REFUSED), true);
  for (const message of [
    UNREACHABLE,
    "relay unreachable: network error",
    "approval submit abandoned: no relay answer within 15 s; it may or may not have reached the relay",
    "HTTP 504: gateway timeout",
    "",
    null,
  ])
    assert.equal(decision.isDefinitiveRelayRefusal(message), false, message);
});
