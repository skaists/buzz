import assert from "node:assert/strict";
import test from "node:test";

import {
  approvalCardView,
  settledStatusFromRelayError,
} from "./workflowApprovalDecision.mjs";

const ME = "56694530e53104c824408896ebb78751de7d2894800308369f463feff4a8bd27";
const OTHER =
  "755029663fee78e734a217867246e22933a4683dda19dd8435488688a29f9541";
const REF = "AB".repeat(32);
const NOW = Date.parse("2026-09-28T12:00:00Z");
const approval = {
  status: "pending",
  expiresAt: "2026-09-28T13:00:00Z",
  approverSpec: ME,
  approvalRef: REF,
  candidateRef: " commitA ",
};
const view = (over = {}) =>
  approvalCardView({ approval, myPubkey: ME, nowMs: NOW, ...over });

test("designated approver gets live buttons bound to ref + candidate", () => {
  const v = view({ myPubkey: ME.toUpperCase() });
  assert.equal(v.mode, "actions");
  assert.equal(v.buttonsDisabled, false);
  assert.deepEqual(v.decision, {
    token: REF.toLowerCase(),
    candidate: "commitA",
  });
});

test("anyone else sees the card but no live buttons", () => {
  const v = view({ myPubkey: OTHER });
  assert.equal(v.mode, "waiting");
  assert.equal(v.buttonsDisabled, true);
});

test("unbound gate sends no candidate", () => {
  assert.equal(
    view({ approval: { ...approval, candidateRef: null } }).decision.candidate,
    undefined,
  );
});

test("audit 1: both buttons stay disabled from click until the relay answers", () => {
  for (const action of ["grant", "deny"]) {
    const v = view({ phase: "sending", action });
    assert.equal(v.mode, "sending");
    assert.equal(v.buttonsDisabled, true);
    assert.equal(v.statusText, action === "deny" ? "Denying…" : "Approving…");
  }
});

test("audit 1: a gate another seat already settled shows that state, not buttons", () => {
  for (const [status, label] of [
    ["granted", "Approved"],
    ["denied", "Denied"],
    ["expired", "Expired"],
  ]) {
    const v = view({ approval: { ...approval, status } });
    assert.equal(v.mode, "settled");
    assert.equal(v.buttonsDisabled, true);
    assert.equal(v.statusText, label);
  }
  const race = view({
    phase: "failed",
    action: "grant",
    errorMessage: "relay error 400: invalid: approval already denied",
  });
  assert.equal(race.mode, "settled");
  assert.equal(race.settledStatus, "denied");
  assert.match(race.statusText, /already settled elsewhere/);
});

test("audit 1: after the relay accepts, the card is settled (no second decision)", () => {
  const v = view({ phase: "sent", action: "deny" });
  assert.equal(v.mode, "settled");
  assert.equal(v.buttonsDisabled, true);
  assert.match(v.statusText, /^Denied/);
});

test("audit 4: a relay refusal reverts to actionable with the inline error", () => {
  const v = view({
    phase: "failed",
    action: "grant",
    errorMessage: "forbidden: candidate mismatch",
  });
  assert.equal(v.mode, "actions");
  assert.equal(v.buttonsDisabled, false);
  assert.equal(v.error, "forbidden: candidate mismatch");
});

test("relay settle messages are classified; other refusals are not", () => {
  assert.equal(
    settledStatusFromRelayError("invalid: approval already granted"),
    "granted",
  );
  assert.equal(
    settledStatusFromRelayError("invalid: approval already acted on (race)"),
    "settled",
  );
  assert.equal(
    settledStatusFromRelayError("invalid: approval token has expired"),
    "expired",
  );
  assert.equal(
    settledStatusFromRelayError("forbidden: not the designated approver"),
    null,
  );
  assert.equal(settledStatusFromRelayError(undefined), null);
});

test("an open gate past its expiry shows Expired", () => {
  const v = view({
    approval: { ...approval, expiresAt: "2026-09-28T11:59:59Z" },
  });
  assert.equal(v.mode, "settled");
  assert.equal(v.statusText, "Expired");
});

test("a malformed reference never produces live buttons", () => {
  const v = view({ approval: { ...approval, approvalRef: "not-a-hash" } });
  assert.equal(v.mode, "unavailable");
  assert.equal(v.buttonsDisabled, true);
});
