/**
 * The generated client wired to the session: every request gets `bearer()`, and a
 * 401 gets one retry after a forced refresh (clock skew, mid-session revocation).
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
