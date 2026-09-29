// Codex P1 on #10: a slow submit cannot be cancelled, so the 20 s deadline
// must never hand the buttons back while the original grant/deny can still
// reach the relay. Only the request's real outcome ends the wait.
//
// Namespace import on purpose: against the pre-fix module these tests fail on
// behaviour (the "uncertain" state fell through to live buttons) rather than
// on a missing named export at link time.
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

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

/** Drive the card the way WorkflowApprovalCard does and record its views. */
function start(request, { refetch = () => {} } = {}) {
  const card = { phase: "idle", errorMessage: null, refetches: 0 };
  card.view = (over = {}) =>
    decision.approvalCardView({
      approval,
      myPubkey: ME,
      nowMs: NOW,
      phase: card.phase,
      action: "grant",
      errorMessage: card.errorMessage,
      ...over,
    });
  card.handle = decision.trackApprovalDecision(request, {
    timeoutMs: TIMEOUT,
    refetchMs: REFETCH,
    refetch: () => {
      card.refetches++;
      refetch();
    },
    onPhase: (update) => {
      card.phase = update.phase;
      if (update.phase === "failed") card.errorMessage = update.errorMessage;
    },
  });
  return card;
}

test("P1: the timeout does not re-enable the buttons", (t) => {
  // Pre-fix, the timed-out card rendered live buttons.
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

  t.mock.timers.enable({ apis: ["setTimeout", "setInterval"] });
  const card = start(new Promise(() => {}));
  assert.equal(card.phase, "sending");
  assert.equal(card.view().buttonsDisabled, true);
  t.mock.timers.tick(TIMEOUT);
  assert.equal(card.phase, "uncertain");
  assert.equal(card.view().buttonsDisabled, true);
  assert.equal(card.view().statusText, decision.APPROVAL_STILL_WAITING_TEXT);
  // Still locked long after the deadline: nothing but the answer unlocks it.
  t.mock.timers.tick(10 * TIMEOUT);
  assert.equal(card.view().buttonsDisabled, true);
  card.handle.dispose();
});

test("P1: a late success settles the card", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout", "setInterval"] });
  const request = deferred();
  const card = start(request.promise);
  t.mock.timers.tick(TIMEOUT + 1);
  assert.equal(card.phase, "uncertain");
  request.resolve();
  await card.handle.settled;
  assert.equal(card.phase, "sent");
  const v = card.view();
  assert.equal(v.mode, "settled");
  assert.equal(v.buttonsDisabled, true);
  assert.match(v.statusText, /^Approved/);
});

test("P1: a late failure re-enables the buttons with the relay's error", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout", "setInterval"] });
  const request = deferred();
  const card = start(request.promise);
  t.mock.timers.tick(TIMEOUT + 1);
  assert.equal(card.view().buttonsDisabled, true);
  request.reject(new Error("forbidden: candidate mismatch"));
  await card.handle.settled;
  assert.equal(card.phase, "failed");
  const v = card.view();
  assert.equal(v.mode, "actions");
  assert.equal(v.buttonsDisabled, false);
  assert.equal(v.error, "forbidden: candidate mismatch");
});

test("P1: a late 'already settled' refusal shows the settled state, not buttons", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout", "setInterval"] });
  const request = deferred();
  const card = start(request.promise);
  t.mock.timers.tick(TIMEOUT + 1);
  request.reject(new Error("forbidden: approval already denied"));
  await card.handle.settled;
  assert.equal(card.view().mode, "settled");
  assert.equal(card.view().buttonsDisabled, true);
});

test("P1: approvals are re-read while waiting and the polling stops on the answer", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout", "setInterval"] });
  const request = deferred();
  const card = start(request.promise);
  t.mock.timers.tick(TIMEOUT - 1);
  assert.equal(card.refetches, 0, "no extra reads before the deadline");
  t.mock.timers.tick(1);
  assert.equal(card.refetches, 1, "re-read immediately at the deadline");
  t.mock.timers.tick(3 * REFETCH);
  assert.equal(card.refetches, 4);
  request.resolve();
  await card.handle.settled;
  t.mock.timers.tick(5 * REFETCH);
  assert.equal(card.refetches, 4, "no reads after the relay answered");
});

test("P1: a settle seen from the relay while waiting settles the card", (t) => {
  t.mock.timers.enable({ apis: ["setTimeout", "setInterval"] });
  const card = start(new Promise(() => {}));
  t.mock.timers.tick(TIMEOUT);
  for (const [status, label] of [
    ["granted", "Approved"],
    ["denied", "Denied"],
  ]) {
    const v = card.view({ approval: { ...approval, status } });
    assert.equal(v.mode, "settled");
    assert.equal(v.buttonsDisabled, true);
    assert.equal(v.statusText, label);
  }
  // Once the record shows the settle, the card stops re-reading.
  const before = card.refetches;
  card.handle.stopPolling();
  t.mock.timers.tick(5 * REFETCH);
  assert.equal(card.refetches, before);
  card.handle.dispose();
});

test("an answer inside the deadline never shows the waiting state", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout", "setInterval"] });
  const request = deferred();
  const card = start(request.promise);
  request.resolve();
  await card.handle.settled;
  t.mock.timers.tick(2 * TIMEOUT);
  assert.equal(card.phase, "sent");
  assert.equal(card.refetches, 0);
});

test("a card that goes away ignores the late answer", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout", "setInterval"] });
  const request = deferred();
  const card = start(request.promise);
  card.handle.dispose();
  t.mock.timers.tick(2 * TIMEOUT);
  request.resolve();
  await card.handle.settled;
  assert.equal(card.phase, "sending");
  assert.equal(card.refetches, 0);
});
