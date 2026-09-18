import assert from "node:assert/strict";
import { test } from "node:test";

import { describeRunBlocker, runBlockerLabel } from "./reviewGate.ts";

const REVIEWER =
  "755029663fee78e734a217867246e22933a4683dda19dd8435488688a29f9541";
const NOW = new Date("2026-09-18T06:00:00Z");

function gateStep(overrides = {}) {
  return {
    stepId: "review",
    status: "waiting_approval",
    output: {
      approval_ref: "ab".repeat(32),
      approver_spec: REVIEWER,
      candidate_ref: "0123abcd",
      requested_at: "2026-09-18T02:00:00+00:00",
      expires_at: "2026-09-19T02:00:00+00:00",
    },
    startedAt: null,
    completedAt: null,
    error: null,
    ...overrides,
  };
}

function plainStep(stepId) {
  return {
    stepId,
    status: "completed",
    output: { event_id: "x" },
    startedAt: null,
    completedAt: null,
    error: null,
  };
}

function run(overrides = {}) {
  return {
    status: "waiting_approval",
    executionTrace: [plainStep("build"), gateStep()],
    ...overrides,
  };
}

test("a run waiting on a gate names the reviewer, candidate and expiry", () => {
  const blocker = describeRunBlocker(run(), NOW);
  assert.deepEqual(blocker, {
    kind: "awaiting_review",
    stepId: "review",
    reviewerSpec: REVIEWER,
    candidateRef: "0123abcd",
    expiresAt: "2026-09-19T02:00:00+00:00",
  });
  assert.equal(
    runBlockerLabel(blocker),
    "Waiting on review by 75502966…9541 · candidate 0123abcd",
  );
});

test("an `any` gate without a candidate says so plainly", () => {
  const blocker = describeRunBlocker(
    run({
      executionTrace: [
        gateStep({
          output: { approval_ref: "ab".repeat(32), approver_spec: "any" },
        }),
      ],
    }),
    NOW,
  );
  assert.equal(blocker.kind, "awaiting_review");
  assert.equal(blocker.candidateRef, null);
  assert.equal(runBlockerLabel(blocker), "Waiting on review by any member");
});

test("a waiting run whose gate has lapsed reads as expired, not waiting", () => {
  const blocker = describeRunBlocker(run(), new Date("2026-09-20T00:00:00Z"));
  assert.equal(blocker.kind, "review_expired");
  assert.equal(
    runBlockerLabel(blocker),
    "Review gate expired · reviewer 75502966…9541 · candidate 0123abcd",
  );
});

test("the most recent undecided gate wins over an earlier granted one", () => {
  const blocker = describeRunBlocker(
    run({
      executionTrace: [
        gateStep({
          stepId: "first-review",
          status: "completed",
          output: {
            approval_ref: "cd".repeat(32),
            approver_spec: REVIEWER,
            decision: "granted",
          },
        }),
        gateStep({ stepId: "second-review" }),
      ],
    }),
    NOW,
  );
  assert.equal(blocker.stepId, "second-review");
});

test("no blocker is invented for runs that are not waiting or have no gate in the trace", () => {
  for (const status of [
    "pending",
    "running",
    "completed",
    "failed",
    "cancelled",
  ]) {
    assert.equal(describeRunBlocker(run({ status }), NOW), null);
  }
  assert.equal(
    describeRunBlocker(run({ executionTrace: [plainStep("build")] }), NOW),
    null,
  );
  assert.equal(describeRunBlocker(run({ executionTrace: [] }), NOW), null);
});
