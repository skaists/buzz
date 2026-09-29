// Pure view-state for the workflow approval card (WF-08).
//
// The relay honours a grant/deny only when it is signed by the designated
// approver, names the stored approval reference (`approval_ref`, the SHA-256
// of the minted token) in its `d` tag, and — for a candidate-bound gate —
// names the identical candidate. `approval_ref` is therefore a lookup key,
// not a bearer secret: holding it lets nobody else decide the gate.

const HEX64 = /^[0-9a-f]{64}$/i;

/** How long the card waits for the relay before it says so on the card. */
export const APPROVAL_DECISION_TIMEOUT_MS = 20_000;
/** While an answer is overdue, how often the card re-reads the gate. */
export const APPROVAL_UNCERTAIN_REFETCH_MS = 3_000;
export const APPROVAL_STILL_WAITING_TEXT = "Still waiting for the relay…";
export const APPROVAL_VERIFYING_TEXT =
  "Checking the gate with the relay before another decision…";

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
  if (/approval token has expired|approval already expired/.test(m))
    return "expired";
  if (/approval already granted/.test(m)) return "granted";
  if (/approval already denied/.test(m)) return "denied";
  if (/approval already acted on/.test(m)) return "settled";
  return null;
}

/**
 * A relay answer that proves this signed decision was refused and can never be
 * accepted later ("relay rejected event: …" is the relay's own accepted=false
 * reply). Transport errors, gateway errors and the Rust-side abandon are not
 * definitive: the decision may or may not have reached the relay.
 * @param {string | null | undefined} message
 */
export function isDefinitiveRelayRefusal(message) {
  const m = `${message ?? ""}`.toLowerCase();
  return m.startsWith("relay rejected event:");
}

/**
 * Per-session record of decisions whose outcome was ever unknown, keyed by
 * approval reference. Survives card remounts, so a reload of the card cannot
 * be used to sign the opposite decision.
 * @type {Map<string, "grant" | "deny">}
 */
export const UNCERTAIN_DECISION_LOCKS = new Map();

const ALL_OFF = Object.freeze({ grant: true, deny: true });

/**
 * Drives one approval card's decisions. Guarantees:
 *
 * - Cancellation: Tauri's `invoke` takes no AbortSignal, so a submit cannot be
 *   cancelled from here. The Rust command drops its own HTTP request after
 *   15 s (`APPROVAL_SUBMIT_DEADLINE`). This controller never pretends to cancel:
 *   its 20 s deadline only changes what the card shows.
 * - Epoch fencing: every attempt gets a monotonic attempt id. Every async
 *   callback (submit result, verify result, timers) is dropped unless it
 *   belongs to the current attempt, so a late answer from an abandoned attempt
 *   can never change state or overwrite a newer attempt.
 * - No double-sign: when the outcome is unknown (the 20 s deadline passed, or
 *   the submit failed without a definitive relay refusal), no second signing
 *   is allowed until `verify()` has read the gate from the relay and found it
 *   still pending. After a non-definitive failure only the same decision may
 *   be signed again, so Approve followed by Deny cannot happen.
 *
 * @param {{
 *   send: (input: { action: "grant" | "deny", attempt: number }) => Promise<unknown>,
 *   verify: () => Promise<string>,
 *   onChange: (state: ApprovalDecisionState) => void,
 *   refetch?: () => unknown,
 *   lockKey?: string,
 *   locks?: Map<string, "grant" | "deny">,
 *   timeoutMs?: number,
 *   refetchMs?: number,
 * }} options
 *
 * @typedef {{
 *   attempt: number,
 *   phase: "idle" | "sending" | "uncertain" | "verifying" | "sent" | "failed" | "settled",
 *   action?: "grant" | "deny",
 *   errorMessage: string | null,
 *   lockedAction: "grant" | "deny" | null,
 *   settledStatus: string | null,
 * }} ApprovalDecisionState
 */
export function createApprovalDecisionController({
  send,
  verify,
  onChange,
  refetch,
  lockKey,
  locks = UNCERTAIN_DECISION_LOCKS,
  timeoutMs = APPROVAL_DECISION_TIMEOUT_MS,
  refetchMs = APPROVAL_UNCERTAIN_REFETCH_MS,
}) {
  /** @type {ApprovalDecisionState} */
  let state = {
    attempt: 0,
    phase: "idle",
    action: undefined,
    errorMessage: null,
    lockedAction: (lockKey && locks.get(lockKey)) || null,
    settledStatus: null,
  };
  let disposed = false;
  let pollStopped = false;
  let deadline = null;
  let poll = null;
  // Each poll has a generation; a verify answer is honoured only for the
  // current attempt AND the current poll generation.
  let pollGeneration = 0;
  let verifyInFlight = 0;
  let refreshing = false;

  const set = (patch) => {
    state = { ...state, ...patch };
    onChange(state);
  };
  const current = (attempt) => !disposed && attempt === state.attempt;
  const clearTimers = () => {
    if (deadline !== null) clearTimeout(deadline);
    if (poll !== null) clearInterval(poll);
    deadline = null;
    poll = null;
    // Retire the current poll: its in-flight verify answer is fenced off.
    pollGeneration += 1;
  };

  // Refresh the card's cached view of the gate (one read at a time).
  const refreshCache = () => {
    if (!refetch || refreshing || pollStopped) return;
    refreshing = true;
    let pending;
    try {
      pending = refetch();
    } catch {
      pending = undefined;
    }
    Promise.resolve(pending)
      .catch(() => {})
      .finally(() => {
        refreshing = false;
      });
  };

  // Verified read of the gate from the relay, fenced to `attempt` and to the
  // poll generation. `settleOnly`: the submit is still unresolved, so a pending
  // gate must not re-enable anything (the decision may still land).
  const verifyOnce = (attempt, generation, settleOnly) => {
    if (verifyInFlight === generation || !current(attempt)) return;
    verifyInFlight = generation;
    let read;
    try {
      read = Promise.resolve(verify());
    } catch (error) {
      read = Promise.reject(error);
    }
    const fenced = () => !current(attempt) || generation !== pollGeneration;
    read.then(
      (status) => {
        if (verifyInFlight === generation) verifyInFlight = 0;
        if (fenced()) return;
        const s = `${status ?? ""}`.toLowerCase();
        if (s === "granted" || s === "denied" || s === "expired") {
          clearTimers();
          set({ phase: "settled", settledStatus: s });
        } else if (s === "pending" && !settleOnly) {
          // Verified still open: a second signing is now allowed.
          clearTimers();
          set({ phase: "failed" });
        }
        // Anything else (unknown status) is not a verification: keep waiting.
      },
      () => {
        if (verifyInFlight === generation) verifyInFlight = 0;
        // Not verified; the poll retries.
      },
    );
  };

  const startPolling = (attempt, settleOnly) => {
    if (poll !== null) clearInterval(poll);
    pollGeneration += 1;
    // The card already shows the gate settled: nothing left to find out.
    if (pollStopped) return;
    const generation = pollGeneration;
    const tick = () => {
      if (!current(attempt) || generation !== pollGeneration) return;
      refreshCache();
      verifyOnce(attempt, generation, settleOnly);
    };
    tick();
    poll = setInterval(tick, refetchMs);
  };

  const view = () => state;

  /** @param {"grant" | "deny"} action */
  const canSubmit = (action) => {
    if (disposed) return false;
    if (state.phase !== "idle" && state.phase !== "failed") return false;
    if (state.lockedAction && state.lockedAction !== action) return false;
    return true;
  };

  /** @param {"grant" | "deny"} action @returns {number | null} attempt id */
  const submit = (action) => {
    if (!canSubmit(action)) return null;
    clearTimers();
    pollStopped = false;
    const attempt = state.attempt + 1;
    let transportDone = false;
    let timedOut = false;
    set({
      attempt,
      phase: "sending",
      action,
      errorMessage: null,
      settledStatus: null,
    });

    deadline = setTimeout(() => {
      if (!current(attempt) || transportDone) return;
      timedOut = true;
      set({ phase: "uncertain" });
      // The submit is unresolved: reads may settle the card, never re-enable.
      startPolling(attempt, true);
    }, timeoutMs);

    let request;
    try {
      request = Promise.resolve(send({ action, attempt }));
    } catch (error) {
      request = Promise.reject(error);
    }
    request.then(
      () => {
        transportDone = true;
        if (!current(attempt)) return;
        clearTimers();
        set({ phase: "sent" });
      },
      (error) => {
        transportDone = true;
        if (!current(attempt)) return;
        const message = error instanceof Error ? error.message : String(error);
        if (settledStatusFromRelayError(message)) {
          // The relay says the gate is already closed: show that.
          clearTimers();
          set({ phase: "failed", errorMessage: message });
          return;
        }
        const definitive = isDefinitiveRelayRefusal(message);
        if (definitive && !timedOut) {
          clearTimers();
          set({ phase: "failed", errorMessage: message });
          return;
        }
        // Unknown outcome: lock to this decision unless the relay refused it,
        // and allow nothing until a verified read says the gate is open.
        const lockedAction = definitive ? state.lockedAction : action;
        if (lockKey && lockedAction) locks.set(lockKey, lockedAction);
        clearTimers();
        set({ phase: "verifying", errorMessage: message, lockedAction });
        startPolling(attempt, false);
      },
    );
    return attempt;
  };

  return {
    submit,
    canSubmit,
    getState: view,
    /** The card already shows a settled gate: stop re-reading the relay. */
    stopPolling() {
      pollStopped = true;
      if (poll !== null) clearInterval(poll);
      poll = null;
    },
    /** The card went away: stop timers; every later answer is fenced off. */
    dispose() {
      disposed = true;
      clearTimers();
    },
  };
}

/**
 * @param {{
 *   approval: { status: string, expiresAt: string, approverSpec: string,
 *               approvalRef: string, candidateRef: string | null },
 *   myPubkey?: string | null,
 *   nowMs: number,
 *   phase?: "idle" | "sending" | "uncertain" | "verifying" | "sent" | "failed" | "settled",
 *   action?: "grant" | "deny",
 *   errorMessage?: string | null,
 *   lockedAction?: "grant" | "deny" | null,
 *   settledStatus?: string | null,
 * }} input
 */
export function approvalCardView({
  approval,
  myPubkey,
  nowMs,
  phase = "idle",
  action,
  errorMessage,
  lockedAction = null,
  settledStatus = null,
}) {
  const decision = HEX64.test(approval.approvalRef ?? "")
    ? {
        token: approval.approvalRef.toLowerCase(),
        candidate: approval.candidateRef?.trim() || undefined,
      }
    : undefined;
  const settled = (status, detail) => ({
    mode: "settled",
    buttonsDisabled: true,
    disabledActions: ALL_OFF,
    statusText: detail
      ? `${SETTLED_LABELS[status] ?? "Settled"} · ${detail}`
      : (SETTLED_LABELS[status] ?? "Settled"),
    settledStatus: status,
    error: null,
    decision,
  });

  // 1. The record itself says the gate is closed (someone already settled it).
  if (approval.status in SETTLED_LABELS && approval.status !== "pending")
    return settled(approval.status);

  // 2. This card's own decision.
  if (phase === "settled")
    return settled(settledStatus ?? "settled", "confirmed by the relay");
  if (phase === "sending") {
    return {
      mode: "sending",
      buttonsDisabled: true,
      disabledActions: ALL_OFF,
      statusText: action === "deny" ? "Denying…" : "Approving…",
      error: null,
      decision,
    };
  }
  if (phase === "uncertain" || phase === "verifying") {
    // Past expiry the relay refuses any decision, and a pending record will
    // not change: show Expired rather than waiting forever.
    if (new Date(approval.expiresAt).getTime() < nowMs)
      return settled("expired");
    // The outcome is unknown: never re-enable here.
    return {
      mode: phase,
      buttonsDisabled: true,
      disabledActions: ALL_OFF,
      statusText:
        phase === "uncertain"
          ? APPROVAL_STILL_WAITING_TEXT
          : APPROVAL_VERIFYING_TEXT,
      error: phase === "verifying" ? errorMessage || null : null,
      decision,
    };
  }
  if (phase === "sent")
    return settled(
      action === "deny" ? "denied" : "granted",
      "sent, waiting for the run",
    );
  if (phase === "failed") {
    const closed = settledStatusFromRelayError(errorMessage);
    if (closed) return settled(closed, "already settled elsewhere");
  }

  // 3. Open gate.
  if (new Date(approval.expiresAt).getTime() < nowMs) return settled("expired");
  const error =
    phase === "failed"
      ? errorMessage || "The relay refused this decision."
      : null;
  if (!decision) {
    return {
      mode: "unavailable",
      buttonsDisabled: true,
      disabledActions: ALL_OFF,
      statusText: "This approval cannot be decided from Desktop.",
      error,
      decision,
    };
  }
  const spec = (approval.approverSpec ?? "").trim().toLowerCase();
  const me = (myPubkey ?? "").trim().toLowerCase();
  // A hex approver spec is checked here so only the designated approver sees
  // live buttons. Any other spec form is left to the relay, which enforces it.
  const isApprover = HEX64.test(spec) ? spec === me : me.length > 0;
  if (!isApprover) {
    return {
      mode: "waiting",
      buttonsDisabled: true,
      disabledActions: ALL_OFF,
      statusText: "Waiting on the designated approver.",
      error,
      decision,
    };
  }
  if (lockedAction) {
    // An earlier decision's outcome was unknown: only that decision may be
    // signed again, never the opposite one.
    const label = lockedAction === "deny" ? "Deny" : "Approve";
    return {
      mode: "actions",
      buttonsDisabled: false,
      disabledActions: {
        grant: lockedAction !== "grant",
        deny: lockedAction !== "deny",
      },
      statusText: `The gate is still open. Your earlier ${label} may have reached the relay, so only ${label} can be sent again.`,
      error,
      decision,
    };
  }
  return {
    mode: "actions",
    buttonsDisabled: false,
    disabledActions: { grant: false, deny: false },
    statusText: "Your decision is needed.",
    error,
    decision,
  };
}
