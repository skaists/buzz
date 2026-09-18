import type { TraceEntry, WorkflowApproval } from "@/shared/api/types";

/**
 * A `request_approval` gate as the run trace + approvals API describe it
 * (WF-08). Derived, never stored: the trace entry's `output` carries what the
 * relay wrote at mint (`approval_ref`, `approver_spec`, `candidate_ref`,
 * `requested_at`, `expires_at`) and, once decided, the reviewer's evidence
 * (`decision`, `approver`, `note`, `decision_event_id`, `decided_at`,
 * `reasserted`). The approvals API row, when present, fills the same fields
 * for relays that have not yet written evidence into the trace.
 */
export type ReviewGateState = "waiting" | "granted" | "denied" | "expired";

export type ReviewGate = {
  state: ReviewGateState;
  /** `"any"` or a 64-char hex pubkey — who may decide. */
  reviewerSpec: string;
  /** Hex pubkey of whoever actually decided, once decided. */
  decidedBy: string | null;
  /** Opaque candidate the gate is bound to (e.g. a commit sha), if any. */
  candidateRef: string | null;
  note: string | null;
  /** Event id of the signed grant/deny that decided the gate. */
  decisionEventId: string | null;
  /** SHA-256 hex of the approval token — a correlation id, never actionable. */
  approvalRef: string | null;
  requestedAt: string | null;
  decidedAt: string | null;
  expiresAt: string | null;
  /** True when a re-sent identical decision completed an interrupted apply. */
  reasserted: boolean;
};

function str(value: unknown): string | null {
  if (typeof value !== "string") return null;
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : null;
}

function isGateOutput(output: Record<string, unknown>): boolean {
  return (
    typeof output.approval_ref === "string" ||
    typeof output.approver_spec === "string" ||
    typeof output.decision === "string"
  );
}

/**
 * Describe the review gate a trace step represents, or `null` when the step
 * is not a gate. Pure; safe to call for every step of every run.
 */
export function describeReviewGate(
  step: TraceEntry,
  approval?: WorkflowApproval | null,
  now: Date = new Date(),
): ReviewGate | null {
  const output = step.output ?? {};
  const isWaitingStatus = step.status === "waiting_approval";
  const isDeniedStatus = step.status === "denied";
  const matchesApproval = approval != null && approval.stepId === step.stepId;

  if (
    !isWaitingStatus &&
    !isDeniedStatus &&
    !isGateOutput(output) &&
    !matchesApproval
  ) {
    return null;
  }

  const decision = str(output.decision);
  const approvalStatus = matchesApproval ? approval.status : null;
  const expiresAt =
    str(output.expires_at) ?? (matchesApproval ? approval.expiresAt : null);

  let state: ReviewGateState;
  if (decision === "granted" || approvalStatus === "granted") {
    state = "granted";
  } else if (
    decision === "denied" ||
    approvalStatus === "denied" ||
    isDeniedStatus
  ) {
    state = "denied";
  } else if (
    approvalStatus === "expired" ||
    (expiresAt !== null && new Date(expiresAt) < now)
  ) {
    state = "expired";
  } else {
    state = "waiting";
  }

  return {
    state,
    reviewerSpec:
      str(output.approver_spec) ??
      (matchesApproval ? approval.approverSpec : null) ??
      "any",
    decidedBy:
      str(output.approver) ??
      (matchesApproval ? approval.approverPubkey : null),
    candidateRef:
      str(output.candidate_ref) ??
      (matchesApproval ? approval.candidateRef : null),
    note: str(output.note) ?? (matchesApproval ? approval.note : null),
    decisionEventId: str(output.decision_event_id),
    approvalRef:
      str(output.approval_ref) ??
      (matchesApproval ? approval.approvalRef : null),
    requestedAt: str(output.requested_at),
    decidedAt: str(output.decided_at),
    expiresAt,
    reasserted: output.reasserted === true,
  };
}

/** Short, human label for a reviewer spec or pubkey (hex → first/last 4). */
export function shortPubkey(value: string | null): string | null {
  if (value === null) return null;
  if (value === "any") return "any member";
  if (/^[0-9a-f]{64}$/i.test(value)) {
    return `${value.slice(0, 8)}…${value.slice(-4)}`;
  }
  return value;
}
