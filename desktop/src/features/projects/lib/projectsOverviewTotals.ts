import type {
  Project,
  ProjectActivitySummary,
} from "@/features/projects/hooks";

export type ProjectsOverviewTotals = {
  projects: number;
  repositories: number;
  prs: number;
  issues: number;
};

/**
 * Totals for the Projects overview tiles. A repository can sit in more than
 * one project; the totals count each repository, and its pull requests and
 * issues, once. Per-project counts are left to the project pages.
 */
export function projectsOverviewTotals(
  projects: Pick<Project, "repositories">[],
  repositorySummaries:
    | Record<string, Pick<ProjectActivitySummary, "issueCount" | "prCount">>
    | undefined,
): ProjectsOverviewTotals {
  const repoAddresses = new Set(
    projects.flatMap((project) =>
      project.repositories.map((repository) => repository.repoAddress),
    ),
  );
  let prs = 0;
  let issues = 0;
  for (const repoAddress of repoAddresses) {
    const summary = repositorySummaries?.[repoAddress];
    prs += summary?.prCount ?? 0;
    issues += summary?.issueCount ?? 0;
  }
  return {
    projects: projects.length,
    repositories: repoAddresses.size,
    prs,
    issues,
  };
}
