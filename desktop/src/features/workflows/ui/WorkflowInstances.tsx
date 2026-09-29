import { useWorkflowsRunsQueries } from "@/features/workflows/hooks";
import { runBlockerLabel } from "@/features/workflows/reviewGate";
import {
  type RunInstance,
  summarizeRunInstances,
} from "@/features/workflows/runInstances";
import type { Workflow } from "@/shared/api/types";

type WorkflowInstancesProps = {
  workflows: { workflow: Workflow; channelName: string }[];
  selectedRunId: string | null;
  onSelectInstance: (workflowId: string, runId: string) => void;
};

function statusWords(status: string) {
  return status.replace(/_/g, " ");
}

function instanceDetail(instance: RunInstance): string {
  if (instance.blocker) return runBlockerLabel(instance.blocker);
  const { run } = instance;
  if (run.status === "failed" && run.errorCode) {
    return `failed (${statusWords(run.errorCode)})`;
  }
  return statusWords(run.status);
}

function InstanceGroup({
  group,
  instances,
  onSelectInstance,
  selectedRunId,
  title,
}: {
  group: "waiting" | "running" | "recent";
  instances: RunInstance[];
  onSelectInstance: WorkflowInstancesProps["onSelectInstance"];
  selectedRunId: string | null;
  title: string;
}) {
  if (instances.length === 0) return null;
  return (
    <div data-instance-group={group}>
      <p className="mb-1 text-2xs font-medium uppercase tracking-[0.16em] text-muted-foreground">
        {title}
      </p>
      <ul className="space-y-1">
        {instances.map((instance) => (
          <li key={`${instance.workflowId}:${instance.run.id}`}>
            <button
              className={`w-full rounded-lg border px-3 py-2 text-left text-xs transition-colors ${
                selectedRunId === instance.run.id
                  ? "border-primary/40 bg-primary/5"
                  : "border-border/60 hover:bg-muted/20"
              }`}
              data-run-status={instance.run.status}
              data-testid={`workflow-instance-${instance.run.id}`}
              onClick={() =>
                onSelectInstance(instance.workflowId, instance.run.id)
              }
              type="button"
            >
              <span className="flex flex-wrap items-baseline gap-x-2">
                <span className="min-w-0 truncate font-medium text-foreground">
                  {instance.workflowName}
                </span>
                {instance.channelName ? (
                  <span className="text-muted-foreground">
                    #{instance.channelName}
                  </span>
                ) : null}
                <span className="ml-auto shrink-0 text-muted-foreground">
                  {new Date(instance.run.createdAt * 1000).toLocaleString()}
                </span>
              </span>
              <span
                className={`mt-0.5 block break-words ${
                  instance.blocker?.kind === "review_expired" ||
                  instance.run.status === "failed"
                    ? "text-destructive"
                    : "text-muted-foreground"
                }`}
              >
                {instanceDetail(instance)}
              </span>
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}

/**
 * Hive-wide list of workflow run instances, derived from the same per-workflow
 * run queries the detail panel uses. Renders nothing until a run exists.
 */
export function WorkflowInstances({
  workflows,
  selectedRunId,
  onSelectInstance,
}: WorkflowInstancesProps) {
  const runsByWorkflowId = useWorkflowsRunsQueries(
    workflows.map(({ workflow }) => workflow.id),
  );
  const summary = summarizeRunInstances(workflows, runsByWorkflowId);

  if (
    summary.waiting.length === 0 &&
    summary.running.length === 0 &&
    summary.recent.length === 0
  ) {
    return null;
  }

  return (
    <section
      aria-label="Workflow runs"
      className="mb-4 space-y-3 rounded-xl border border-border/70 bg-card/60 p-3"
      data-testid="workflow-instances"
    >
      <InstanceGroup
        group="waiting"
        instances={summary.waiting}
        onSelectInstance={onSelectInstance}
        selectedRunId={selectedRunId}
        title="Waiting on review"
      />
      <InstanceGroup
        group="running"
        instances={summary.running}
        onSelectInstance={onSelectInstance}
        selectedRunId={selectedRunId}
        title="Running"
      />
      <InstanceGroup
        group="recent"
        instances={summary.recent}
        onSelectInstance={onSelectInstance}
        selectedRunId={selectedRunId}
        title={
          summary.recentTotal > summary.recent.length
            ? `Recent (${summary.recent.length} of ${summary.recentTotal})`
            : "Recent"
        }
      />
    </section>
  );
}
