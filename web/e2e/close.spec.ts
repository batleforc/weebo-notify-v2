import { expect, test } from "@playwright/test";

import { cleanup, client, freshChannel, signIn, upsert } from "./support";

test.afterEach(cleanup);

/** RFC 0004 §6.P 1, 2, 7 and 8: alice manages the channels she creates. */
test("a manager closes a notification by hand, live in every tab", async ({
  page,
  context,
}) => {
  const http = await client(await signIn(page));
  const channel = await freshChannel(http);
  const title = `CVE-2026-1234 #${Date.now()}`;
  const id = await upsert(http, channel, "cve", {
    title,
    state: "open",
    status: "warning",
    priority: "high",
  });

  // A second tab already on the inbox, to see the closure arrive (§6.P 2).
  // Its own sign-in: the app keeps its session per tab.
  const other = await context.newPage();
  await signIn(other);
  await other.goto(`/c/${channel}`);
  const row = other.locator("li", { hasText: title });
  // The row says its state through the status matrix's accessible name.
  const state = row.getByRole("img");
  await expect(state).toHaveAccessibleName(/open|ouvert/i);

  await page.goto(`/c/${channel}/n/${id}`);
  const detail = page.getByRole("complementary");
  await expect(detail.getByRole("heading", { name: title })).toBeVisible();

  // Two buttons, two names (§6.P 8): the panel's own Close stays apart.
  const closeNotification = detail.getByRole("button", {
    name: /close notification|clore la notification/i,
  });
  await expect(closeNotification).toBeVisible();

  // With the keyboard (§11): focus it, Enter.
  await closeNotification.focus();
  const closed = page.waitForResponse(
    (r) => r.url().endsWith(`/notifications/${id}/close`) && r.ok(),
  );
  await page.keyboard.press("Enter");
  await closed;

  await expect(closeNotification).toHaveCount(0);
  await expect(detail).toContainText(/by |par /);
  await expect(state).toHaveAccessibleName(/closed|fermé/i);

  // Idempotent (§6.P 7): the same close again keeps the first closer.
  const again = await http.post(`/api/v1/notifications/${id}/close`);
  expect(again.ok()).toBeTruthy();
  const body = await again.json();
  expect(body.head.state).toBe("closed");
  expect(body.closed_by).toBeTruthy();
});
