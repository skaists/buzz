import assert from "node:assert/strict";
import { test } from "node:test";

import { describeReviewGate, shortPubkey } from "./reviewGate.ts";

const REVIEWER =
  "755029663fee78e734a217867246e22933a4683dda19dd8435488688a29f9541";

function step(overrides) {
  return {
    stepId: "review",
    status: "completed",
    output: {},
    startedAt: null,
    completedAt: null,
    error: null,
    ...overrides,
  };
}

function approval(overrides) {
  return {
    approvalRef: "ab".repeat(32),
    workflowId: "wf",
    runId: "run",
    stepId: "review",
    stepIndex: 1,
    approverSpec: REVIEWER,
    status: "pending",
    approverPubkey: null,
    note: null,
    candidateRef: "0123abcd",
    expiresAt: "2999-01-01T00:00:00.000Z",
    createdAt: 0,
    ...overrides,
  };
}

test("non-gate steps are not review gates", () => {
  assert.equal(
    describeReviewGate(step({ stepId: "announce", output: { event_id: "x" } })),
    null,
  );
  assert.equal(describeReviewGate(step({ stepId: "announce" })), null);
});

test("a minted gate reads as waiting with its bindings", () => {
  const gate = describeReviewGate(
    step({
      status: "waiting_approval",
      output: {
        approval_ref: "ab".repeat(32),
        approver_spec: REVIEWER,
        candidate_ref: "0123abcd",
        message: "review it",
        requested_at: "2026-09-18T02:00:00+00:00",
        expires_at: "2999-01-01T00:00:00+00:00",
      },
    }),
    null,
    new Date("2026-09-18T03:00:00Z"),
  );
  assert.deepEqual(gate, {
    state: "waiting",
    reviewerSpec: REVIEWER,
    decidedBy: null,
    candidateRef: "0123abcd",
    note: null,
    decisionEventId: null,
    approvalRef: "ab".repeat(32),
    requestedAt: "2026-09-18T02:00:00+00:00",
    decidedAt: null,
    expiresAt: "2999-01-01T00:00:00+00:00",
    reasserted: false,
  });
});

test("a granted gate carries the reviewer's evidence", () => {
  const gate = describeReviewGate(
    step({
      status: "completed",
      output: {
        approval_ref: "ab".repeat(32),
        approver_spec: REVIEWER,
        candidate_ref: "0123abcd",
        decision: "granted",
        approver: REVIEWER,
        note: "tests green at 0123abcd",
        decision_event_id: "evt-1",
        decided_at: "2026-09-18T04:00:00+00:00",
        reasserted: true,
      },
    }),
  );
  assert.equal(gate?.state, "granted");
  assert.equal(gate?.decidedBy, REVIEWER);
  assert.equal(gate?.note, "tests green at 0123abcd");
  assert.equal(gate?.decisionEventId, "evt-1");
  assert.equal(gate?.decidedAt, "2026-09-18T04:00:00+00:00");
  assert.equal(gate?.reasserted, true);
});

test("a denied gate reads as denied even without a decision field", () => {
  const gate = describeReviewGate(
    step({ status: "denied", output: { approval_ref: "ab".repeat(32) } }),
  );
  assert.equal(gate?.state, "denied");
});

test("the approvals API row fills gaps when the trace has no evidence yet", () => {
  const gate = describeReviewGate(
    step({ status: "waiting_approval", output: {} }),
    approval({ status: "granted", approverPubkey: REVIEWER, note: "ok" }),
  );
  assert.equal(gate?.state, "granted");
  assert.equal(gate?.decidedBy, REVIEWER);
  assert.equal(gate?.candidateRef, "0123abcd");
  assert.equal(gate?.note, "ok");
  assert.equal(gate?.reviewerSpec, REVIEWER);
});

test("an approval row for a different step does not turn a step into a gate", () => {
  assert.equal(
    describeReviewGate(
      step({ stepId: "announce" }),
      approval({ stepId: "review" }),
    ),
    null,
  );
});

test("a pending gate past its expiry reads as expired", () => {
  const gate = describeReviewGate(
    step({
      status: "waiting_approval",
      output: {
        approval_ref: "ab".repeat(32),
        expires_at: "2026-09-18T02:00:00+00:00",
      },
    }),
    null,
    new Date("2026-09-19T00:00:00Z"),
  );
  assert.equal(gate?.state, "expired");
});

test("shortPubkey abbreviates hex, names 'any', passes other specs through", () => {
  assert.equal(shortPubkey(REVIEWER), "75502966…9541");
  assert.equal(shortPubkey("any"), "any member");
  assert.equal(shortPubkey("@release-manager"), "@release-manager");
  assert.equal(shortPubkey(null), null);
});
