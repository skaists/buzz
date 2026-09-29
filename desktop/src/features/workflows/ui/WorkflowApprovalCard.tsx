import { Check, X } from "lucide-react";
import * as React from "react";

import {
  useApprovalMutation,
  useRefreshApprovalState,
} from "@/features/workflows/hooks";
import {
  type ApprovalCardPhase,
  approvalCardView,
  trackApprovalDecision,
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
  const refreshApprovalState = useRefreshApprovalState();
  const [phase, setPhase] = React.useState<ApprovalCardPhase>("idle");
  const [action, setAction] = React.useState<Decision | undefined>();
  const [errorMessage, setErrorMessage] = React.useState<string | null>(null);
  const [, setExpiryTick] = React.useState(0);
  // Synchronous guard: a second click in the same frame, before React has
  // re-rendered the disabled buttons, must not send a second decision. It is
  // released only when the relay actually answers, never by the deadline.
  const inFlight = React.useRef(false);
  const tracker = React.useRef<ReturnType<typeof trackApprovalDecision> | null>(
    null,
  );
  const statusRef = React.useRef<HTMLOutputElement>(null);
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

  // Stop timers if the card goes away mid-decision.
  React.useEffect(() => () => tracker.current?.dispose(), []);

  // The card shows a settled gate (seen from the relay, or expired) while our
  // submit is still overdue: stop re-reading the gate.
  const showsSettled = view.mode === "settled";
  React.useEffect(() => {
    if (showsSettled) tracker.current?.stopPolling();
  }, [showsSettled]);

  // An overdue decision on a gate that then expires must not stay busy:
  // re-render at expiry so the card shows Expired.
  const expiresAtMs = new Date(approval.expiresAt).getTime();
  React.useEffect(() => {
    if (phase !== "uncertain") return undefined;
    const wait = expiresAtMs - Date.now();
    if (!(wait > 0)) return undefined;
    const timer = setTimeout(
      () => setExpiryTick((tick) => tick + 1),
      Math.min(wait + 50, 2_147_483_647),
    );
    return () => clearTimeout(timer);
  }, [phase, expiresAtMs]);

  const decide = (next: Decision) => {
    if (inFlight.current || view.buttonsDisabled || !view.decision) return;
    inFlight.current = true;
    focusStatusAfterDecision.current = true;
    setAction(next);
    setErrorMessage(null);
    const request = approvalMutation.mutateAsync({
      token: view.decision.token,
      candidate: view.decision.candidate,
      action: next,
    });
    tracker.current = trackApprovalDecision(request, {
      refetch: refreshApprovalState,
      onPhase: (update) => {
        if (update.phase === "sent" || update.phase === "failed") {
          inFlight.current = false;
          tracker.current = null;
        }
        if (update.phase === "failed")
          setErrorMessage(update.errorMessage ?? null);
        setPhase(update.phase);
      },
    });
  };

  const showButtons =
    view.mode === "actions" ||
    view.mode === "sending" ||
    view.mode === "uncertain";

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
      <output
        ref={statusRef}
        tabIndex={-1}
        aria-live="polite"
        className="mb-2 block rounded text-xs font-medium focus-visible:outline-hidden focus-visible:ring-1 focus-visible:ring-ring"
        data-testid="workflow-approval-status"
      >
        {view.statusText}
        {view.error ? (
          <span
            className="mt-1 block font-normal text-destructive"
            role="alert"
            data-testid="workflow-approval-error"
          >
            {view.error}
          </span>
        ) : null}
      </output>
      {showButtons ? (
        // Plain buttons, no <form>: Enter never submits a decision by default,
        // and nothing is auto-focused.
        <div
          className="flex flex-wrap gap-2"
          data-testid="workflow-approval-actions"
          aria-busy={view.mode === "sending" || view.mode === "uncertain"}
        >
          <Button
            type="button"
            size="sm"
            disabled={view.buttonsDisabled}
            onClick={() => decide("grant")}
            data-testid="workflow-approval-approve"
          >
            <Check aria-hidden="true" />
            {view.mode !== "actions" && action === "grant"
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
            {view.mode !== "actions" && action === "deny" ? "Denying…" : "Deny"}
          </Button>
        </div>
      ) : null}
    </div>
  );
}
