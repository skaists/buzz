// Pure view-state for the workflow approval card (WF-08).
//
// The relay honours a grant/deny only when it is signed by the designated
// approver, names the stored approval reference (`approval_ref`, the SHA-256
// of the minted token) in its `d` tag, and — for a candidate-bound gate —
// names the identical candidate. `approval_ref` is therefore a lookup key,
// not a bearer secret: holding it lets nobody else decide the gate.

const HEX64 = /^[0-9a-f]{64}$/i;

/** How long the card waits for the relay before it gives control back. */
export const APPROVAL_DECISION_TIMEOUT_MS = 20_000;
export const APPROVAL_TIMEOUT_MESSAGE =
  "The relay did not answer in time. Nothing was settled; try again.";

const SETTLED_LABELS = {
  granted: "Approved",
  denied: "Denied",
  expired: "Expired",
};

/**
 * Relay refusals that mean the gate is no longer open (another seat or an
 * earlier decision settled it, or it expired). Everything else leaves the
 * gate actionable.
 * @param {string | null | undefined} message
 * @returns {"granted" | "denied" | "expired" | "settled" | null}
 */
export function settledStatusFromRelayError(message) {
  const m = `${message ?? ""}`.toLowerCase();
  if (/approval token has expired|approval already expired/.test(m)) return "expired";
  if (/approval already granted/.test(m)) return "granted";
  if (/approval already denied/.test(m)) return "denied";
  if (/approval already acted on/.test(m)) return "settled";
  return null;
}

/**
 * Resolve `promise`, or reject with APPROVAL_TIMEOUT_MESSAGE after `ms`.
 * @template T
 * @param {Promise<T>} promise
 * @param {number} [ms]
 * @returns {Promise<T>}
 */
export function withApprovalTimeout(promise, ms = APPROVAL_DECISION_TIMEOUT_MS) {
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(APPROVAL_TIMEOUT_MESSAGE)), ms);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

/**
 * @param {{
 *   approval: { status: string, expiresAt: string, approverSpec: string,
 *               approvalRef: string, candidateRef: string | null },
 *   myPubkey?: string | null,
 *   nowMs: number,
 *   phase?: "idle" | "sending" | "sent" | "failed",
 *   action?: "grant" | "deny",
 *   errorMessage?: string | null,
 * }} input
 */
export function approvalCardView({ approval, myPubkey, nowMs, phase = "idle", action, errorMessage }) {
  const decision = HEX64.test(approval.approvalRef ?? "")
    ? { token: approval.approvalRef.toLowerCase(), candidate: approval.candidateRef?.trim() || undefined }
    : undefined;
  const settled = (status, detail) => ({
    mode: "settled",
    buttonsDisabled: true,
    statusText: detail ? `${SETTLED_LABELS[status] ?? "Settled"} · ${detail}` : SETTLED_LABELS[status] ?? "Settled",
    settledStatus: status,
    error: null,
    decision,
  });

  // 1. The record itself says the gate is closed (someone already settled it).
  if (approval.status in SETTLED_LABELS && approval.status !== "pending") return settled(approval.status);

  // 2. This card's own decision.
  if (phase === "sending") {
    return {
      mode: "sending",
      buttonsDisabled: true,
      statusText: action === "deny" ? "Denying…" : "Approving…",
      error: null,
      decision,
    };
  }
  if (phase === "sent") return settled(action === "deny" ? "denied" : "granted", "sent, waiting for the run");
  if (phase === "failed") {
    const closed = settledStatusFromRelayError(errorMessage);
    if (closed) return settled(closed, "already settled elsewhere");
  }

  // 3. Open gate.
  if (new Date(approval.expiresAt).getTime() < nowMs) return settled("expired");
  const error = phase === "failed" ? errorMessage || "The relay refused this decision." : null;
  if (!decision) {
    return { mode: "unavailable", buttonsDisabled: true, statusText: "This approval cannot be decided from Desktop.", error, decision };
  }
  const spec = (approval.approverSpec ?? "").trim().toLowerCase();
  const me = (myPubkey ?? "").trim().toLowerCase();
  // A hex approver spec is checked here so only the designated approver sees
  // live buttons. Any other spec form is left to the relay, which enforces it.
  const isApprover = HEX64.test(spec) ? spec === me : me.length > 0;
  if (!isApprover) {
    return { mode: "waiting", buttonsDisabled: true, statusText: "Waiting on the designated approver.", error, decision };
  }
  return { mode: "actions", buttonsDisabled: false, statusText: "Your decision is needed.", error, decision };
}
