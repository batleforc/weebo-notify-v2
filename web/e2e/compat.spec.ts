import { expect, request, test } from "@playwright/test";

import { api, cleanup, client, freshChannel, signIn } from "./support";

test.afterEach(cleanup);

/** What an Uptime Kuma / Alertmanager setup holds: a key owned by the channel. */
async function channelKey(token: string, channel: string): Promise<string> {
  const http = await client(token);
  const res = await http.post("/api/v1/apikeys", {
    data: {
      owner: { owner: "channel", id: channel },
      channels: [channel],
      scopes: ["publish"],
    },
  });
  expect(res.ok(), await res.text()).toBeTruthy();
  return (await res.json()).secret as string;
}

test("Gotify, ntfy and Alertmanager senders land in the inbox", async ({
  page,
}) => {
  const token = await signIn(page);
  const channel = await freshChannel(await client(token));
  const key = await channelKey(token, channel);
  const anon = await request.newContext({ baseURL: api });

  // Gotify: the key in the query string, as Gotify clients send it.
  const gotify = await anon.post(`/api/v1/compat/gotify/message?token=${key}`, {
    data: {
      title: "gotify says hi",
      message: "from a gotify client",
      priority: 8,
    },
  });
  expect(gotify.ok(), await gotify.text()).toBeTruthy();

  // ntfy: raw text body, headers, Basic auth with the key as password.
  const ntfy = await anon.post(`/api/v1/compat/ntfy/${channel}`, {
    data: "disk 91% on nas-01",
    headers: {
      Title: "ntfy says hi",
      Priority: "high",
      Tags: "disk,nas",
      authorization: `Basic ${Buffer.from(`x:${key}`).toString("base64")}`,
    },
  });
  expect(ntfy.ok(), await ntfy.text()).toBeTruthy();

  // Alertmanager: firing then resolved is one notification that closes.
  const alert = (status: "firing" | "resolved") => ({
    version: "4",
    status,
    receiver: "weebo",
    groupKey: "{}",
    alerts: [
      {
        status,
        fingerprint: "e2e0001",
        labels: {
          alertname: "NodeDown",
          severity: "critical",
          instance: "node-7",
        },
        annotations: { summary: "node-7 unreachable" },
        startsAt: new Date().toISOString(),
      },
    ],
  });
  const auth = { authorization: `Bearer ${key}` };
  const firing = await anon.post(`/api/v1/compat/alertmanager/${channel}`, {
    data: alert("firing"),
    headers: auth,
  });
  expect(firing.ok(), await firing.text()).toBeTruthy();

  await page.goto(`/c/${channel}`);
  await expect(page.locator("li", { hasText: "gotify says hi" })).toBeVisible();
  await expect(page.locator("li", { hasText: "ntfy says hi" })).toBeVisible();
  const node = page.locator("li", { hasText: "NodeDown" });
  await expect(node).toContainText(/open|working/i);

  const resolved = await anon.post(`/api/v1/compat/alertmanager/${channel}`, {
    data: alert("resolved"),
    headers: auth,
  });
  expect(resolved.ok(), await resolved.text()).toBeTruthy();
  await expect(node).toContainText(/success/i);
  await expect(page.locator("li", { hasText: "NodeDown" })).toHaveCount(1);
});
