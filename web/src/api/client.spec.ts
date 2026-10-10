/**
 * The one retry `installAuth` adds on a `401` — and that it stays one.
 *
 * A retry loop on `401` is a client refreshing a token and hammering its own API
 * as fast as the network allows; no retry at all is a session dying on the first
 * clock skew. Both directions are pinned here against the real axios instance.
 */
import type { AxiosAdapter, InternalAxiosRequestConfig } from "axios";
import { beforeEach, describe, expect, it, vi } from "vitest";

const session = vi.hoisted(() => ({
  bearer: vi.fn<() => Promise<string | null>>(),
  refresh: vi.fn<() => Promise<string | null>>(),
}));

vi.mock("@/auth/session", () => ({ useSession: () => session }));

const { client, installAuth } = await import("./client");
const { unread } = await import("./generated");

/**
 * Answer each request with the next status, recording what was sent. Passed per
 * request rather than on `instance.defaults`: the generated client's `setConfig`
 * reassigns that object, which detaches it from what axios actually reads.
 */
function serve(...statuses: number[]): {
  seen: InternalAxiosRequestConfig[];
  adapter: AxiosAdapter;
} {
  const seen: InternalAxiosRequestConfig[] = [];
  const adapter: AxiosAdapter = async (config) => {
    seen.push(config);
    const status = statuses.shift() ?? 200;
    const response = {
      data: {},
      status,
      statusText: String(status),
      headers: {},
      config,
    };
    if (status >= 400) {
      const error = Object.assign(new Error(`HTTP ${status}`), {
        config,
        response,
        isAxiosError: true,
      });
      throw error;
    }
    return response;
  };
  return { seen, adapter };
}

describe("installAuth's 401 retry", () => {
  beforeEach(() => {
    session.bearer.mockReset().mockResolvedValue("token-1");
    session.refresh.mockReset().mockResolvedValue("token-2");
    installAuth();
  });

  it("retries once with the refreshed token", async () => {
    const { seen, adapter } = serve(401, 200);

    const response = await client.instance.request({
      url: "/api/v1/x",
      adapter,
    });

    expect(response.status).toBe(200);
    expect(seen).toHaveLength(2);
    expect(String(seen[1].headers.Authorization)).toBe("Bearer token-2");
    expect(session.refresh).toHaveBeenCalledOnce();
  });

  it("gives up when the refresh does not produce a token", async () => {
    session.refresh.mockResolvedValue(null);
    const { seen, adapter } = serve(401);

    await expect(
      client.instance.request({ url: "/api/v1/x", adapter }),
    ).rejects.toMatchObject({ response: { status: 401 } });
    expect(seen).toHaveLength(1);
  });

  it("does not retry a second time when the retry is refused too", async () => {
    const { seen, adapter } = serve(401, 401);

    await expect(
      client.instance.request({ url: "/api/v1/x", adapter }),
    ).rejects.toMatchObject({ response: { status: 401 } });
    expect(seen).toHaveLength(2);
    expect(session.refresh).toHaveBeenCalledOnce();
  });

  it("leaves other failures alone", async () => {
    const { seen, adapter } = serve(503);

    await expect(
      client.instance.request({ url: "/api/v1/x", adapter }),
    ).rejects.toMatchObject({ response: { status: 503 } });
    expect(seen).toHaveLength(1);
    expect(session.refresh).not.toHaveBeenCalled();
  });
});

/**
 * What a generated SDK call actually puts on the wire. Settings that only live on
 * `client.instance.defaults` never get here — the generated `setConfig` swaps that
 * object out from under axios — so origin, credentials and the time limit have to
 * come from the generated client's own config (`runtime.ts`, `installAuth`).
 */
describe("a generated SDK call", () => {
  beforeEach(() => {
    session.bearer.mockReset().mockResolvedValue("token-1");
    installAuth();
  });

  it("goes to the configured API origin, with the bearer and a time limit", async () => {
    const { seen, adapter } = serve(200);

    await unread({ adapter });

    expect(seen).toHaveLength(1);
    expect(new URL(String(seen[0].url)).origin).toBe("https://notify.test");
    expect(String(seen[0].headers.Authorization)).toBe("Bearer token-1");
    expect(seen[0].timeout).toBeGreaterThan(0);
  });
});
