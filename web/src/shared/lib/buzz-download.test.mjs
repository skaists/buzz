import { strict as assert } from "node:assert";
import test from "node:test";

import {
  BUZZ_RELEASES_URL,
  resolveBuzzDownloadUrlForPlatform,
  skaistsBuildUrl,
} from "./buzz-download.ts";

test("every platform, phones included, resolves to the releases page", async () => {
  for (const operatingSystem of ["linux", "macos", "windows", "unknown"]) {
    for (const architecture of ["arm64", "x64", "unknown"]) {
      assert.equal(
        await resolveBuzzDownloadUrlForPlatform({
          operatingSystem,
          architecture,
        }),
        BUZZ_RELEASES_URL,
        `${operatingSystem}/${architecture}`,
      );
    }
  }
});

test("no configured skaists build means no edition link", () => {
  assert.equal(skaistsBuildUrl(undefined), undefined);
  assert.equal(skaistsBuildUrl(""), undefined);
  assert.equal(skaistsBuildUrl("   "), undefined);
});

test("a configured https build URL is used as given", () => {
  const url = "https://github.com/skaists/buzz/releases";
  assert.equal(skaistsBuildUrl(url), url);
  assert.equal(skaistsBuildUrl(` ${url} `), url);
});

test("non-https or malformed values never become a link", () => {
  assert.equal(skaistsBuildUrl("http://example.com/buzz.exe"), undefined);
  assert.equal(skaistsBuildUrl("javascript:alert(1)"), undefined);
  assert.equal(skaistsBuildUrl("not a url"), undefined);
});
