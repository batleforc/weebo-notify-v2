// Everything a scenario reads from the environment, with the defaults `task perf:stack` serves.
export const BASE_URL = __ENV.PERF_URL || "http://localhost:8080";
export const API = `${BASE_URL}/api/v1`;
// Scenario 02 spreads its sockets over BASE_URL and PERF_REPLICAS (comma-separated base
// URLs, e.g. `task perf:replica`'s http://localhost:8081); publishers stay on BASE_URL.
export const WS_URLS = [BASE_URL, ...(__ENV.PERF_REPLICAS || "").split(",").filter(Boolean)].map(
  (url) => `${url.replace(/^http/, "ws")}/api/v1/ws`,
);
export const OIDC_URL = __ENV.PERF_OIDC_URL || "http://localhost:9110";

// The perf-NNN identities of `task perf:identities`; must match its PERF_USERS. Each may
// publish 60/min (burst 120) and hold 20 streams, so 200 carry 150 publications/s.
export const USERS = Number(__ENV.PERF_USERS || 200);
export const CHANNEL = __ENV.PERF_CHANNEL || "perf-load";

// One principal may hold 20 live connections per replica (MAX_STREAMS_PER_PRINCIPAL).
export const STREAMS_PER_USER = 20;
