import assert from "node:assert/strict";
import test from "node:test";

import { safeNpub } from "@/shared/lib/nostrUtils";

import {
  describeQueryError,
  describeTimelineState,
  filterPulseNotes,
} from "./pulseTimeline.ts";

// Synthetic keys only; none of these belongs to anyone.
const ALICE = "a1".repeat(32);
const BOB = "b2".repeat(32);
const CAROL = "c3".repeat(32);

function note(id, pubkey, content) {
  return { id: id.repeat(64), pubkey, createdAt: 0, content, tags: [] };
}

const NOTES = [
  note("1", ALICE, "shipping the hive today"),
  note("2", BOB, "morning pollen count is high"),
  note("3", CAROL, "nothing about bees here"),
];

const PROFILES = {
  [ALICE]: {
    displayName: "Marigold Tester",
    avatarUrl: null,
    nip05Handle: null,
    ownerPubkey: null,
  },
  [BOB]: {
    displayName: null,
    name: "quillwort",
    avatarUrl: null,
    nip05Handle: null,
    ownerPubkey: null,
  },
};

const ids = (notes) => notes.map((n) => n.id[0]);

test("a failed query with no notes is an error, never the empty state", () => {
  assert.equal(
    describeTimelineState({ isLoading: false, isError: true, count: 0 }),
    "error",
  );
});

test("the empty state needs a query that answered with zero notes", () => {
  assert.equal(
    describeTimelineState({ isLoading: false, isError: false, count: 0 }),
    "empty",
  );
});

test("loading wins, and notes already on screen stay on screen", () => {
  assert.equal(
    describeTimelineState({ isLoading: true, isError: false, count: 0 }),
    "loading",
  );
  assert.equal(
    describeTimelineState({ isLoading: false, isError: false, count: 3 }),
    "list",
  );
  assert.equal(
    describeTimelineState({ isLoading: false, isError: true, count: 3 }),
    "list",
  );
});

test("describeQueryError keeps the cause's own words", () => {
  assert.equal(
    describeQueryError(new Error("relay connection refused")),
    "relay connection refused",
  );
  assert.equal(
    describeQueryError("relay timed out after 10s"),
    "relay timed out after 10s",
  );
  assert.equal(describeQueryError({ code: 7 }), "[object Object]");
});

test("search matches by author display name only", () => {
  // "marigold" is in ALICE's display name and in no note's text.
  assert.ok(!NOTES.some((n) => n.content.toLowerCase().includes("marigold")));
  assert.deepEqual(ids(filterPulseNotes(NOTES, PROFILES, "MariGold")), ["1"]);
});

test("search matches by kind-0 name when there is no display name", () => {
  assert.deepEqual(ids(filterPulseNotes(NOTES, PROFILES, "quill")), ["2"]);
});

test("search matches by note text only", () => {
  // "pollen" is in BOB's note and in no profile.
  assert.ok(
    !Object.values(PROFILES).some((p) =>
      `${p.displayName ?? ""} ${p.name ?? ""}`.toLowerCase().includes("pollen"),
    ),
  );
  assert.deepEqual(ids(filterPulseNotes(NOTES, PROFILES, "pollen")), ["2"]);
});

test("search matches by hex pubkey prefix and by npub prefix", () => {
  assert.deepEqual(ids(filterPulseNotes(NOTES, PROFILES, "c3c3c3")), ["3"]);
  const carolNpub = safeNpub(CAROL);
  assert.ok(carolNpub?.startsWith("npub1"));
  assert.deepEqual(
    ids(filterPulseNotes(NOTES, PROFILES, carolNpub.slice(0, 20))),
    ["3"],
  );
});

test("a query that matches nothing returns nothing, and a blank query too", () => {
  assert.deepEqual(filterPulseNotes(NOTES, PROFILES, "zzzz-no-such-thing"), []);
  assert.deepEqual(filterPulseNotes(NOTES, PROFILES, "   "), []);
});

test("filtering the 50-note Everyone page stays under the 200 ms budget", () => {
  const page = Array.from({ length: 50 }, (_, i) =>
    note(
      String(i % 10),
      i % 2 ? ALICE : BOB,
      `note number ${i} about the hive`,
    ),
  );
  const started = performance.now();
  for (const query of ["h", "hi", "hiv", "hive", "hive ", "marigold"]) {
    filterPulseNotes(page, PROFILES, query);
  }
  const elapsed = performance.now() - started;
  console.log(
    `pulse search: 6 keystrokes over 50 notes in ${elapsed.toFixed(2)} ms`,
  );
  assert.ok(elapsed < 200, `${elapsed} ms`);
});
