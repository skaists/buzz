import {
  resolveUserLabel,
  type UserProfileLookup,
} from "@/features/profile/lib/identity";
import type {
  Project,
  ProjectActivitySummary,
} from "@/features/projects/hooks";
import { projectsOverviewActivityByDay } from "@/features/projects/lib/projectsOverviewTotals";
import { normalizePubkey } from "@/shared/lib/pubkey";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/shared/ui/tooltip";
import { UserAvatar } from "@/shared/ui/UserAvatar";
import { OverviewRailSection } from "./ProjectOverviewPanel";
import { ProjectsContributionGraph } from "./ProjectsContributionGraph";

type ProjectsOverviewRailProps = {
  profiles?: UserProfileLookup;
  projects: Project[];
  /** Activity keyed by repository address, not by project. */
  repositorySummaries?: Record<string, ProjectActivitySummary>;
  summaries?: Record<string, ProjectActivitySummary>;
};

function overviewPeople(
  projects: Project[],
  summaries: Record<string, ProjectActivitySummary> | undefined,
) {
  return [
    ...new Set(
      projects.flatMap((project) =>
        [
          project.owner,
          ...project.repositories.flatMap((repository) => [
            repository.owner,
            ...repository.contributors,
          ]),
          ...(summaries?.[project.id]?.participantPubkeys ?? []),
        ].map(normalizePubkey),
      ),
    ),
  ];
}

/** Workspace people and contribution activity for the overview side rail. */
export function ProjectsOverviewRail({
  profiles,
  projects,
  repositorySummaries,
  summaries,
}: ProjectsOverviewRailProps) {
  const people = overviewPeople(projects, summaries);
  const activityByDay = projectsOverviewActivityByDay(
    projects,
    repositorySummaries,
  );

  // Plain stacked cards — the overview panel's rail column owns placement
  // and spacing, so the sections can never drift apart or collide.
  return (
    <>
      <div className="rounded-lg border border-border/60 p-4">
        <OverviewRailSection title="People" titleClassName="text-base">
          {people.length > 0 ? (
            <div className="flex flex-wrap gap-1.5">
              {people.slice(0, 18).map((pubkey) => {
                const profile = profiles?.[normalizePubkey(pubkey)];
                const label = resolveUserLabel({ profiles, pubkey });
                return (
                  <Tooltip key={pubkey}>
                    <TooltipTrigger asChild>
                      <span className="inline-flex">
                        <UserAvatar
                          accent={profile?.isAgent === true}
                          avatarUrl={profile?.avatarUrl ?? null}
                          displayName={label}
                          size="sm"
                        />
                      </span>
                    </TooltipTrigger>
                    <TooltipContent>{label}</TooltipContent>
                  </Tooltip>
                );
              })}
            </div>
          ) : (
            <p className="text-sm text-muted-foreground">No people yet.</p>
          )}
        </OverviewRailSection>
      </div>

      <div className="min-w-0 rounded-lg border border-border/60 p-4">
        <OverviewRailSection
          title="Contribution Activity"
          titleClassName="text-base"
        >
          <ProjectsContributionGraph activityByDay={activityByDay} compact />
        </OverviewRailSection>
      </div>
    </>
  );
}
