export const APPROVAL_DECISION_TIMEOUT_MS: number;
export const APPROVAL_UNCERTAIN_REFETCH_MS: number;
export const APPROVAL_STILL_WAITING_TEXT: string;
export type ApprovalCardPhase =
  | "idle"
  | "sending"
  | "uncertain"
  | "sent"
  | "failed";
export type ApprovalCardView = {
  mode:
    | "settled"
    | "sending"
    | "uncertain"
    | "unavailable"
    | "waiting"
    | "actions";
  buttonsDisabled: boolean;
  statusText: string;
  settledStatus?: "granted" | "denied" | "expired" | "settled";
  error: string | null;
  decision?: { token: string; candidate?: string };
};
export function settledStatusFromRelayError(
  message: string | null | undefined,
): "granted" | "denied" | "expired" | "settled" | null;
export type ApprovalDecisionUpdate = {
  phase: "sending" | "uncertain" | "sent" | "failed";
  errorMessage?: string;
};
export function trackApprovalDecision(
  request: Promise<unknown>,
  options: {
    onPhase: (update: ApprovalDecisionUpdate) => void;
    refetch?: () => unknown;
    timeoutMs?: number;
    refetchMs?: number;
  },
): {
  settled: Promise<void>;
  stopPolling: () => void;
  dispose: () => void;
};
export function approvalCardView(input: {
  approval: {
    status: string;
    expiresAt: string;
    approverSpec: string;
    approvalRef: string;
    candidateRef: string | null;
  };
  myPubkey?: string | null;
  nowMs: number;
  phase?: ApprovalCardPhase;
  action?: "grant" | "deny";
  errorMessage?: string | null;
}): ApprovalCardView;
