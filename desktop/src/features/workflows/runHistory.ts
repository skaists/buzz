import {
  describeReviewGate,
  shortPubkey,
} from "@/features/workflows/reviewGate";
import type { WorkflowApproval, WorkflowRun } from "@/shared/api/types";

export type RunTransitionKind =
  | "created"
  | "started"
  | "step"
  | "review_requested"
  | "review_granted"
  | "review_denied"
  | "review_expired"
  | "completed"
  | "failed"
  | "cancelled";

/**
 * One transition in a run's life, derived only from durable state (the run
 * row, its trace, and the approvals rows). `at` is epoch milliseconds, or
 * `null` when the relay recorded no time for it: ordinary steps carry no
 * timestamps in the trace, and nothing is invented to fill the gap.
 */
export type RunTransition = {
  kind: RunTransitionKind;
  at: number | null;
  stepId: string | null;
  label: string;
  /** Event id of the signed decision, for review transitions that have one. */
  evidenceEventId: string | null;
};

function secondsToMs(value: number | null): number | null {
  return value === null ? null : value * 1000;
}

function isoToMs(value: string | null): number | null {
  if (value === null) return null;
  const ms = Date.parse(value);
  return Number.isNaN(ms) ? null : ms;
}

function words(value: string): string {
  return value.replace(/_/g, " ");
}

function by(prefix: string, pubkey: string | null): string {
  const who = shortPubkey(pubkey);
  return who === null ? prefix : `${prefix} by ${who}`;
}

/**
 * A run's transition history in causal order: run created/started, then the
 * trace as the relay wrote it, then the terminal state. Order follows the
 * trace, never the clock, so skewed or missing times cannot reorder events.
 */
export function describeRunTransitions(
  run: WorkflowRun,
  approvals: WorkflowApproval[] = [],
  now: Date = new Date(),
): RunTransition[] {
  const transitions: RunTransition[] = [
    {
      kind: "created",
      at: secondsToMs(run.createdAt),
      stepId: null,
      label: "Run created",
      evidenceEventId: null,
    },
  ];

  if (run.startedAt !== null) {
    transitions.push({
      kind: "started",
      at: secondsToMs(run.startedAt),
      stepId: null,
      label: "Run started",
      evidenceEventId: null,
    });
  }

  for (const step of run.executionTrace) {
    const approval = approvals.find((a) => a.stepId === step.stepId) ?? null;
    const gate = describeReviewGate(step, approval, now);

    if (gate === null) {
      transitions.push({
        kind: "step",
        at: secondsToMs(step.completedAt),
        stepId: step.stepId,
        label: `Step ${step.stepId} ${words(step.status)}`,
        evidenceEventId: null,
      });
      continue;
    }

    const candidate =
      gate.candidateRef === null ? "" : ` · candidate ${gate.candidateRef}`;
    transitions.push({
      kind: "review_requested",
      at: isoToMs(gate.requestedAt) ?? secondsToMs(step.startedAt),
      stepId: step.stepId,
      label: `Review requested from ${
        shortPubkey(gate.reviewerSpec) ?? gate.reviewerSpec
      }${candidate}`,
      evidenceEventId: null,
    });

    if (gate.state === "granted" || gate.state === "denied") {
      transitions.push({
        kind: gate.state === "granted" ? "review_granted" : "review_denied",
        at: isoToMs(gate.decidedAt),
        stepId: step.stepId,
        label: by(
          gate.state === "granted" ? "Review granted" : "Review denied",
          gate.decidedBy,
        ),
        evidenceEventId: gate.decisionEventId,
      });
    } else if (gate.state === "expired") {
      transitions.push({
        kind: "review_expired",
        at: isoToMs(gate.expiresAt),
        stepId: step.stepId,
        label: "Review gate expired",
        evidenceEventId: null,
      });
    }
  }

  if (
    run.status === "completed" ||
    run.status === "failed" ||
    run.status === "cancelled"
  ) {
    const reason =
      run.status === "failed" && run.errorCode
        ? ` (${words(run.errorCode)})`
        : "";
    transitions.push({
      kind: run.status,
      at: secondsToMs(run.completedAt),
      stepId: null,
      label: `Run ${run.status}${reason}`,
      evidenceEventId: null,
    });
  }

  return transitions;
}
