import { CircleDot, FolderGit2, Folders, GitPullRequest } from "lucide-react";
import type * as React from "react";

import type {
  Project,
  ProjectActivitySummary,
} from "@/features/projects/hooks";
import { projectsOverviewTotals } from "@/features/projects/lib/projectsOverviewTotals";

export type ProjectsOverviewSection =
  | "projects"
  | "repositories"
  | "prs"
  | "issues";

type ProjectsOverviewPanelProps = {
  children: React.ReactNode;
  metadata: React.ReactNode;
  onSelectSection: (section: ProjectsOverviewSection) => void;
  projects: Project[];
  /** Activity keyed by repository address, not by project. */
  repositorySummaries?: Record<string, ProjectActivitySummary>;
};

function StatPill({
  count,
  icon: Icon,
  label,
  onClick,
}: {
  count: number;
  icon: React.ComponentType<{ className?: string }>;
  label: string;
  onClick: () => void;
}) {
  return (
    <button
      className="flex flex-col rounded-lg border border-border/60 bg-transparent px-3.5 py-3 text-left transition-colors hover:bg-muted/30"
      data-testid="projects-overview-stat"
      onClick={onClick}
      type="button"
    >
      <span className="flex w-full items-center justify-between gap-2">
        <span className="text-xs font-medium text-muted-foreground">
          {label}
        </span>
        <Icon className="h-3.5 w-3.5 text-muted-foreground/70" />
      </span>
      <span className="mt-auto pt-3 text-2xl font-semibold leading-none tracking-tight text-foreground">
        {count}
      </span>
    </button>
  );
}

export function ProjectsOverviewPanel({
  children,
  metadata,
  onSelectSection,
  projects,
  repositorySummaries,
}: ProjectsOverviewPanelProps) {
  const totals = projectsOverviewTotals(projects, repositorySummaries);

  // The feed owns the full left column; the stat counters live at the top
  // of the side rail as compact cards, above People and Contribution
  // Activity, instead of a full-width row over the feed. The feed column
  // has no left inset so the timeline spine lines up with the section tabs.
  return (
    <section
      className="-mr-[calc(1rem+10px)] mb-4"
      data-testid="projects-overview-panel"
    >
      <div className="grid xl:grid-cols-[minmax(0,1fr)_18rem] 2xl:grid-cols-[minmax(0,1fr)_24rem]">
        <div className="order-1 min-w-0 pb-4 pr-4 pt-2 xl:order-none xl:col-start-1 xl:row-start-1">
          {children}
        </div>
        <div className="order-2 flex min-w-0 flex-col gap-3 px-4 pb-4 pt-2 xl:order-none xl:col-start-2 xl:row-start-1">
          <div className="grid grid-cols-2 gap-2">
            <StatPill
              count={totals.projects}
              icon={Folders}
              label="Projects"
              onClick={() => onSelectSection("projects")}
            />
            <StatPill
              count={totals.repositories}
              icon={FolderGit2}
              label="Repositories"
              onClick={() => onSelectSection("repositories")}
            />
            <StatPill
              count={totals.prs}
              icon={GitPullRequest}
              label="Pull requests"
              onClick={() => onSelectSection("prs")}
            />
            <StatPill
              count={totals.issues}
              icon={CircleDot}
              label="Issues"
              onClick={() => onSelectSection("issues")}
            />
          </div>
          {metadata}
        </div>
      </div>
    </section>
  );
}
