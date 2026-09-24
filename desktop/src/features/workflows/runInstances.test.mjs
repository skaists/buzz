import assert from "node:assert/strict";
import { test } from "node:test";

import { summarizeRunInstances } from "./runInstances.ts";

const REVIEWER =
  "755029663fee78e734a217867246e22933a4683dda19dd8435488688a29f9541";
const NOW = new Date("2026-09-18T06:00:00Z");

function wf(id, name, channelName = "bBUGreports") {
  return {
    workflow: {
      id,
      name,
      ownerPubkey: REVIEWER,
      channelId: "chan",
      definition: {},
      status: "active",
      createdAt: 0,
      updatedAt: 0,
    },
    channelName,
  };
}

function run(id, status, createdAt, overrides = {}) {
  return {
    id,
    workflowId: "unset",
    status,
    currentStep: null,
    executionTrace: [],
    startedAt: createdAt,
    completedAt: null,
    errorCode: null,
    errorMessage: null,
    createdAt,
    ...overrides,
  };
}

const gate = {
  stepId: "review",
  status: "waiting_approval",
  output: {
    approval_ref: "ab".repeat(32),
    approver_spec: REVIEWER,
    candidate_ref: "0123abcd",
    expires_at: "2026-09-19T00:00:00+00:00",
  },
  startedAt: null,
  completedAt: null,
  error: null,
};

function ids(list) {
  return list.map((i) => `${i.workflowId}/${i.run.id}`);
}

test("instances from every workflow are grouped by durable run status", () => {
  const summary = summarizeRunInstances(
    [wf("a", "Build review"), wf("b", "Nightly")],
    {
      a: [
        run("a1", "waiting_approval", 100, { executionTrace: [gate] }),
        run("a2", "completed", 50, { completedAt: 60 }),
      ],
      b: [run("b1", "running", 200), run("b2", "pending", 210)],
    },
    NOW,
  );
  assert.deepEqual(ids(summary.waiting), ["a/a1"]);
  assert.deepEqual(ids(summary.running), ["b/b2", "b/b1"]);
  assert.deepEqual(ids(summary.recent), ["a/a2"]);
  assert.equal(summary.waiting[0].workflowName, "Build review");
  assert.equal(summary.waiting[0].channelName, "bBUGreports");
});

test("a waiting instance carries its blocker; others carry none", () => {
  const summary = summarizeRunInstances(
    [wf("a", "Build review")],
    {
      a: [
        run("a1", "waiting_approval", 100, { executionTrace: [gate] }),
        run("a2", "running", 90),
      ],
    },
    NOW,
  );
  assert.equal(summary.waiting[0].blocker.kind, "awaiting_review");
  assert.equal(summary.waiting[0].blocker.candidateRef, "0123abcd");
  assert.equal(summary.running[0].blocker, null);
});

test("the longest-waiting obligation is listed first", () => {
  const summary = summarizeRunInstances(
    [wf("a", "A"), wf("b", "B")],
    {
      a: [run("new", "waiting_approval", 300)],
      b: [run("old", "waiting_approval", 100)],
    },
    NOW,
  );
  assert.deepEqual(ids(summary.waiting), ["b/old", "a/new"]);
});

test("recent is newest-finished first and bounded", () => {
  const runs = [];
  for (let i = 1; i <= 8; i += 1) {
    runs.push(
      run(`r${i}`, i % 2 ? "completed" : "failed", i, { completedAt: i * 10 }),
    );
  }
  const summary = summarizeRunInstances([wf("a", "A")], { a: runs }, NOW, 3);
  assert.deepEqual(ids(summary.recent), ["a/r8", "a/r7", "a/r6"]);
  assert.equal(summary.recentTotal, 8);
});

test("a finished run without a completion time falls back to its creation time", () => {
  const summary = summarizeRunInstances(
    [wf("a", "A")],
    {
      a: [
        run("timed", "completed", 10, { completedAt: 20 }),
        run("untimed", "cancelled", 30),
      ],
    },
    NOW,
  );
  assert.deepEqual(ids(summary.recent), ["a/untimed", "a/timed"]);
});

test("workflows whose runs have not loaded contribute nothing, and unknown ids are ignored", () => {
  const summary = summarizeRunInstances(
    [wf("a", "A")],
    { a: undefined, ghost: [run("g1", "running", 1)] },
    NOW,
  );
  assert.deepEqual(summary, {
    waiting: [],
    running: [],
    recent: [],
    recentTotal: 0,
  });
});
