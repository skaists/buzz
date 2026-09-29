import { Check, X } from "lucide-react";
import * as React from "react";

import {
  fetchApprovalStatusFromRelay,
  useApprovalMutation,
  useRefreshApprovalState,
} from "@/features/workflows/hooks";
import {
  type ApprovalDecisionController,
  type ApprovalDecisionState,
  approvalCardView,
  createApprovalDecisionController,
  gateViewForKey,
  submitForKey,
} from "@/features/workflows/workflowApprovalDecision.mjs";
import { useIdentityQuery } from "@/shared/api/hooks";
import { useAppFocused } from "@/shared/lib/useDocumentVisible";
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
  const appFocused = useAppFocused();
  const [decision, setDecision] = React.useState<ApprovalDecisionState | null>(
    null,
  );
  const [, setExpiryTick] = React.useState(0);
  // The identity-plus-gate key of the attached controller (set when the
  // effect below creates it).
  const [controllerKey, setControllerKey] = React.useState<string | null>(null);
  const controllerRef = React.useRef<ApprovalDecisionController | null>(null);
  const statusRef = React.useRef<HTMLOutputElement>(null);
  const focusStatusAfterDecision = React.useRef(false);

  // Decision records are per identity and gate (and cleared on community
  // teardown), so they never leak across accounts or communities.
  const lockKey = `${(identityQuery.data?.pubkey ?? "").toLowerCase()}:${approval.approvalRef.toLowerCase()}`;
  // Until the controller for this exact key is attached, its state and its
  // buttons are not this card's: show nothing from it and keep both disabled.
  const current = controllerKey === lockKey ? decision : null;
  const view = gateViewForKey(
    approvalCardView({
      approval,
      myPubkey: identityQuery.data?.pubkey,
      nowMs: Date.now(),
      phase: current?.phase ?? "idle",
      action: current?.action,
      errorMessage: current?.errorMessage ?? null,
      lockedAction: current?.lockedAction ?? null,
      settledStatus: current?.settledStatus ?? null,
    }),
    controllerKey,
    lockKey,
  );

  // The controller is created once per gate and reads the latest values
  // through this ref, so its fencing and locks survive re-renders.
  const latest = React.useRef({
    approval,
    decision: view.decision,
    mutateAsync: approvalMutation.mutateAsync,
    refresh: refreshApprovalState,
    appFocused,
  });
  latest.current = {
    approval,
    decision: view.decision,
    mutateAsync: approvalMutation.mutateAsync,
    refresh: refreshApprovalState,
    appFocused,
  };

  React.useEffect(() => {
    const controller = createApprovalDecisionController({
      lockKey,
      send: ({ action }) => {
        const target = latest.current.decision;
        if (!target)
          return Promise.reject(
            new Error("This approval cannot be decided from Desktop."),
          );
        return latest.current.mutateAsync({
          token: target.token,
          candidate: target.candidate,
          action,
        });
      },
      // Verified read from the relay, not the query cache.
      verify: () => fetchApprovalStatusFromRelay(latest.current.approval),
      refetch: () => latest.current.refresh(),
      isActive: () => latest.current.appFocused,
      onChange: setDecision,
    });
    controllerRef.current = controller;
    setDecision(controller.getState());
    setControllerKey(lockKey);
    return () => {
      controller.dispose();
      if (controllerRef.current === controller) controllerRef.current = null;
    };
  }, [lockKey]);

  // Audit 3: once the outcome is known, focus lands on the card's status
  // line, whether the controller or the rendered gate record settled first.
  const phase = current?.phase ?? "idle";
  const showsSettled = view.mode === "settled";
  const outcomeKnown =
    showsSettled ||
    phase === "sent" ||
    phase === "failed" ||
    phase === "settled";
  React.useEffect(() => {
    if (outcomeKnown && focusStatusAfterDecision.current) {
      focusStatusAfterDecision.current = false;
      statusRef.current?.focus();
    }
  }, [outcomeKnown]);

  // The card shows a settled gate (seen from the relay, or expired) while a
  // decision is still unresolved: stop re-reading the gate.
  React.useEffect(() => {
    if (showsSettled) controllerRef.current?.stopPolling();
  }, [showsSettled]);

  // An unresolved decision on a gate that then expires must not stay busy:
  // re-render at expiry so the card shows Expired.
  const expiresAtMs = new Date(approval.expiresAt).getTime();
  React.useEffect(() => {
    if (phase !== "uncertain" && phase !== "verifying") return undefined;
    const wait = expiresAtMs - Date.now();
    if (!(wait > 0)) return undefined;
    const timer = setTimeout(
      () => setExpiryTick((tick) => tick + 1),
      Math.min(wait + 50, 2_147_483_647),
    );
    return () => clearTimeout(timer);
  }, [phase, expiresAtMs]);

  const decide = (next: Decision) => {
    if (view.disabledActions[next] || !view.decision) return;
    // submitForKey ignores the click unless the attached controller was
    // created for the current identity-plus-gate key. The controller's own
    // synchronous phase check blocks a second click in the same frame, and
    // any signing the relay has not yet verified as safe.
    focusStatusAfterDecision.current = true;
    if (submitForKey(controllerRef.current, lockKey, next) === null)
      focusStatusAfterDecision.current = false;
  };

  const busy =
    view.mode === "sending" ||
    view.mode === "uncertain" ||
    view.mode === "verifying";
  const showButtons = view.mode === "actions" || busy;
  const action = current?.action;

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
          aria-busy={busy}
        >
          <Button
            type="button"
            size="sm"
            disabled={view.disabledActions.grant}
            onClick={() => decide("grant")}
            data-testid="workflow-approval-approve"
          >
            <Check aria-hidden="true" />
            {busy && action === "grant" ? "Approving…" : "Approve"}
          </Button>
          <Button
            type="button"
            size="sm"
            variant="destructive"
            disabled={view.disabledActions.deny}
            onClick={() => decide("deny")}
            data-testid="workflow-approval-deny"
          >
            <X aria-hidden="true" />
            {busy && action === "deny" ? "Denying…" : "Deny"}
          </Button>
        </div>
      ) : null}
    </div>
  );
}
