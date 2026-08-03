import { apiBaseUrl } from "@/config";
import type { CreateClientConfig } from "./generated/client.gen";

/**
 * Configuration the generated client picks up at import time (ADR-0009).
 *
 * Only the base URL lives here. Authentication is an interceptor installed by
 * `installAuth()` in `./client.ts` instead of a static header, because an access
 * token has a fifteen-minute life (§12.5) and a header captured once would go
 * stale in the middle of a session.
 */
export const createClientConfig: CreateClientConfig = (config) => ({
  ...config,
  baseURL: apiBaseUrl,
});
