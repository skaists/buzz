export const APPROVAL_DECISION_TIMEOUT_MS: number;
export const APPROVAL_TIMEOUT_MESSAGE: string;
export type ApprovalCardPhase = "idle" | "sending" | "sent" | "failed";
export type ApprovalCardView = {
  mode: "settled" | "sending" | "unavailable" | "waiting" | "actions";
  buttonsDisabled: boolean;
  statusText: string;
  settledStatus?: "granted" | "denied" | "expired" | "settled";
  error: string | null;
  decision?: { token: string; candidate?: string };
};
export function settledStatusFromRelayError(
  message: string | null | undefined,
): "granted" | "denied" | "expired" | "settled" | null;
export function withApprovalTimeout<T>(promise: Promise<T>, ms?: number): Promise<T>;
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
