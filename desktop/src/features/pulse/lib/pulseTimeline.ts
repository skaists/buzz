import { safeNpub } from "@/shared/lib/nostrUtils";
import type { UserNote } from "@/shared/api/socialTypes";
import type { UserProfileSummary } from "@/shared/api/types";

export type TimelineState = "loading" | "error" | "empty" | "list";

/**
 * Which body a Pulse timeline shows. A failed query with nothing to show is
 * "error", never "empty": "No public notes yet." is only true after the relay
 * answered with zero notes. A failed background refetch that still has notes
 * to show stays "list"; the view names the error above it.
 */
export function describeTimelineState({
  isLoading,
  isError,
  count,
}: {
  isLoading: boolean;
  isError: boolean;
  count: number;
}): TimelineState {
  if (isLoading) return "loading";
  if (count > 0) return "list";
  if (isError) return "error";
  return "empty";
}

/** The query's own error text. Tauri commands reject with plain strings. */
export function describeQueryError(error: unknown): string {
  if (error instanceof Error && error.message.trim().length > 0) {
    return error.message;
  }
  if (typeof error === "string" && error.trim().length > 0) {
    return error;
  }
  return String(error);
}

/**
 * Notes whose author (display name, name, hex pubkey prefix or npub prefix)
 * or text contains the query, case-insensitive. A blank query matches nothing:
 * the search tab shows its prompt instead of a list.
 */
export function filterPulseNotes(
  notes: readonly UserNote[],
  profiles: Record<string, UserProfileSummary>,
  query: string,
): UserNote[] {
  const needle = query.trim().toLowerCase();
  if (needle.length === 0) return [];

  const npubs = new Map<string, string>();
  const npubOf = (pubkey: string) => {
    let npub = npubs.get(pubkey);
    if (npub === undefined) {
      npub = safeNpub(pubkey) ?? "";
      npubs.set(pubkey, npub);
    }
    return npub;
  };

  return notes.filter((note) => {
    const pubkey = note.pubkey.toLowerCase();
    if (pubkey.startsWith(needle)) return true;
    if (needle.startsWith("npub1") && npubOf(pubkey).startsWith(needle)) {
      return true;
    }
    const profile = profiles[pubkey];
    if (profile?.displayName?.toLowerCase().includes(needle)) return true;
    if (profile?.name?.toLowerCase().includes(needle)) return true;
    return note.content.toLowerCase().includes(needle);
  });
}
