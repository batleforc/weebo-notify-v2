import { expect, test } from "@playwright/test";

import { signIn } from "./support";

test("a device sees the others and disconnects one for good", async ({
  browser,
}) => {
  // Two devices of the same person.
  const phone = await (
    await browser.newContext({
      userAgent: "Mozilla/5.0 (Linux; Android 14) Chrome/140.0 Mobile",
    })
  ).newPage();
  const laptop = await (await browser.newContext()).newPage();
  await signIn(phone);
  await expect(phone.getByText(/^\s*live\s*$/i)).toBeVisible();
  await signIn(laptop);

  await laptop.goto("/settings");
  await expect(laptop.getByText(/^\s*live\s*$/i)).toBeVisible();
  await laptop.getByRole("button", { name: /refresh|actualiser/i }).click();
  const row = laptop.locator("li", { hasText: "Chrome · Android" }).last();
  await expect(row).toBeVisible();
  await expect(
    laptop.locator("li", { hasText: /this device|cet appareil/i }),
  ).toHaveCount(1);

  await row.getByRole("button", { name: /disconnect|déconnecter/i }).click();

  // The phone is cut off now, and stays off rather than reconnecting.
  await expect(phone.getByText(/^\s*disconnected\s*$/i)).toBeVisible({
    timeout: 15_000,
  });
  await phone.waitForTimeout(3_000);
  await expect(phone.getByText(/^\s*disconnected\s*$/i)).toBeVisible();
});
