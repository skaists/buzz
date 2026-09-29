export const APPROVAL_DECISION_TIMEOUT_MS: number;
export const APPROVAL_UNCERTAIN_REFETCH_MS: number;
export const APPROVAL_VERIFY_READ_TIMEOUT_MS: number;
export const APPROVAL_STILL_WAITING_TEXT: string;
export const APPROVAL_VERIFYING_TEXT: string;
export type ApprovalDecisionAction = "grant" | "deny";
export type ApprovalCardPhase =
  | "idle"
  | "sending"
  | "uncertain"
  | "verifying"
  | "sent"
  | "failed"
  | "settled";
export type ApprovalCardView = {
  mode:
    | "settled"
    | "sending"
    | "uncertain"
    | "verifying"
    | "unavailable"
    | "waiting"
    | "actions";
  buttonsDisabled: boolean;
  disabledActions: { grant: boolean; deny: boolean };
  statusText: string;
  settledStatus?: string;
  error: string | null;
  decision?: { token: string; candidate?: string };
};
export function settledStatusFromRelayError(
  message: string | null | undefined,
): "granted" | "denied" | "expired" | "settled" | null;
export function isDefinitiveRelayRefusal(
  message: string | null | undefined,
): boolean;
export const UNCERTAIN_DECISION_LOCKS: Map<string, ApprovalDecisionAction>;
export function approvalLockKey(input: {
  communityId?: string | null;
  pubkey?: string | null;
  approvalRef: string;
}): string;
export const IN_FLIGHT_DECISION_SUBMITS: Map<string, number>;
export function resetUncertainDecisionLocks(
  locks?: Map<string, ApprovalDecisionAction>,
  inFlight?: Map<string, number>,
): void;
export type ApprovalDecisionState = {
  attempt: number;
  phase: ApprovalCardPhase;
  action?: ApprovalDecisionAction;
  errorMessage: string | null;
  lockedAction: ApprovalDecisionAction | null;
  settledStatus: string | null;
};
export type ApprovalDecisionController = {
  lockKey: string | null;
  submit: (action: ApprovalDecisionAction) => number | null;
  canSubmit: (action: ApprovalDecisionAction) => boolean;
  getState: () => ApprovalDecisionState;
  stopPolling: () => void;
  dispose: () => void;
};
export function createApprovalDecisionController(options: {
  send: (input: {
    action: ApprovalDecisionAction;
    attempt: number;
  }) => Promise<unknown>;
  verify: () => Promise<string>;
  onChange: (state: ApprovalDecisionState) => void;
  refetch?: () => unknown;
  lockKey?: string;
  locks?: Map<string, ApprovalDecisionAction>;
  inFlight?: Map<string, number>;
  timeoutMs?: number;
  refetchMs?: number;
  verifyTimeoutMs?: number;
  isActive?: () => boolean;
}): ApprovalDecisionController;
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
  action?: ApprovalDecisionAction;
  errorMessage?: string | null;
  lockedAction?: ApprovalDecisionAction | null;
  settledStatus?: string | null;
}): ApprovalCardView;
export function submitForKey(
  controller:
    | Pick<ApprovalDecisionController, "lockKey" | "submit">
    | null
    | undefined,
  currentKey: string,
  action: ApprovalDecisionAction,
): number | null;
export function gateViewForKey<
  V extends {
    buttonsDisabled: boolean;
    disabledActions: { grant: boolean; deny: boolean };
  },
>(view: V, controllerKey: string | null, currentKey: string): V;
