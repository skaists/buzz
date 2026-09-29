import assert from "node:assert/strict";
import test from "node:test";

import { projectsOverviewTotals } from "./projectsOverviewTotals.ts";

function repository(repoAddress) {
  return { repoAddress };
}

function project(id, repoAddresses) {
  return { id, repositories: repoAddresses.map(repository) };
}

const SHARED = "30617:owner:shared";
const ONLY_A = "30617:owner:only-a";
const ONLY_B = "30617:owner:only-b";

const summaries = {
  [SHARED]: { issueCount: 2, prCount: 5 },
  [ONLY_A]: { issueCount: 1, prCount: 3 },
  [ONLY_B]: { issueCount: 0, prCount: 7 },
};

test("a repository in two projects counts once, with its pull requests and issues", () => {
  const projects = [
    project("a", [SHARED, ONLY_A]),
    project("b", [SHARED, ONLY_B]),
  ];

  assert.deepEqual(projectsOverviewTotals(projects, summaries), {
    projects: 2,
    repositories: 3,
    prs: 5 + 3 + 7,
    issues: 2 + 1 + 0,
  });
});

test("the shared repository is the only difference from the per-project sum", () => {
  // Precondition: the fixture really does put one repository in two projects.
  const projects = [
    project("a", [SHARED, ONLY_A]),
    project("b", [SHARED, ONLY_B]),
  ];
  const listed = projects.flatMap((entry) =>
    entry.repositories.map((repo) => repo.repoAddress),
  );
  assert.equal(listed.filter((address) => address === SHARED).length, 2);
  assert.equal(listed.length, 4);

  const totals = projectsOverviewTotals(projects, summaries);
  assert.equal(totals.repositories, listed.length - 1);
});

test("a repository with no summary yet counts as a repository with no activity", () => {
  const totals = projectsOverviewTotals(
    [project("a", [SHARED, "30617:owner:unsummarised"])],
    { [SHARED]: summaries[SHARED] },
  );

  assert.deepEqual(totals, { projects: 1, repositories: 2, prs: 5, issues: 2 });
});

test("no summaries at all reads zero activity, not a crash", () => {
  assert.deepEqual(
    projectsOverviewTotals([project("a", [SHARED])], undefined),
    {
      projects: 1,
      repositories: 1,
      prs: 0,
      issues: 0,
    },
  );
});
