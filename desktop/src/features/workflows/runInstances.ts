import {
  describeRunBlocker,
  type RunBlocker,
} from "@/features/workflows/reviewGate";
import type { Workflow, WorkflowRun } from "@/shared/api/types";

/** One run of one workflow, as the hive-wide instances list shows it. */
export type RunInstance = {
  workflowId: string;
  workflowName: string;
  channelName: string;
  run: WorkflowRun;
  /** What a waiting run is blocked on; `null` for every other status. */
  blocker: RunBlocker | null;
};

export type RunInstancesSummary = {
  /** Waiting on a typed dependency. Longest-waiting first. */
  waiting: RunInstance[];
  /** Pending or running. Newest first. */
  running: RunInstance[];
  /** Completed, failed or cancelled. Newest-finished first, bounded. */
  recent: RunInstance[];
  /** How many finished runs exist before the bound is applied. */
  recentTotal: number;
};

type WorkflowRef = {
  workflow: Pick<Workflow, "id" | "name">;
  channelName: string;
};

function finishedAt(run: WorkflowRun): number {
  return run.completedAt ?? run.createdAt;
}

/**
 * Group the runs of every listed workflow by durable run status. Pure: the
 * caller supplies whatever runs have loaded; a workflow whose runs are still
 * loading contributes nothing rather than a guess, and runs for workflows
 * that are not listed are ignored.
 */
export function summarizeRunInstances(
  workflows: WorkflowRef[],
  runsByWorkflowId: Record<string, WorkflowRun[] | undefined>,
  now: Date = new Date(),
  recentLimit = 5,
): RunInstancesSummary {
  const waiting: RunInstance[] = [];
  const running: RunInstance[] = [];
  const finished: RunInstance[] = [];

  for (const { workflow, channelName } of workflows) {
    for (const run of runsByWorkflowId[workflow.id] ?? []) {
      const instance: RunInstance = {
        workflowId: workflow.id,
        workflowName: workflow.name,
        channelName,
        run,
        blocker: describeRunBlocker(run, now),
      };
      if (run.status === "waiting_approval") {
        waiting.push(instance);
      } else if (run.status === "pending" || run.status === "running") {
        running.push(instance);
      } else {
        finished.push(instance);
      }
    }
  }

  waiting.sort((a, b) => a.run.createdAt - b.run.createdAt);
  running.sort((a, b) => b.run.createdAt - a.run.createdAt);
  finished.sort((a, b) => finishedAt(b.run) - finishedAt(a.run));

  return {
    waiting,
    running,
    recent: finished.slice(0, Math.max(0, recentLimit)),
    recentTotal: finished.length,
  };
}
