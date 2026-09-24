import { describeRunTransitions } from "@/features/workflows/runHistory";
import type { WorkflowApproval, WorkflowRun } from "@/shared/api/types";

type WorkflowRunHistoryProps = {
  run: WorkflowRun;
  approvals?: WorkflowApproval[];
};

function formatAt(at: number | null) {
  return at === null ? "time not recorded" : new Date(at).toLocaleString();
}

/**
 * Transition history for one run, in causal order. Read-only; every line is
 * derived from durable run state by `describeRunTransitions`.
 */
export function WorkflowRunHistory({
  run,
  approvals = [],
}: WorkflowRunHistoryProps) {
  const transitions = describeRunTransitions(run, approvals);

  return (
    <ol className="space-y-1.5" data-testid="workflow-run-history">
      {transitions.map((transition, index) => (
        <li
          className="flex flex-wrap items-baseline gap-x-3 gap-y-0.5 text-xs"
          data-transition-kind={transition.kind}
          // Order is causal and the list is derived, so position is identity.
          // biome-ignore lint/suspicious/noArrayIndexKey: derived, append-only list
          key={`${index}-${transition.kind}-${transition.stepId ?? ""}`}
        >
          <span className="min-w-0 flex-1 break-words text-foreground">
            {transition.label}
          </span>
          <span className="shrink-0 text-muted-foreground">
            {formatAt(transition.at)}
          </span>
          {transition.evidenceEventId ? (
            <span
              className="w-full break-all font-mono text-2xs text-muted-foreground"
              title="Signed decision event"
            >
              evidence {transition.evidenceEventId}
            </span>
          ) : null}
        </li>
      ))}
    </ol>
  );
}
