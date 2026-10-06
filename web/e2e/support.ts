import {
  type APIRequestContext,
  expect,
  type Page,
  request,
} from "@playwright/test";

export const api = process.env.WNK_E2E_API_URL ?? "http://localhost:8080";
const user = process.env.WNK_E2E_USER ?? "alice";
const password = process.env.WNK_E2E_PASSWORD ?? "weebo-dev";

/**
 * Signs in through the real identity provider and returns the access token the
 * SPA received. Works with any provider whose login form has `username` and
 * `password` fields (the oidc-mock, Dex, Authentik's flow executor).
 */
export async function signIn(page: Page): Promise<string> {
  await page.goto("/login");
  const token = page.waitForResponse(
    (r) => /\/token$/.test(new URL(r.url()).pathname) && r.ok(),
  );
  await page.getByRole("button", { name: /sign in|se connecter/i }).click();
  await page.locator('input[name="username"]').fill(user);
  await page.locator('input[name="password"]').fill(password);
  await page
    .locator('button[type="submit"], input[type="submit"]')
    .first()
    .click();
  const body = await (await token).json();
  await expect(page).toHaveURL(/\/(\?|$|c\/|n\/)/);
  return body.access_token as string;
}

export async function client(token: string): Promise<APIRequestContext> {
  return request.newContext({
    baseURL: api,
    extraHTTPHeaders: { authorization: `Bearer ${token}` },
  });
}

/** Channels this run created; `cleanup` deletes them even when a test failed. */
const created: { http: APIRequestContext; id: string }[] = [];

/** A throwaway dynamic channel, so runs never collide with each other or with seeded data. */
export async function freshChannel(http: APIRequestContext): Promise<string> {
  const id = `e2e-${Date.now().toString(36)}`;
  const res = await http.post("/api/v1/channels", {
    data: { id, display_name: id },
  });
  expect(res.ok(), await res.text()).toBeTruthy();
  created.push({ http, id });
  return id;
}

export async function cleanup(): Promise<void> {
  for (const { http, id } of created.splice(0)) {
    await http.delete(`/api/v1/channels/${id}`);
  }
}

export async function upsert(
  http: APIRequestContext,
  channel: string,
  externalId: string,
  revision: Record<string, unknown>,
): Promise<string> {
  const res = await http.put(
    `/api/v1/channels/${channel}/notifications/${externalId}`,
    {
      data: revision,
    },
  );
  expect(res.ok(), await res.text()).toBeTruthy();
  return (await res.json()).notification.id as string;
}
