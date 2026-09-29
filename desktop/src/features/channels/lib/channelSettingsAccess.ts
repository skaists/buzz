import type { Channel } from "@/shared/api/types";

// The relay rejects every change to an archived channel except unarchiving,
// so an archived channel offers no settings edit; Unarchive stays available.
export function canEditChannelSettings(
  channel: Channel | null | undefined,
  canManageChannel: boolean,
): boolean {
  return (
    canManageChannel &&
    channel?.channelType !== "dm" &&
    (channel?.archivedAt ?? null) === null
  );
}
