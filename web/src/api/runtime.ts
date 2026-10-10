import { apiBaseUrl } from "@/config";
import type { CreateClientConfig } from "./generated/client.gen";

/**
 * Base URL and timeout for the generated client (ADR-0009), set here because `setConfig`
 * drops `client.instance.defaults`. axios defaults to no timeout. Auth is an interceptor:
 * tokens rotate (§12.5).
 */
export const createClientConfig: CreateClientConfig = (config) => ({
  ...config,
  baseURL: apiBaseUrl,
  timeout: 30_000,
});
