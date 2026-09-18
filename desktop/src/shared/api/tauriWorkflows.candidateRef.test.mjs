import assert from "node:assert/strict";
import { test } from "node:test";

import { fromRawApproval } from "./tauriWorkflows.ts";

const REVIEWER =
  "755029663fee78e734a217867246e22933a4683dda19dd8435488688a29f9541";

// RED on base 7ea34f62 (and 191a577): the mapper had no `candidateRef`, so the
// WF-08 binding the relay now returns was silently dropped before it reached
// any Workflows screen. GREEN once the field is carried through.
test("fromRawApproval keeps the WF-08 candidate binding", () => {
  const bound = fromRawApproval({
    approval_ref: "cd".repeat(32),
    workflow_id: "wf",
    run_id: "run",
    step_id: "review",
    step_index: 1,
    approver_spec: REVIEWER,
    status: "pending",
    approver_pubkey: null,
    note: null,
    candidate_ref: "0123abcd",
    expires_at: "2999-01-01T00:00:00.000Z",
    created_at: 0,
  });
  assert.equal(bound.candidateRef, "0123abcd");
});

test("fromRawApproval tolerates relays that predate candidate_ref", () => {
  const legacy = fromRawApproval({
    approval_ref: "cd".repeat(32),
    workflow_id: "wf",
    run_id: "run",
    step_id: "review",
    step_index: 1,
    approver_spec: "any",
    status: "pending",
    approver_pubkey: null,
    note: null,
    expires_at: "2999-01-01T00:00:00.000Z",
    created_at: 0,
  });
  assert.equal(legacy.candidateRef, null);
});
