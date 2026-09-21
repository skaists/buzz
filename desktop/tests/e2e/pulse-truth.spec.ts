import { expect, test } from "@playwright/test";
import type { Page } from "@playwright/test";

import { installMockBridge } from "../helpers/bridge";

const RELAY_ERROR = "relay unreachable: connection refused (e2e)";

// Reject get_global_notes while window.__pulseFail is set. The app reads
// __TAURI_INTERNALS__.invoke at call time, so wrapping it after boot reaches
// every later Pulse fetch.
async function failGlobalNotes(page: Page, fail: boolean) {
  await page.waitForFunction(
    () =>
      typeof (window as { __TAURI_INTERNALS__?: { invoke?: unknown } })
        .__TAURI_INTERNALS__?.invoke === "function",
  );
  await page.evaluate(
    ({ fail, message }) => {
      const w = window as typeof window & {
        __pulseFail?: boolean;
        __pulseWrapped?: boolean;
        __TAURI_INTERNALS__: {
          invoke: (cmd: string, ...rest: unknown[]) => Promise<unknown>;
        };
      };
      w.__pulseFail = fail;
      if (w.__pulseWrapped) return;
      const internals = w.__TAURI_INTERNALS__;
      const invoke = internals.invoke.bind(internals);
      internals.invoke = (cmd, ...rest) =>
        cmd === "get_global_notes" && w.__pulseFail
          ? Promise.reject(message)
          : invoke(cmd, ...rest);
      w.__pulseWrapped = true;
    },
    { fail, message: RELAY_ERROR },
  );
}

const rows = (page: Page) => page.locator("#pulse-panel-search [data-index]");

test.describe("Pulse tells the truth", () => {
  test("a relay failure names its cause and offers Retry, never 'No public notes yet.'", async ({
    page,
  }) => {
    await installMockBridge(page);
    await page.goto("/");
    await failGlobalNotes(page, true);
    await page.getByTestId("open-pulse-view").click();

    const alert = page.locator("#pulse-panel-everyone [role=alert]");
    await expect(alert).toContainText(RELAY_ERROR, { timeout: 20_000 });
    await expect(page.getByText("No public notes yet.")).toHaveCount(0);

    await failGlobalNotes(page, false);
    await alert.getByRole("button", { name: "Retry" }).click();
    await expect(
      page.locator("#pulse-panel-everyone [data-index]").first(),
    ).toBeVisible();
    await expect(alert).toHaveCount(0);
  });

  test("search narrows by author and by text, live and on submit", async ({
    page,
  }) => {
    await installMockBridge(page);
    await page.goto("/");
    await page.getByTestId("open-pulse-view").click();
    await page.locator("#pulse-tab-search").click();

    const panel = page.locator("#pulse-panel-search");
    const input = panel.locator("input[type=search]");
    await expect(
      panel.getByText("Search Pulse notes by author or text."),
    ).toBeVisible();

    // By text only: one note says this.
    await input.fill("Release checklist");
    await expect(rows(page)).toHaveCount(1);
    await expect(rows(page).first()).toContainText(
      "Release checklist is ready for async feedback.",
    );

    // By author only: no note's text says "alice"; her two notes match by name.
    await input.fill("alice");
    await expect(rows(page)).toHaveCount(2);

    await input.fill("zzzz-no-such-note");
    await expect(rows(page)).toHaveCount(0);
    await expect(panel.getByText("No Pulse notes match")).toBeVisible();

    await input.fill("");
    await expect(
      panel.getByText("Search Pulse notes by author or text."),
    ).toBeVisible();
    await expect(rows(page)).toHaveCount(0);

    // The button submits the same query as typing (trimmed).
    await input.fill("  sidebar polish  ");
    await panel.locator("form button[type=submit]").click();
    await expect(input).toHaveValue("sidebar polish");
    await expect(rows(page)).toHaveCount(1);
    await expect(rows(page).first()).toContainText("sidebar polish");
  });

  test("keystroke to filtered list stays under 200 ms", async ({ page }) => {
    await installMockBridge(page);
    await page.goto("/");
    await page.getByTestId("open-pulse-view").click();
    await page.locator("#pulse-tab-search").click();
    const input = page.locator("#pulse-panel-search input[type=search]");
    await input.fill("Pulse update");
    await expect(rows(page).first()).toBeVisible();

    const timings: number[] = [];
    for (const [query, expected] of [
      ["lighter forum", "lighter forum"],
      ["Release checklist", "Release checklist"],
      ["sidebar polish", "sidebar polish"],
    ] as const) {
      const elapsed = await page.evaluate(
        ({ query, expected }) =>
          new Promise<number>((resolve, reject) => {
            const el = document.querySelector<HTMLInputElement>(
              "#pulse-panel-search input[type=search]",
            );
            if (!el) return reject(new Error("no search input"));
            const setValue = Object.getOwnPropertyDescriptor(
              HTMLInputElement.prototype,
              "value",
            )?.set;
            const started = performance.now();
            setValue?.call(el, query);
            el.dispatchEvent(new Event("input", { bubbles: true }));
            const check = () => {
              const first = document.querySelector(
                "#pulse-panel-search [data-index]",
              );
              if (first?.textContent?.includes(expected)) {
                resolve(performance.now() - started);
              } else if (performance.now() - started > 2_000) {
                reject(new Error(`no match for ${query}`));
              } else {
                requestAnimationFrame(check);
              }
            };
            requestAnimationFrame(check);
          }),
        { query, expected },
      );
      timings.push(elapsed);
    }
    console.log(
      `pulse search keystroke->rendered ms: ${timings.map((t) => t.toFixed(1)).join(", ")}`,
    );
    for (const t of timings) expect(t).toBeLessThan(200);
  });
});
