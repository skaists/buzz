import { Check, X } from "lucide-react";
import * as React from "react";

import { useApprovalMutation } from "@/features/workflows/hooks";
import {
  type ApprovalCardPhase,
  approvalCardView,
} from "@/features/workflows/workflowApprovalDecision.mjs";
import { useIdentityQuery } from "@/shared/api/hooks";
import type { WorkflowApproval } from "@/shared/api/types";
import { Button } from "@/shared/ui/button";

type WorkflowApprovalCardProps = {
  approval: WorkflowApproval;
};

type Decision = "grant" | "deny";

export function WorkflowApprovalCard({ approval }: WorkflowApprovalCardProps) {
  const identityQuery = useIdentityQuery();
  const approvalMutation = useApprovalMutation();
  const [phase, setPhase] = React.useState<ApprovalCardPhase>("idle");
  const [action, setAction] = React.useState<Decision | undefined>();
  const [errorMessage, setErrorMessage] = React.useState<string | null>(null);
  // Synchronous guard: a second click in the same frame, before React has
  // re-rendered the disabled buttons, must not send a second decision.
  const inFlight = React.useRef(false);
  const statusRef = React.useRef<HTMLDivElement>(null);
  const focusStatusAfterDecision = React.useRef(false);

  const view = approvalCardView({
    approval,
    myPubkey: identityQuery.data?.pubkey,
    nowMs: Date.now(),
    phase,
    action,
    errorMessage,
  });

  // Audit 3: after the relay answers, focus lands on the card's status line.
  React.useEffect(() => {
    if (phase === "sent" || phase === "failed") {
      if (focusStatusAfterDecision.current) {
        focusStatusAfterDecision.current = false;
        statusRef.current?.focus();
      }
    }
  }, [phase]);

  const decide = (next: Decision) => {
    if (inFlight.current || view.buttonsDisabled || !view.decision) return;
    inFlight.current = true;
    focusStatusAfterDecision.current = true;
    setAction(next);
    setErrorMessage(null);
    setPhase("sending");
    approvalMutation.mutate(
      {
        token: view.decision.token,
        candidate: view.decision.candidate,
        action: next,
      },
      {
        onSuccess: () => setPhase("sent"),
        onError: (error) => {
          setErrorMessage(
            error instanceof Error ? error.message : String(error),
          );
          setPhase("failed");
        },
        onSettled: () => {
          inFlight.current = false;
        },
      },
    );
  };

  const showButtons = view.mode === "actions" || view.mode === "sending";

  return (
    <div
      className="rounded-lg border border-amber-500/30 bg-amber-500/5 p-3"
      data-testid="workflow-approval-card"
      data-approval-mode={view.mode}
    >
      <p className="mb-2 text-sm font-medium">
        {view.mode === "settled" ? "Approval" : "Approval Required"}
      </p>
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
      <div
        ref={statusRef}
        tabIndex={-1}
        role="status"
        aria-live="polite"
        className="mb-2 rounded text-xs font-medium focus-visible:outline-hidden focus-visible:ring-1 focus-visible:ring-ring"
        data-testid="workflow-approval-status"
      >
        {view.statusText}
        {view.error ? (
          <p
            className="mt-1 font-normal text-destructive"
            role="alert"
            data-testid="workflow-approval-error"
          >
            {view.error}
          </p>
        ) : null}
      </div>
      {showButtons ? (
        // Plain buttons, no <form>: Enter never submits a decision by default,
        // and nothing is auto-focused.
        <div
          className="flex flex-wrap gap-2"
          data-testid="workflow-approval-actions"
          aria-busy={view.mode === "sending"}
        >
          <Button
            type="button"
            size="sm"
            disabled={view.buttonsDisabled}
            onClick={() => decide("grant")}
            data-testid="workflow-approval-approve"
          >
            <Check aria-hidden="true" />
            {view.mode === "sending" && action === "grant"
              ? "Approving…"
              : "Approve"}
          </Button>
          <Button
            type="button"
            size="sm"
            variant="destructive"
            disabled={view.buttonsDisabled}
            onClick={() => decide("deny")}
            data-testid="workflow-approval-deny"
          >
            <X aria-hidden="true" />
            {view.mode === "sending" && action === "deny" ? "Denying…" : "Deny"}
          </Button>
        </div>
      ) : null}
    </div>
  );
}
