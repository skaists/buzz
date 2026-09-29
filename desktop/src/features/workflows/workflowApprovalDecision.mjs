// Pure decision-state for the workflow approval card (WF-08).
//
// The relay honours a grant/deny only when it is signed by the designated
// approver, names the stored approval reference (`approval_ref`, the SHA-256
// of the minted token) in its `d` tag, and — for a candidate-bound gate —
// names the identical candidate. `approval_ref` is therefore a lookup key,
// not a bearer secret: holding it lets nobody else decide the gate.

const HEX64 = /^[0-9a-f]{64}$/i;

/**
 * @param {{ status: string, expiresAt: string, approverSpec: string,
 *           approvalRef: string, candidateRef: string | null }} approval
 * @param {string | null | undefined} myPubkey hex pubkey of the signed-in identity
 * @param {number} nowMs
 */
export function workflowApprovalDecisionState(approval, myPubkey, nowMs) {
  const expired = new Date(approval.expiresAt).getTime() < nowMs;
  if (approval.status !== "pending" || expired) {
    return { visible: false, canDecide: false, reason: expired ? "expired" : "decided" };
  }
  if (!HEX64.test(approval.approvalRef ?? "")) {
    return { visible: true, canDecide: false, reason: "no-reference" };
  }
  const spec = (approval.approverSpec ?? "").trim().toLowerCase();
  const me = (myPubkey ?? "").trim().toLowerCase();
  // A hex approver spec is checked here so only the designated approver sees
  // live buttons. Any other spec form is left to the relay, which enforces it.
  const isApprover = HEX64.test(spec) ? spec === me : me.length > 0;
  return {
    visible: true,
    canDecide: isApprover,
    reason: isApprover ? "approver" : "not-approver",
    decision: {
      token: approval.approvalRef.toLowerCase(),
      candidate: approval.candidateRef?.trim() || undefined,
    },
  };
}
