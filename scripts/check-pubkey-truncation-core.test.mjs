import assert from "node:assert/strict";
import { test } from "node:test";

import { findPubkeyTruncations } from "./check-pubkey-truncation-core.mjs";

// Fixtures are source text. OPEN is the template-placeholder opener, built
// from two pieces so no plain string literal contains a placeholder.
const OPEN = "$" + "{";

function flagged(source) {
  return findPubkeyTruncations(source).map((hit) => hit.lineNumber);
}

test("flags slice/substring on receivers named like a pubkey or npub", () => {
  assert.deepEqual(
    flagged(
      [
        "const a = pubkey.slice(0, 8);",
        "const b = authorPubkey.substring(0, 12);",
        "const c = member.pub_key.slice(0, 8);",
        "const d = userNpub?.slice(0, 10);",
      ].join("\n"),
    ),
    [1, 2, 3, 4],
  );
});

test("flags pubkey-bearing field names that do not say pubkey", () => {
  assert.deepEqual(
    flagged(
      [
        "const label = ownerLabel?.trim() || `" +
          OPEN +
          "repository.owner.slice(0, 8)}…`;",
        "const who = event.author.slice(0, 8);",
        "const s = grant.signer?.substring(0, 8);",
        "const r = gate.reviewer.slice(0, 8) + gate.approver.slice(-4);",
        "const m = dm.sender.slice(0, 8) + dm.recipient.slice(0, 8) + channel.creator.slice(0, 8);",
      ].join("\n"),
    ),
    [1, 2, 3, 4, 5],
  );
});

test("does not flag truncation of things that are not keys", () => {
  assert.deepEqual(
    flagged(
      [
        "const preview = `" + OPEN + "post.content.slice(0, 200)}...`;",
        "const id = `event " + OPEN + "repositoryEvent.id.slice(0, 8)}…`;",
        "const first = pubkeys.slice(0, 3);",
        "const label = ownerLabel.slice(0, 20);",
        "const shown = truncatePubkey(repository.owner);",
      ].join("\n"),
    ),
    [],
  );
});

test("reports the trimmed source line with its 1-based number", () => {
  assert.deepEqual(
    findPubkeyTruncations("\n  const a = pubkey.slice(0, 8);  \n"),
    [{ lineNumber: 2, line: "const a = pubkey.slice(0, 8);" }],
  );
});

test("a generic receiver is invisible to a name-based guard (known limit)", () => {
  assert.deepEqual(
    flagged(
      "return `" + OPEN + "value.slice(0, 8)}…" + OPEN + "value.slice(-4)}`;",
    ),
    [],
  );
});
