/**
 * Scenario 03 — Inbox reads
 *
 * What a client does when it opens: list the inbox, count unread, catch up with /sync,
 * list its channels. Ramp 10 → 100 VU over the perf identities. Run it after
 * `perf:run:publish` so the inbox has a few thousand rows to page through.
 *
 * Pre-requisite: `task perf:stack` in another terminal.
 * Run: `task perf:run:read` (thresholds enforced).
 */
import { check, group } from "k6";
import http from "k6/http";
import { API, CHANNEL } from "../config.js";
import { auth, loginAll, tokenOf } from "../helpers.js";

export const options = {
  stages: [
    { duration: "30s", target: 10 }, // warm up
    { duration: "60s", target: 50 },
    { duration: "60s", target: 100 },
    { duration: "30s", target: 0 }, // cool down
  ],
  thresholds: {
    http_req_failed: ["rate<0.01"],
    checks: ["rate>0.99"],
    // Per endpoint, so the summary says which one is slow.
    "http_req_duration{name:inbox}": ["p(95)<300"],
    "http_req_duration{name:unread}": ["p(95)<300"],
    "http_req_duration{name:sync}": ["p(95)<300"],
    "http_req_duration{name:channels}": ["p(95)<300"],
    http_req_duration: ["p(99)<1000"],
  },
};

export const setup = () => ({ tokens: loginAll() });

function getJson(path, name, headers) {
  const res = http.get(`${API}${path}`, { headers, tags: { name } });
  check(res, {
    [`${name} 200`]: (r) => r.status === 200,
    [`${name} json`]: (r) => (r.headers["Content-Type"] || "").includes("json") && r.body.length > 0,
  });
}

export default function ({ tokens }) {
  const headers = auth(tokenOf(tokens));
  group("open inbox", () => {
    getJson(`/notifications?channels=${CHANNEL}&limit=50`, "inbox", headers);
    getJson("/unread", "unread", headers);
    getJson("/sync?limit=100", "sync", headers);
    getJson("/channels", "channels", headers);
  });
}
