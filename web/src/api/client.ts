/**
 * The generated client, wired to the session.
 *
 * Two things happen here and nowhere else:
 *
 *   * every request gets its bearer token from `useSession().bearer()`, which
 *     refreshes on its own when the fifteen-minute access token is about to die;
 *   * a `401` that survives that gets exactly one retry after a forced refresh,
 *     for the case a proactive refresh could not catch — clock skew between the
 *     browser and the provider, or a token revoked mid-session.
 *
 * Anything past that is a session that has ended: the store flips `expired` and
 * the router sends the person back to the provider.
 */

import type { AxiosError, InternalAxiosRequestConfig } from "axios";

import { useSession } from "@/auth/session";
import { client } from "./generated/client.gen";

type Retryable = InternalAxiosRequestConfig & { _retried?: boolean };

let installed = false;

/** Wire the client to the session store. Called once, from `main.ts`. */
export function installAuth(): void {
  if (installed) {
    return;
  }
  installed = true;

  const session = useSession();

  // The security scheme is `bearer` (§8), so the generated layer adds the
  // prefix; this callback only ever returns the raw token.
  client.setConfig({
    ...client.getConfig(),
    auth: () => session.bearer().then((token) => token ?? undefined),
  });

  client.instance.interceptors.response.use(
    (response) => response,
    async (error: AxiosError) => {
      const request = error.config as Retryable | undefined;
      if (error.response?.status !== 401 || !request || request._retried) {
        return Promise.reject(error);
      }

      request._retried = true;
      const token = await session.refresh();
      if (!token) {
        return Promise.reject(error);
      }

      request.headers.set("Authorization", `Bearer ${token}`);
      return client.instance.request(request);
    },
  );
}

export { client };
