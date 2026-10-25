/**
 * Scenario 02 — Live fan-out (audit point 4)
 *
 * PERF_STREAMS WebSocket subscribers (default 1 000, 20 per identity, the per-principal
 * ceiling) stay connected to /ws while publishers post PERF_RATE notifications/s (default
 * 100) to `perf-load`. Every publication pokes every stream, which is the storm point 4
 * was about: the run fails if streams drop or delivery falls behind.
 *
 * `delivery` is publish → frame received, read from the `perf-sent-<ms>` title; the
 * publisher and the subscribers share the k6 clock. /ws runs the same backlog code as
 * /stream (SSE), which k6 cannot hold open.
 *
 * Over several replicas: `task perf:replica` too, then PERF_REPLICAS=http://localhost:8081;
 * the sockets alternate between replicas, publications all go to the first.
 *
 * Pre-requisite: `task perf:stack` with PERF_USERS >= PERF_STREAMS / 20 (per replica).
 * Run: `task perf:run:fanout` (thresholds enforced).
 */
import { check, fail, sleep } from "k6";
import { Counter, Trend } from "k6/metrics";
import ws from "k6/ws";
import { STREAMS_PER_USER, USERS, WS_URLS } from "../config.js";
import { auth, loginAll, publish, publisherToken, tokenOf } from "../helpers.js";

const STREAMS = Number(__ENV.PERF_STREAMS || 1000);
const RATE = Number(__ENV.PERF_RATE || 100);
const RAMP = 20; // seconds over which the subscribers connect
const PUBLISH = 180; // seconds of publishing, once every stream is up
const HOLD = (RAMP + 10 + PUBLISH + 10) * 1000;

const delivery = new Trend("delivery", true);
const frames = new Counter("frames");
const dropped = new Counter("streams_dropped");

export const options = {
  setupTimeout: "120s",
  scenarios: {
    subscribers: {
      executor: "per-vu-iterations",
      exec: "subscribe",
      vus: STREAMS,
      iterations: 1,
      maxDuration: `${HOLD / 1000 + 30}s`,
    },
    publishers: {
      executor: "constant-arrival-rate",
      exec: "publisher",
      rate: RATE,
      timeUnit: "1s",
      duration: `${PUBLISH}s`,
      startTime: `${RAMP + 10}s`,
      preAllocatedVUs: 20,
      maxVUs: 100,
    },
  },
  thresholds: {
    http_req_failed: ["rate<0.01"],
    checks: ["rate>0.99"],
    // A stream that closes on its own is the storm starting.
    streams_dropped: ["count==0"],
    delivery: ["p(95)<1000", "p(99)<3000"],
    // Per replica, so a run over PERF_REPLICAS shows what crossing the bus costs.
    ...Object.fromEntries(WS_URLS.map((_, i) => [`delivery{replica:${i}}`, ["p(95)<1000"]])),
    "http_req_duration{name:publish}": ["p(95)<500"],
  },
};

export function setup() {
  if (USERS * STREAMS_PER_USER < STREAMS) {
    fail(`${STREAMS} streams need PERF_USERS >= ${Math.ceil(STREAMS / STREAMS_PER_USER)}`);
  }
  return { tokens: loginAll() };
}

export function subscribe({ tokens }) {
  // Spread the handshakes instead of opening every socket in the same millisecond.
  const delay = ((__VU - 1) / STREAMS) * RAMP * 1000;
  sleep(delay / 1000);
  const start = Date.now();
  let closing = false;

  // VU n uses identity n mod USERS, so each identity holds STREAMS / USERS sockets.
  const replica = __VU % WS_URLS.length;
  const tags = { replica: String(replica) };
  const res = ws.connect(WS_URLS[replica], { headers: auth(tokenOf(tokens)) }, (socket) => {
    socket.on("message", (text) => {
      frames.add(1);
      const sent = /perf-sent-(\d+)/.exec(text);
      // Ignore the backlog replayed on connect: only what was published since counts.
      if (sent && Number(sent[1]) >= start) delivery.add(Date.now() - Number(sent[1]), tags);
    });
    socket.on("close", () => {
      if (!closing) dropped.add(1);
    });
    socket.setTimeout(() => {
      closing = true;
      socket.close();
    }, HOLD - delay);
  });
  check(res, { "ws 101": (r) => r && r.status === 101 });
}

export function publisher({ tokens }) {
  publish(publisherToken(tokens));
}
