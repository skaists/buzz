import { useApprovalMutation } from "@/features/workflows/hooks";
import { workflowApprovalDecisionState } from "@/features/workflows/workflowApprovalDecision.mjs";
import { useIdentityQuery } from "@/shared/api/hooks";
import type { WorkflowApproval } from "@/shared/api/types";
import { Button } from "@/shared/ui/button";

type WorkflowApprovalCardProps = {
  approval: WorkflowApproval;
};

export function WorkflowApprovalCard({ approval }: WorkflowApprovalCardProps) {
  const identityQuery = useIdentityQuery();
  const approvalMutation = useApprovalMutation();
  const state = workflowApprovalDecisionState(
    approval,
    identityQuery.data?.pubkey,
    Date.now(),
  );

  if (!state.visible) {
    return null;
  }

  const decide = (action: "grant" | "deny") => {
    if (!state.decision) return;
    approvalMutation.mutate({
      token: state.decision.token,
      candidate: state.decision.candidate,
      action,
    });
  };
  const pendingAction = approvalMutation.isPending
    ? approvalMutation.variables?.action
    : undefined;

  return (
    <div
      className="rounded-lg border border-amber-500/30 bg-amber-500/5 p-3"
      data-testid="workflow-approval-card"
    >
      <p className="mb-2 text-sm font-medium">Approval Required</p>
      <p className="mb-2 text-xs text-muted-foreground">
        Approver: {approval.approverSpec}
      </p>
      {approval.candidateRef ? (
        <p
          className="mb-2 font-mono text-xs text-muted-foreground"
          data-testid="workflow-approval-candidate"
        >
          Candidate: {approval.candidateRef}
        </p>
      ) : null}
      <p className="mb-2 text-xs text-muted-foreground">
        Expires: {new Date(approval.expiresAt).toLocaleString()}
      </p>
      {state.canDecide ? (
        <div className="flex gap-2" data-testid="workflow-approval-actions">
          <Button
            type="button"
            size="sm"
            disabled={approvalMutation.isPending}
            onClick={() => decide("grant")}
            data-testid="workflow-approval-approve"
          >
            {pendingAction === "grant" ? "Approving…" : "Approve"}
          </Button>
          <Button
            type="button"
            size="sm"
            variant="outline"
            disabled={approvalMutation.isPending}
            onClick={() => decide("deny")}
            data-testid="workflow-approval-deny"
          >
            {pendingAction === "deny" ? "Denying…" : "Deny"}
          </Button>
        </div>
      ) : (
        <p className="text-xs text-muted-foreground" role="status">
          {state.reason === "no-reference"
            ? "This approval cannot be decided from Desktop."
            : "Waiting on the designated approver."}
        </p>
      )}
      {approvalMutation.isError ? (
        <p
          className="mt-2 text-xs text-destructive"
          role="alert"
          data-testid="workflow-approval-error"
        >
          {approvalMutation.error instanceof Error
            ? approvalMutation.error.message
            : String(approvalMutation.error)}
        </p>
      ) : null}
    </div>
  );
}
