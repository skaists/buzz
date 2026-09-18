import { strict as assert } from "node:assert";
import test from "node:test";

import { canEditChannelSettings } from "./channelSettingsAccess.ts";

function channel(overrides = {}) {
  return {
    id: "channel-id",
    name: "general",
    channelType: "stream",
    visibility: "open",
    description: "",
    topic: null,
    purpose: null,
    memberCount: 2,
    memberPubkeys: [],
    lastMessageAt: null,
    archivedAt: null,
    participants: [],
    participantPubkeys: [],
    isMember: true,
    ttlSeconds: 3600,
    ttlDeadline: null,
    ...overrides,
  };
}

test("a manager can edit an active channel's settings", () => {
  assert.equal(canEditChannelSettings(channel(), true), true);
});

test("an archived channel offers no settings edit, even to a manager", () => {
  const archived = channel({ archivedAt: "2026-09-18T05:00:00Z" });
  assert.equal(canEditChannelSettings(archived, true), false);
});

test("a non-manager cannot edit channel settings", () => {
  assert.equal(canEditChannelSettings(channel(), false), false);
});

test("DM settings are never editable", () => {
  assert.equal(
    canEditChannelSettings(channel({ channelType: "dm" }), true),
    false,
  );
});
