/**
 * The CSP (§10.3, ADR-0021). Build-time only: `vite.config.ts` renders it with
 * shell placeholders into `csp-template.txt`, which nginx resolves at boot.
 * Inline-script hashes are computed from the HTML, never pasted.
 */

import { createHash } from "node:crypto";

/** `<script>` blocks with no `src`, which are what a hash-based policy has to name. */
const INLINE_SCRIPT = /<script(?![^>]*\ssrc=)[^>]*>([\s\S]*?)<\/script>/gi;

/** `'sha256-…'` of every inline script's exact bytes, as the browser hashes them. */
export function inlineScriptHashes(html: string): string[] {
  return [...html.matchAll(INLINE_SCRIPT)].map(
    (match) =>
      `'sha256-${createHash("sha256").update(match[1], "utf8").digest("base64")}'`,
  );
}

/** A CSP source is an origin; a path would silently match nothing. */
export function originOf(url: string): string {
  return new URL(url).origin;
}

export interface PolicyInputs {
  /** Already an origin: at build time it is a shell placeholder, not a parseable URL. */
  apiOrigin: string;
  oidcOrigin: string;
  /** Presigned attachment origin (§6.1); optional, absent means attachments don't render. */
  attachmentOrigin?: string;
  /** Source expressions from {@link inlineScriptHashes}. */
  hashes: string[];
  /** Extra `connect-src` for the desktop IPC channel (`scripts/tauri-csp.ts`). */
  connect?: string[];
}

/** Starts from `default-src 'none'` and adds back only what is needed. */
export function contentSecurityPolicy({
  apiOrigin: api,
  oidcOrigin: issuer,
  attachmentOrigin,
  hashes,
  connect = [],
}: PolicyInputs): string {
  return [
    "default-src 'none'",
    // No `'unsafe-inline'` (§10.3).
    ["script-src 'self'", ...hashes].join(" "),
    // reka-ui positions popovers via `style` attributes, which can't be hashed.
    "style-src 'self' 'unsafe-inline'",
    // `data:` for payload images (§2.3), `blob:` for bearer-fetched object URLs,
    // the bucket for presigned attachments (§6.1). Never `https:`.
    ["img-src 'self' data: blob:", api, attachmentOrigin]
      .filter(Boolean)
      .join(" "),
    "font-src 'self'",
    // Limits where an injected script could exfiltrate a token.
    ["connect-src 'self'", ...connect, api, issuer].join(" "),
    "manifest-src 'self'",
    "worker-src 'self'",
    "form-action 'self'",
    // An injected `<base>` would repoint every relative URL.
    "base-uri 'none'",
    // Clickjacking over the action buttons (§2.3). `csp` is defined in nginx's `Reporting-Endpoints`.
    "frame-ancestors 'none'",
    "report-to csp",
  ].join("; ");
}
