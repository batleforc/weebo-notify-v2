import { apiBaseUrl } from "@/config";
import type { CreateClientConfig } from "./generated/client.gen";

/** Base URL for the generated client (ADR-0009). Auth is an interceptor: tokens rotate (§12.5). */
export const createClientConfig: CreateClientConfig = (config) => ({
  ...config,
  baseURL: apiBaseUrl,
});
