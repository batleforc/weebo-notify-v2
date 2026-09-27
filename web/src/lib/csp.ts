/**
 * The Content-Security-Policy this deployment ships with (design §10.3, ADR-0021).
 *
 * Build-time code that lives under `src/` so it shares the test suite — nothing in
 * the application imports it, so none of it reaches the bundle. `vite.config.ts`
 * calls it once, at build time, to emit `csp-template.txt` (below).
 *
 * # Why the policy is assembled here and rendered by nginx
 *
 * §10.3 asks for a strict CSP and `lib/markdown.ts` calls it the fourth gate after
 * `html: false`, DOMPurify and the scheme check. ADR-0019 first closed that gap
 * with a `<meta>` tag stamped at build time — but a `<meta>` policy is fixed when
 * the page is built, and §11 says a deployment is "start this image with these
 * env vars", not "build this image again" (ADR-0021 supersedes ADR-0019 on that
 * point). So this function is now called twice, in two different shapes:
 *
 *   * by `vite.config.ts`, at build time, with **shell placeholders** in place of
 *     `apiOrigin`/`oidcIssuer`/`attachmentOrigin` (`${WNK_API_BASE_URL}` and so
 *     on) and the **real** inline-script hash, because the hash names bytes this
 *     build shipped and the origins do not exist yet. The result is written to
 *     `csp-template.txt`, which ships inside the image;
 *   * by `csp.spec.ts`, with real example origins, to pin the policy's shape.
 *
 * `web/nginx/docker-entrypoint.d/05-runtime-config.sh` resolves the placeholders
 * against the container's actual environment at boot and hands the result to
 * nginx as one `Content-Security-Policy` header — which is also why
 * `frame-ancestors` and `report-to` are ordinary entries in the list below now:
 * a header can carry everything a `<meta>` tag could not.
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

/** What the policy has to be told about the deployment it names. */
export interface PolicyInputs {
  /**
   * The origin the API is reachable at — already an origin, not a URL to derive
   * one from.
   *
   * That split matters here specifically: the build-time caller passes
   * `${WNK_API_BASE_URL}`, a shell placeholder that is not a parseable URL yet
   * and must not be run through {@link originOf}. Extracting the origin is now
   * the caller's job — `csp.spec.ts` does it with real URLs, `vite.config.ts`
   * does not do it at all.
   */
  apiOrigin: string;
  /** The identity provider's origin — discovery, JWKS and the token endpoint. */
  oidcOrigin: string;
  /**
   * Where presigned attachment URLs point (§6.1), already an origin.
   *
   * Optional, and the only input here that is. The other two are what the
   * application *is*; this one is what its object store happens to be, and a
   * deployment that publishes no images never needs it. Absent, `img-src` names
   * it not at all and an attachment renders as the caption
   * `NotificationImage.vue` shows — which is a visible failure, not a silent one.
   *
   * It cannot be derived: `GET /attachments/{id}` answers with a URL minted by
   * whatever `[storage] endpoint` the server was configured with, and this
   * bundle never sees the server's configuration.
   */
  attachmentOrigin?: string;
  /** Source expressions from {@link inlineScriptHashes}. */
  hashes: string[];
  /**
   * Extra `connect-src` sources the host shell needs — the desktop's IPC
   * channel (`scripts/tauri-csp.ts`). Absent for the browser.
   */
  connect?: string[];
}

/**
 * Build the policy for one deployment.
 *
 * Starts from `default-src 'none'` and adds back only what the application is
 * observed to need. The alternative — starting permissive and subtracting — is how
 * a policy ends up allowing something nobody can explain.
 */
export function contentSecurityPolicy({
  apiOrigin: api,
  oidcOrigin: issuer,
  attachmentOrigin,
  hashes,
  connect = [],
}: PolicyInputs): string {
  // Not run through `originOf`: the optional bucket origin arrives already an
  // origin (or already a shell placeholder, at build time) — same reasoning as
  // `apiOrigin`/`oidcOrigin` above.
  const bucket = attachmentOrigin || null;

  return [
    "default-src 'none'",
    // No `'unsafe-inline'`, which is the whole point (§10.3): a body published by
    // a CI job reaches readers holding an access token in memory, and the one
    // inline script this page has is named by its hash.
    ["script-src 'self'", ...hashes].join(" "),
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
    //
    // `blob:` for avatars and thumbnails, which are fetched with the bearer token
    // and shown as object URLs (`lib/object-url.ts`) — an `<img>` cannot send the
    // header their routes require. A `blob:` URL can only be minted by script
    // already running on this origin, so it admits nothing `script-src` has not.
    ["img-src 'self' data: blob:", api, bucket].filter(Boolean).join(" "),
    "font-src 'self'",
    // The API and the identity provider, and nothing else. This is the directive
    // that makes a stolen token hard to send anywhere: an injected script that
    // cannot reach an attacker's host has to exfiltrate through one of these two.
    ["connect-src 'self'", ...connect, api, issuer].join(" "),
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
    // The two directives ADR-0019 said a `<meta>` tag ignores — `'none'` and not
    // `'self'` because nothing in this application frames itself, and the
    // clickjacking this prevents is aimed at the action buttons of §2.3, a
    // transparent frame over "Approve" being the whole attack. `Reporting-Endpoints`
    // (a separate header, `web/nginx/default.conf.template`) defines what `csp`
    // names here.
    "frame-ancestors 'none'",
    "report-to csp",
  ].join("; ");
}
