/**
 * Scenario 01 — Publish burst
 *
 * POST /notifications to `perf-load` at a ramping arrival rate, 10 → 150 publications/s,
 * spread over the perf identities. Measures the write path alone: authorisation,
 * idempotency, the insert, the fan-out enqueue and the NOTIFY poke. No reader is connected.
 *
 * Pre-requisite: `task perf:stack` in another terminal.
 * Run: `task perf:run:publish` (thresholds enforced).
 */
import { loginAll, publish, publisherToken } from "../helpers.js";

export const options = {
  scenarios: {
    publish: {
      executor: "ramping-arrival-rate",
      startRate: 10,
      timeUnit: "1s",
      preAllocatedVUs: 50,
      maxVUs: 200,
      stages: [
        { duration: "30s", target: 50 }, // warm up
        { duration: "60s", target: 100 },
        { duration: "60s", target: 150 },
        { duration: "30s", target: 0 }, // cool down
      ],
    },
  },
  thresholds: {
    // Error rate below 1 %: hard requirement, in every scenario.
    http_req_failed: ["rate<0.01"],
    checks: ["rate>0.99"],
    // Latency targets: enforced by `perf:run:publish`, report-only in `perf:run:all`.
    "http_req_duration{name:publish}": ["p(95)<300", "p(99)<1000"],
  },
};

export const setup = () => ({ tokens: loginAll() });

export default function ({ tokens }) {
  publish(publisherToken(tokens));
}
