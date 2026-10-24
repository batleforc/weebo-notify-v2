import { check, fail } from "k6";
import crypto from "k6/crypto";
import exec from "k6/execution";
import http from "k6/http";
import { API, CHANNEL, OIDC_URL, USERS } from "./config.js";

const CLIENT = "weebo-notify";
const REDIRECT = "http://localhost:5173/auth/callback";

/** Access token for one identity, through the same PKCE flow a browser runs (`task dev:cli`). */
export function login(user) {
  const verifier = crypto.hexEncode(crypto.randomBytes(32));
  const challenge = crypto.sha256(verifier, "base64rawurl");
  const page = http.get(
    `${OIDC_URL}/authorize?response_type=code&client_id=${CLIENT}` +
      `&redirect_uri=${encodeURIComponent(REDIRECT)}&scope=openid%20groups` +
      `&code_challenge=${challenge}&code_challenge_method=S256`,
  );
  const request = /name="request" value="([^"]+)"/.exec(page.body);
  if (!request) fail(`oidc mock: no login form for ${user} (is \`task perf:stack\` running?)`);

  const consent = http.post(
    `${OIDC_URL}/authorize`,
    { request: request[1], username: user, password: "weebo-dev" },
    { redirects: 0 },
  );
  const code = /[?&]code=([^&]+)/.exec(consent.headers.Location || "");
  if (!code) fail(`oidc mock: ${user} was not let in (status ${consent.status})`);

  const token = http.post(`${OIDC_URL}/token`, {
    grant_type: "authorization_code",
    code: code[1],
    code_verifier: verifier,
    redirect_uri: REDIRECT,
    client_id: CLIENT,
  });
  return token.json("access_token");
}

/** One token per perf identity; call from `setup()` so VUs share them. */
export function loginAll() {
  const tokens = [];
  for (let i = 1; i <= USERS; i++) tokens.push(login(perfUser(i)));
  // The first authenticated call provisions the account; do it before the load starts.
  for (const token of tokens) http.get(`${API}/channels`, { headers: auth(token) });
  return tokens;
}

export const perfUser = (i) => `perf-${String(i).padStart(3, "0")}`;

export const auth = (token) => ({ Authorization: `Bearer ${token}` });

/** The token this VU uses, spread evenly over the identities. */
export const tokenOf = (tokens) => tokens[(__VU - 1) % tokens.length];

/** Round-robin over the identities per publication, so none outruns its 60/min quota. */
export const publisherToken = (tokens) =>
  tokens[exec.scenario.iterationInTest % tokens.length];

/** Publish one notification stamped with the send time, which `02_fanout` reads back. */
export function publish(token) {
  const res = http.post(
    `${API}/notifications`,
    JSON.stringify({
      channel: CHANNEL,
      title: `perf-sent-${Date.now()}`,
      body: "k6 load run",
      state: "open",
      status: "info",
    }),
    { headers: { ...auth(token), "Content-Type": "application/json" }, tags: { name: "publish" } },
  );
  check(res, {
    "publish 201": (r) => r.status === 201,
    "publish has an id": (r) => r.status !== 201 || !!r.json("notification.id"),
  });
  return res;
}
