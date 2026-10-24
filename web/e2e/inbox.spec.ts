import { expect, test } from "@playwright/test";

import { cleanup, client, freshChannel, signIn, upsert } from "./support";

test.afterEach(cleanup);

test("a lifecycle notification updates in place, live", async ({ page }) => {
  const http = await client(await signIn(page));
  const channel = await freshChannel(http);
  await page.goto(`/c/${channel}`);

  // A new arrival waits behind the "N new" separator so the list never jumps (§10.5).
  const title = `deploy api #${Date.now()}`;
  await upsert(http, channel, "deploy", {
    title,
    state: "working",
    status: "info",
  });
  const row = page.locator("li", { hasText: title });
  await expect(async () => {
    const reveal = page.getByRole("button", { name: /1 new|1 nouvelle/i });
    if (await reveal.isVisible()) await reveal.click();
    await expect(row).toBeVisible({ timeout: 1000 });
  }).toPass();
  await expect(row).toContainText(/working/i);

  await upsert(http, channel, "deploy", {
    title,
    state: "closed",
    status: "success",
  });
  await expect(row).toContainText(/success/i);
  await expect(row).toContainText(/2 revisions/i);
  await expect(page.locator("li", { hasText: title })).toHaveCount(1);
});

test("the detail panel shows the timeline and records an ack", async ({
  page,
}) => {
  const http = await client(await signIn(page));
  const channel = await freshChannel(http);
  const id = await upsert(http, channel, "pressure", {
    title: "node-3 memory pressure",
    body: "Eviction threshold reached on `node-3`.",
    state: "working",
    status: "warning",
    priority: "urgent",
    actions: [{ type: "ack", key: "ack", label: "Acknowledge" }],
  });

  await page.goto(`/c/${channel}/n/${id}`);
  const detail = page.getByRole("complementary");
  await expect(
    detail.getByRole("heading", { name: "node-3 memory pressure" }),
  ).toBeVisible();
  await expect(detail.locator("code", { hasText: "node-3" })).toBeVisible();

  await detail.getByRole("button", { name: /acknowledge/i }).click();
  // The timeline entry, not /acknowledg/: the button's own label matched that at once,
  // so the API check below raced the POST. Polled for the same reason.
  await expect(detail).toContainText("acknowledged (ack)");
  await expect
    .poll(async () => {
      const answers = await http.get(
        `/api/v1/notifications/${id}/interactions`,
      );
      return JSON.stringify(await answers.json());
    })
    .toContain('"ack"');
});

test("an admin creates and deletes a channel from the UI", async ({ page }) => {
  await signIn(page);
  const id = `e2e-ui-${Date.now().toString(36)}`;
  await page.goto("/admin");
  await page.getByRole("textbox", { name: /channel id|identifiant/i }).fill(id);
  await page
    .getByRole("button", { name: /create channel|créer le canal/i })
    .click();
  await expect(page.getByRole("navigation").getByText(id)).toBeVisible();

  page.once("dialog", (d) => d.accept());
  await page
    .getByRole("button", { name: /delete channel|supprimer le canal/i })
    .click();
  await expect(page.getByRole("navigation").getByText(id)).toHaveCount(0);
});

test("a channel manager adds a person found by name", async ({ page }) => {
  const http = await client(await signIn(page));
  const channel = await freshChannel(http);
  await page.goto("/admin");
  await page
    .getByRole("main")
    .getByRole("button", { name: new RegExp(channel) })
    .click();

  await page
    .getByRole("textbox", { name: /member id|identifiant du membre/i })
    .fill("alice");
  await page.getByRole("option", { name: /alice/i }).first().click();
  await page.getByRole("button", { name: /^\s*(add|ajouter)\s*$/i }).click();

  const res = await http.get(`/api/v1/channels/${channel}`);
  const members = (await res.json()).members as {
    principal: { kind: string };
  }[];
  expect(
    members.filter((m) => m.principal.kind === "user").length,
  ).toBeGreaterThan(0);
});

test("@mobile the bottom tab bar reaches settings and the theme applies", async ({
  page,
}) => {
  await signIn(page);
  const tabs = page.getByRole("navigation", { name: /sections/i });
  await expect(tabs).toBeVisible();
  await tabs.getByRole("link", { name: /settings|réglages/i }).click();
  await expect(page).toHaveURL(/\/settings$/);

  await page.getByRole("button", { name: /^(dark|sombre)$/i }).click();
  await expect(page.locator("html")).toHaveClass(/dark/);
  await page.getByRole("button", { name: /^(light|clair)$/i }).click();
  await expect(page.locator("html")).not.toHaveClass(/dark/);
});
