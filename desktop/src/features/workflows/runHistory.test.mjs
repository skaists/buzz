import assert from "node:assert/strict";
import { test } from "node:test";

import { describeRunTransitions } from "./runHistory.ts";

const REVIEWER =
  "755029663fee78e734a217867246e22933a4683dda19dd8435488688a29f9541";
const EVENT_ID = "e1".repeat(32);
const NOW = new Date("2026-09-18T06:00:00Z");
const T0 = Date.parse("2026-09-18T02:00:00Z") / 1000;

function plainStep(stepId, status = "completed") {
  return {
    stepId,
    status,
    output: status === "completed" ? { event_id: "x" } : {},
    startedAt: null,
    completedAt: null,
    error: null,
  };
}

function gateStep(output = {}, overrides = {}) {
  return {
    stepId: "review",
    status: "waiting_approval",
    output: {
      approval_ref: "ab".repeat(32),
      approver_spec: REVIEWER,
      candidate_ref: "0123abcd",
      requested_at: "2026-09-18T02:00:05+00:00",
      expires_at: "2026-09-19T02:00:05+00:00",
      ...output,
    },
    startedAt: T0 + 5,
    completedAt: null,
    error: null,
    ...overrides,
  };
}

function run(overrides = {}) {
  return {
    id: "run-1",
    workflowId: "wf",
    status: "completed",
    currentStep: null,
    executionTrace: [],
    startedAt: T0 + 1,
    completedAt: T0 + 60,
    errorCode: null,
    errorMessage: null,
    createdAt: T0,
    ...overrides,
  };
}

function shape(transitions) {
  return transitions.map((t) => [t.kind, t.stepId, t.at]);
}

test("a granted run reads created → started → steps → requested → granted → completed, in trace order", () => {
  const transitions = describeRunTransitions(
    run({
      executionTrace: [
        plainStep("build"),
        gateStep(
          {
            decision: "granted",
            approver: REVIEWER,
            decision_event_id: EVENT_ID,
            decided_at: "2026-09-18T02:00:40+00:00",
          },
          { status: "completed" },
        ),
        plainStep("announce"),
      ],
    }),
    [],
    NOW,
  );
  assert.deepEqual(shape(transitions), [
    ["created", null, T0 * 1000],
    ["started", null, (T0 + 1) * 1000],
    ["step", "build", null],
    ["review_requested", "review", (T0 + 5) * 1000],
    ["review_granted", "review", (T0 + 40) * 1000],
    ["step", "announce", null],
    ["completed", null, (T0 + 60) * 1000],
  ]);
  const granted = transitions[4];
  assert.equal(granted.label, "Review granted by 75502966…9541");
  assert.equal(granted.evidenceEventId, EVENT_ID);
  assert.equal(
    transitions[3].label,
    "Review requested from 75502966…9541 · candidate 0123abcd",
  );
  assert.equal(transitions[2].label, "Step build completed");
});

test("untimed steps carry no invented time", () => {
  const transitions = describeRunTransitions(
    run({
      executionTrace: [plainStep("build"), plainStep("skip-me", "skipped")],
    }),
    [],
    NOW,
  );
  assert.deepEqual(
    transitions.filter((t) => t.kind === "step").map((t) => [t.label, t.at]),
    [
      ["Step build completed", null],
      ["Step skip-me skipped", null],
    ],
  );
});

test("a waiting run ends at the request: no terminal transition is invented", () => {
  const transitions = describeRunTransitions(
    run({
      status: "waiting_approval",
      completedAt: null,
      currentStep: 1,
      executionTrace: [plainStep("build"), gateStep()],
    }),
    [],
    NOW,
  );
  assert.equal(transitions.at(-1).kind, "review_requested");
});

test("a lapsed gate on a waiting run records the expiry at its deadline", () => {
  const transitions = describeRunTransitions(
    run({
      status: "waiting_approval",
      completedAt: null,
      executionTrace: [gateStep()],
    }),
    [],
    new Date("2026-09-20T00:00:00Z"),
  );
  const last = transitions.at(-1);
  assert.equal(last.kind, "review_expired");
  assert.equal(last.at, Date.parse("2026-09-19T02:00:05+00:00"));
  assert.equal(last.label, "Review gate expired");
});

test("a denied run records who denied and why the run failed", () => {
  const transitions = describeRunTransitions(
    run({
      status: "failed",
      errorCode: "approval_denied",
      executionTrace: [
        gateStep(
          {
            decision: "denied",
            approver: REVIEWER,
            decision_event_id: EVENT_ID,
            decided_at: "2026-09-18T02:00:40+00:00",
          },
          { status: "denied" },
        ),
      ],
    }),
    [],
    NOW,
  );
  assert.deepEqual(
    transitions.slice(-2).map((t) => [t.kind, t.label]),
    [
      ["review_denied", "Review denied by 75502966…9541"],
      ["failed", "Run failed (approval denied)"],
    ],
  );
});

test("the approvals row fills a decision the trace has not recorded yet", () => {
  const transitions = describeRunTransitions(
    run({
      status: "waiting_approval",
      completedAt: null,
      executionTrace: [gateStep()],
    }),
    [
      {
        approvalRef: "ab".repeat(32),
        workflowId: "wf",
        runId: "run-1",
        stepId: "review",
        stepIndex: 0,
        approverSpec: REVIEWER,
        status: "granted",
        approverPubkey: REVIEWER,
        note: null,
        candidateRef: "0123abcd",
        expiresAt: "2026-09-19T02:00:05+00:00",
        createdAt: T0 + 5,
      },
    ],
    NOW,
  );
  const last = transitions.at(-1);
  assert.equal(last.kind, "review_granted");
  assert.equal(last.at, null);
  assert.equal(last.evidenceEventId, null);
});

test("a pending run with no trace is just its creation", () => {
  const transitions = describeRunTransitions(
    run({ status: "pending", startedAt: null, completedAt: null }),
    [],
    NOW,
  );
  assert.deepEqual(shape(transitions), [["created", null, T0 * 1000]]);
});

test("cancelled and failed runs name their terminal state", () => {
  assert.equal(
    describeRunTransitions(run({ status: "cancelled" }), [], NOW).at(-1).label,
    "Run cancelled",
  );
  assert.equal(
    describeRunTransitions(run({ status: "failed" }), [], NOW).at(-1).label,
    "Run failed",
  );
});
