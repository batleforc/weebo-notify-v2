/**
 * The Content-Security-Policy this bundle ships with (design §10.3, ADR-0019).
 *
 * Build-time code that lives under `src/` so it shares the test suite — nothing in
 * the application imports it, so none of it reaches the bundle. `vite.config.ts`
 * calls it from a `transformIndexHtml` hook.
 *
 * # Why the policy travels with the bundle
 *
 * §10.3 asks for a strict CSP and `lib/markdown.ts` calls it the fourth gate after
 * `html: false`, DOMPurify and the scheme check. Until now it was owed by "whatever
 * serves the bundle", which in this repository is nothing at all — so the gate was
 * a sentence in three documents and a header in zero deployments. A `<meta>` tag is
 * enforced by the browser exactly like the header, and it cannot be forgotten by
 * whoever writes the ingress.
 *
 * Two directives cannot travel this way — a meta policy ignores `frame-ancestors`
 * and `report-to` — and those stay owed by the server. Named in ADR-0019 rather
 * than left to be discovered.
 *
 * # The hashes are computed, never written down
 *
 * `index.html` carries one inline script (the pre-paint theme switch). Its hash is
 * read off the HTML being served rather than pasted into a constant, because a
 * pasted hash is one whitespace edit away from a blank page — and a blank page is
 * how a CSP mistake shows up.
 */

import { createHash } from "node:crypto";

/** `<script>` blocks with no `src`, which are what a hash-based policy has to name. */
const INLINE_SCRIPT = /<script(?![^>]*\ssrc=)[^>]*>([\s\S]*?)<\/script>/gi;

/**
 * The `'sha256-…'` source expressions for every inline script in `html`.
 *
 * The digest covers the exact bytes between the tags, whitespace included, which is
 * what the browser hashes.
 */
export function inlineScriptHashes(html: string): string[] {
  return [...html.matchAll(INLINE_SCRIPT)].map(
    (match) =>
      `'sha256-${createHash("sha256").update(match[1], "utf8").digest("base64")}'`,
  );
}

/**
 * The origin of a configured URL, for the directives that name one.
 *
 * A CSP source is an origin: keeping the path would produce a source expression
 * that matches nothing, silently.
 */
export function originOf(url: string): string {
  return new URL(url).origin;
}

/** What the policy has to be told about the deployment it is built for. */
export interface PolicyInputs {
  /** `VITE_API_BASE_URL` — the API this bundle talks to. */
  apiBaseUrl: string;
  /** `VITE_OIDC_ISSUER` — discovery, JWKS and the token endpoint. */
  oidcIssuer: string;
  /**
   * `VITE_ATTACHMENT_ORIGIN` — where presigned attachment URLs point (§6.1).
   *
   * Optional, and the only input here that is. The other two are what the
   * application *is*; this one is what its object store happens to be, and a
   * deployment that publishes no images never needs it. Absent, `img-src` names
   * it not at all and an attachment renders as the caption
   * `NotificationImage.vue` shows — which is a visible failure, not a silent one.
   *
   * It cannot be derived: `GET /attachments/{id}` answers with a URL minted by
   * whatever `[storage] endpoint` the server was configured with, and this bundle
   * has never seen the server's configuration. It cannot be read at runtime
   * either — a policy is fixed when the document is parsed, and by the time a
   * presign comes back the browser has long stopped listening.
   */
  attachmentOrigin?: string;
  /** Source expressions from {@link inlineScriptHashes}. */
  hashes: string[];
}

/**
 * Build the policy for one deployment.
 *
 * Starts from `default-src 'none'` and adds back only what the application is
 * observed to need. The alternative — starting permissive and subtracting — is how
 * a policy ends up allowing something nobody can explain.
 */
export function contentSecurityPolicy({
  apiBaseUrl,
  oidcIssuer,
  attachmentOrigin,
  hashes,
}: PolicyInputs): string {
  const api = originOf(apiBaseUrl);
  const issuer = originOf(oidcIssuer);
  // Not `originOf` on a possibly-absent value: an empty string would throw, and a
  // build must not fail over an optional input. An origin that is already an
  // origin passes through unchanged.
  const bucket = attachmentOrigin ? originOf(attachmentOrigin) : null;

  return [
    "default-src 'none'",
    // No `'unsafe-inline'`, which is the whole point (§10.3): a body published by
    // a CI job reaches readers holding an access token in memory, and the one
    // inline script this page has is named by its hash.
    `script-src 'self' ${hashes.join(" ")}`,
    // `'unsafe-inline'` here and nowhere else, and it is not laziness: reka-ui
    // positions its popovers and tooltips by writing `style` attributes, which
    // CSP counts as inline styles. Hashing them is impossible — they change with
    // the cursor. Style injection cannot exfiltrate a token the way script
    // injection can, so this is the one directive where the cost is worth it.
    "style-src 'self' 'unsafe-inline'",
    // `data:` for the inline images §2.3 allows in a payload (png/jpeg/webp,
    // validated server-side); the API origin for avatars; the object store for
    // the attachment half of the same field, which is served by presigned URL and
    // therefore from an origin that is not the API's (§6.1, D26). Bodies cannot
    // contain an `<img>` at all — DOMPurify forbids the tag — so beyond those
    // three no image origin is allowed, and in particular never `https:`.
    ["img-src 'self' data:", api, bucket].filter(Boolean).join(" "),
    "font-src 'self'",
    // The API and the identity provider, and nothing else. This is the directive
    // that makes a stolen token hard to send anywhere: an injected script that
    // cannot reach an attacker's host has to exfiltrate through one of these two.
    `connect-src 'self' ${api} ${issuer}`,
    "manifest-src 'self'",
    // The PWA service worker (§10.4), registered from this origin.
    "worker-src 'self'",
    // Nothing here submits a native form — every write goes through fetch — but
    // `'self'` rather than `'none'` keeps a future login form working without a
    // policy change nobody would connect to the failure.
    "form-action 'self'",
    // An injected `<base>` would repoint every relative URL in the page,
    // including the ones this policy allows.
    "base-uri 'none'",
  ].join("; ");
}
