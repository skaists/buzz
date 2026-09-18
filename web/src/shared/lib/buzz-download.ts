export const BUZZ_RELEASES_URL = "https://github.com/block/buzz/releases";

export const SKAISTS_EDITION_LABEL = "skaists buzz";

/**
 * The published skaists buzz build, or undefined while none exists. Only an
 * https URL counts; anything else leaves the edition entry off the page.
 */
export function skaistsBuildUrl(
  configured: string | undefined,
): string | undefined {
  const url = configured?.trim();
  if (!url) return undefined;
  try {
    return new URL(url).protocol === "https:" ? url : undefined;
  } catch {
    return undefined;
  }
}
