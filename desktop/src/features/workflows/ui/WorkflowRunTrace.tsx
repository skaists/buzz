import { Check, Clock, SkipForward, X } from "lucide-react";

import type { WorkflowApproval, WorkflowRun } from "@/shared/api/types";
import { Badge, type BadgeProps } from "@/shared/ui/badge";
import {
  describeReviewGate,
  type ReviewGate,
  shortPubkey,
} from "@/features/workflows/reviewGate";
import { WorkflowApprovalCard } from "@/features/workflows/ui/WorkflowApprovalCard";

type WorkflowRunTraceProps = {
  run: WorkflowRun;
  approvals?: WorkflowApproval[];
};

const GATE_STATE_LABELS: Record<ReviewGate["state"], string> = {
  waiting: "Waiting for review",
  granted: "Approved",
  denied: "Changes requested",
  expired: "Expired",
};

const GATE_STATE_VARIANTS: Record<ReviewGate["state"], BadgeProps["variant"]> =
  {
    waiting: "warning",
    granted: "success",
    denied: "destructive",
    expired: "secondary",
  };

function formatWhen(value: string | null) {
  if (value === null) return null;
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
}

/**
 * The review gate, legibly: who may decide, what exactly they are deciding
 * on, and — once decided — who decided, when, with which signed event. The
 * raw step output stays below it for inspection; this block is the summary a
 * person reads first.
 */
function ReviewGateBlock({ gate }: { gate: ReviewGate }) {
  const rows: Array<[string, string | null, boolean]> = [
    ["Reviewer", shortPubkey(gate.reviewerSpec), true],
    ["Candidate", gate.candidateRef, true],
    ["Requested", formatWhen(gate.requestedAt), false],
    ["Decided by", shortPubkey(gate.decidedBy), true],
    ["Decided", formatWhen(gate.decidedAt), false],
    ["Note", gate.note, false],
    ["Evidence event", gate.decisionEventId, true],
    [
      "Expires",
      gate.state === "waiting" ? formatWhen(gate.expiresAt) : null,
      false,
    ],
  ];
  return (
    <div
      className="mt-3 rounded-lg border border-border/60 bg-muted/20 p-3"
      data-testid="workflow-review-gate"
      data-gate-state={gate.state}
    >
      <div className="mb-2 flex items-center gap-2">
        <p className="text-2xs font-medium uppercase tracking-[0.16em] text-muted-foreground">
          Review gate
        </p>
        <Badge variant={GATE_STATE_VARIANTS[gate.state]}>
          {GATE_STATE_LABELS[gate.state]}
          {gate.reasserted ? " · re-asserted" : ""}
        </Badge>
      </div>
      <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-xs">
        {rows
          .filter(([, value]) => value !== null)
          .map(([label, value, mono]) => (
            <div className="contents" key={label}>
              <dt className="text-muted-foreground">{label}</dt>
              <dd
                className={
                  mono
                    ? "truncate font-mono"
                    : "whitespace-pre-wrap break-words"
                }
                title={value ?? undefined}
              >
                {value}
              </dd>
            </div>
          ))}
      </dl>
    </div>
  );
}

function formatStatusLabel(status: string) {
  return status.replace(/_/g, " ");
}

function StepStatusBadge({ status }: { status: string }) {
  const variants: Record<string, BadgeProps["variant"]> = {
    completed: "success",
    failed: "destructive",
    error: "destructive",
    running: "info",
    pending: "secondary",
    cancelled: "secondary",
    skipped: "secondary",
    waiting_approval: "warning",
  };

  return (
    <Badge variant={variants[status] ?? "secondary"}>
      {formatStatusLabel(status)}
    </Badge>
  );
}

function StepStatusIcon({ status }: { status: string }) {
  switch (status) {
    case "completed":
      return <Check className="h-4 w-4 text-green-500" />;
    case "failed":
    case "error":
      return <X className="h-4 w-4 text-red-500" />;
    case "skipped":
      return <SkipForward className="h-4 w-4 text-muted-foreground" />;
    case "waiting_approval":
      return <Clock className="h-4 w-4 text-amber-500" />;
    default:
      return <Clock className="h-4 w-4 text-blue-500" />;
  }
}

function formatDuration(startedAt: number | null, completedAt: number | null) {
  if (startedAt === null || completedAt === null) return null;
  const seconds = completedAt - startedAt;
  if (seconds < 1) return `${Math.round(seconds * 1000)}ms`;
  return `${seconds.toFixed(1)}s`;
}

export function WorkflowRunTrace({
  run,
  approvals = [],
}: WorkflowRunTraceProps) {
  if (run.executionTrace.length === 0) {
    return (
      <p className="rounded-xl border border-dashed border-border/70 bg-background/60 px-4 py-6 text-center text-sm text-muted-foreground">
        No steps recorded yet.
      </p>
    );
  }

  return (
    <div className="space-y-3" data-testid="workflow-run-trace">
      {run.executionTrace.map((step) => {
        const duration = formatDuration(step.startedAt, step.completedAt);
        const pendingApproval = approvals.find(
          (a) => a.stepId === step.stepId && a.status === "pending",
        );
        const stepApproval =
          approvals.find((a) => a.stepId === step.stepId) ?? null;
        const gate = describeReviewGate(step, stepApproval);

        return (
          <div
            className="rounded-xl border border-border/60 bg-background/80 p-3 shadow-xs"
            key={step.stepId}
          >
            <div className="flex flex-wrap items-center gap-2 text-sm">
              <StepStatusIcon status={step.status} />
              <span className="min-w-0 flex-1 truncate font-mono text-xs font-medium">
                {step.stepId}
              </span>
              <StepStatusBadge status={step.status} />
              {duration ? (
                <span className="text-xs text-muted-foreground">
                  {duration}
                </span>
              ) : null}
            </div>
            {gate ? <ReviewGateBlock gate={gate} /> : null}
            {Object.keys(step.output).length > 0 ? (
              <div className="mt-3">
                <p className="mb-1 text-2xs font-medium uppercase tracking-[0.16em] text-muted-foreground">
                  Output
                </p>
                <pre className="max-h-32 overflow-auto rounded-lg bg-muted/40 px-3 py-2 font-mono text-xs text-muted-foreground">
                  {JSON.stringify(step.output, null, 2)}
                </pre>
              </div>
            ) : null}
            {step.error ? (
              <div className="mt-3">
                <p className="mb-1 text-2xs font-medium uppercase tracking-[0.16em] text-red-400">
                  Error
                </p>
                <pre className="max-h-32 overflow-auto rounded-lg bg-red-500/10 px-3 py-2 font-mono text-xs text-red-400">
                  {step.error}
                </pre>
              </div>
            ) : null}
            {pendingApproval ? (
              <div className="mt-3">
                <p className="mb-2 text-2xs font-medium uppercase tracking-[0.16em] text-amber-600">
                  Pending approval
                </p>
                <WorkflowApprovalCard approval={pendingApproval} />
              </div>
            ) : null}
          </div>
        );
      })}
    </div>
  );
}
