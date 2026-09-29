import assert from "node:assert/strict";
import test from "node:test";

import { workflowApprovalDecisionState } from "./workflowApprovalDecision.mjs";

const ME = "56694530e53104c824408896ebb78751de7d2894800308369f463feff4a8bd27";
const OTHER = "755029663fee78e734a217867246e22933a4683dda19dd8435488688a29f9541";
const REF = "AB".repeat(32);
const NOW = Date.parse("2026-09-28T12:00:00Z");
const base = {
  status: "pending",
  expiresAt: "2026-09-28T13:00:00Z",
  approverSpec: ME,
  approvalRef: REF,
  candidateRef: " commitA ",
};

test("designated approver gets live buttons bound to ref + candidate", () => {
  const s = workflowApprovalDecisionState(base, ME.toUpperCase(), NOW);
  assert.equal(s.visible, true);
  assert.equal(s.canDecide, true);
  assert.deepEqual(s.decision, { token: REF.toLowerCase(), candidate: "commitA" });
});

test("anyone else sees the card but cannot decide", () => {
  const s = workflowApprovalDecisionState(base, OTHER, NOW);
  assert.equal(s.visible, true);
  assert.equal(s.canDecide, false);
  assert.equal(s.reason, "not-approver");
});

test("unbound gate sends no candidate", () => {
  const s = workflowApprovalDecisionState({ ...base, candidateRef: null }, ME, NOW);
  assert.equal(s.decision.candidate, undefined);
});

test("decided or expired gates are hidden", () => {
  assert.equal(workflowApprovalDecisionState({ ...base, status: "granted" }, ME, NOW).visible, false);
  const e = workflowApprovalDecisionState({ ...base, expiresAt: "2026-09-28T11:59:59Z" }, ME, NOW);
  assert.equal(e.visible, false);
  assert.equal(e.reason, "expired");
});

test("a malformed reference never produces live buttons", () => {
  const s = workflowApprovalDecisionState({ ...base, approvalRef: "not-a-hash" }, ME, NOW);
  assert.equal(s.canDecide, false);
  assert.equal(s.reason, "no-reference");
});
