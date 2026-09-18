import { strict as assert } from "node:assert";
import test from "node:test";

import { skaistsBuildUrl } from "./buzz-download.ts";

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
