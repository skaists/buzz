export const BUZZ_RELEASES_URL = "https://github.com/block/buzz/releases";

export const SKAISTS_EDITION_LABEL = "skaists buzz";

export type BuzzDownloadPlatform = {
  operatingSystem: "linux" | "macos" | "windows" | "unknown";
  architecture: "arm64" | "x64" | "unknown";
};

type UserAgentData = {
  platform?: string;
  mobile?: boolean;
  getHighEntropyValues?: (
    hints: string[],
  ) => Promise<{ architecture?: string; bitness?: string }>;
};

function normalizeOperatingSystem(
  navigatorValue: Navigator,
  userAgentData?: UserAgentData,
): BuzzDownloadPlatform["operatingSystem"] {
  const userAgent = navigatorValue.userAgent.toLowerCase();
  const platform = (
    userAgentData?.platform ??
    navigatorValue.platform ??
    ""
  ).toLowerCase();

  // Compatibility tokens are treacherous: iPadOS can report MacIntel and a
  // Macintosh UA, while Android and ChromeOS expose Linux platform strings.
  // Reject non-desktop devices before admitting desktop-looking signals.
  const isIPadDesktopMode =
    platform === "macintel" && navigatorValue.maxTouchPoints > 1;
  const isUnsupportedDevice =
    userAgentData?.mobile === true ||
    isIPadDesktopMode ||
    /android|iphone|ipad|ipod|mobile|tablet|windows phone|iemobile|opera mini|opera mobi|webos|blackberry|bb10|kindle|silk|kaios|cros/.test(
      userAgent,
    );
  if (isUnsupportedDevice) return "unknown";

  if (
    platform === "macos" ||
    platform.startsWith("mac") ||
    userAgent.includes("macintosh")
  )
    return "macos";
  if (
    platform === "windows" ||
    platform.startsWith("win") ||
    userAgent.includes("windows nt")
  )
    return "windows";
  if (
    platform === "linux" ||
    platform.startsWith("linux") ||
    userAgent.includes("linux")
  )
    return "linux";
  return "unknown";
}

function normalizeArchitecture(
  value: string,
): BuzzDownloadPlatform["architecture"] {
  const normalized = value.toLowerCase();
  if (/arm|aarch64/.test(normalized)) return "arm64";
  if (/x86|x64|amd64|64/.test(normalized)) return "x64";
  return "unknown";
}

export async function detectBuzzDownloadPlatform(
  navigatorValue: Navigator,
): Promise<BuzzDownloadPlatform> {
  const userAgentData = (
    navigatorValue as Navigator & { userAgentData?: UserAgentData }
  ).userAgentData;
  const operatingSystem = normalizeOperatingSystem(
    navigatorValue,
    userAgentData,
  );
  let architecture = normalizeArchitecture(navigatorValue.userAgent);

  if (userAgentData?.getHighEntropyValues) {
    try {
      const values = await userAgentData.getHighEntropyValues([
        "architecture",
        "bitness",
      ]);
      architecture = normalizeArchitecture(
        `${values.architecture ?? ""} ${values.bitness ?? ""}`,
      );
    } catch {
      // Privacy settings may reject high-entropy client hints; the user-agent
      // guess above stands.
    }
  }

  return { operatingSystem, architecture };
}

/** Where the invite page sends a visitor on `platform`. */
export async function resolveBuzzDownloadUrlForPlatform(
  platform: BuzzDownloadPlatform,
): Promise<string> {
  switch (platform.operatingSystem) {
    case "linux":
    case "macos":
    case "windows":
      // Desktop visitors choose their build on the releases page, which
      // states each installer's alpha/unsigned status.
      return BUZZ_RELEASES_URL;
    case "unknown":
      // No Buzz build targets phones or unrecognised devices from this page
      // yet; the releases page is the same fallback as before.
      return BUZZ_RELEASES_URL;
  }
}

export async function resolveBuzzDownloadUrl(): Promise<string> {
  return resolveBuzzDownloadUrlForPlatform(
    await detectBuzzDownloadPlatform(navigator),
  );
}

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
