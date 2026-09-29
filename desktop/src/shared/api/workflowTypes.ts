export type WorkflowStatus = "active" | "disabled" | "archived";

export type Workflow = {
  id: string;
  name: string;
  ownerPubkey: string;
  channelId: string | null;
  definition: Record<string, unknown>;
  status: WorkflowStatus;
  createdAt: number;
  updatedAt: number;
};

export type WorkflowSaveResult = {
  workflow: Workflow;
  webhookSecret: string | null;
};

export type WorkflowRunStatus =
  | "pending"
  | "running"
  | "completed"
  | "failed"
  | "cancelled"
  | "waiting_approval";

export type TraceEntry = {
  stepId: string;
  status: string;
  output: Record<string, unknown>;
  startedAt: number | null;
  completedAt: number | null;
  error: string | null;
};

export type WorkflowRun = {
  id: string;
  workflowId: string;
  status: WorkflowRunStatus;
  currentStep: number | null;
  executionTrace: TraceEntry[];
  startedAt: number | null;
  completedAt: number | null;
  errorCode: string | null;
  errorMessage: string | null;
  createdAt: number;
};

export type WorkflowApprovalStatus =
  | "pending"
  | "granted"
  | "denied"
  | "expired";

export type WorkflowApproval = {
  /** Hex SHA-256 of the approval token: the relay's lookup key for a decision
   * (`d` tag). Not a bearer secret; the relay only honours the designated
   * approver's signature (and the bound candidate, when there is one). */
  approvalRef: string;
  workflowId: string;
  runId: string;
  stepId: string;
  stepIndex: number;
  approverSpec: string;
  status: WorkflowApprovalStatus;
  approverPubkey: string | null;
  note: string | null;
  /** Exact candidate the gate is bound to (opaque, e.g. a commit sha); null for unbound gates. */
  candidateRef: string | null;
  expiresAt: string;
  createdAt: number;
};

export type TriggerWorkflowResponse = {
  runId: string;
  workflowId: string;
  status: string;
};

export type ApprovalActionResponse = {
  token: string;
  status: string;
  runId: string;
  workflowId: string;
};
