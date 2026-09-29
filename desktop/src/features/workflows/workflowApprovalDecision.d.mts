export type WorkflowApprovalDecisionState = {
  visible: boolean;
  canDecide: boolean;
  reason: "expired" | "decided" | "no-reference" | "approver" | "not-approver";
  decision?: { token: string; candidate?: string };
};
export function workflowApprovalDecisionState(
  approval: {
    status: string;
    expiresAt: string;
    approverSpec: string;
    approvalRef: string;
    candidateRef: string | null;
  },
  myPubkey: string | null | undefined,
  nowMs: number,
): WorkflowApprovalDecisionState;
