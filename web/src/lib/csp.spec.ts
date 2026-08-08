/**
 * The policy is a security control that fails in two directions — too loose and it
 * protects nothing, too tight and the application is a blank page — so both are
 * pinned here.
 */

import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { describe, expect, it } from "vitest";

import { contentSecurityPolicy, inlineScriptHashes, originOf } from "./csp";

const INPUTS = {
  apiOrigin: "https://notify.test",
  oidcOrigin: "https://idp.test",
  hashes: ["'sha256-abc'"],
};

/** The directive named `name`, as its list of sources. */
function directive(policy: string, name: string): string[] {
  const found = policy
    .split(";")
    .map((part) => part.trim())
    .find((part) => part.startsWith(`${name} `) || part === name);
  expect(found, `${name} is missing from the policy`).toBeDefined();
  return (found as string).split(/\s+/).slice(1);
}

describe("the shipped policy", () => {
  it("lets nothing through by default", () => {
    // Everything below is an exception to this line. If it ever stops being
    // `'none'`, the exceptions stop meaning anything.
    expect(directive(contentSecurityPolicy(INPUTS), "default-src")).toEqual([
      "'none'",
    ]);
  });

  it("refuses inline scripts and names the one it has by hash", () => {
    const scripts = directive(contentSecurityPolicy(INPUTS), "script-src");

    expect(scripts).not.toContain("'unsafe-inline'");
    expect(scripts).not.toContain("'unsafe-eval'");
    expect(scripts).toContain("'self'");
    expect(scripts).toContain("'sha256-abc'");
  });

  it("can only talk to the API and the identity provider", () => {
    // The directive that decides where an injected script could send a token.
    expect(directive(contentSecurityPolicy(INPUTS), "connect-src")).toEqual([
      "'self'",
      "https://notify.test",
      "https://idp.test",
    ]);
  });

  it("allows the inline images §2.3 permits, and no third-party origin", () => {
    const images = directive(contentSecurityPolicy(INPUTS), "img-src");

    expect(images).toEqual(["'self'", "data:", "https://notify.test"]);
    expect(images).not.toContain("https:");
  });

  it("names the object store when there is one, so attachments can render", () => {
    // D26's second half. `contentSecurityPolicy` no longer strips a path off this
    // value itself (ADR-0021) — that was only ever safe when the caller held a
    // real URL to call `originOf` on, and the build-time caller now hands over a
    // shell placeholder it cannot parse. Passing something with a path in it is
    // the deployer's mistake to make and the deployment's own docs to prevent —
    // this test pins that `contentSecurityPolicy` uses the value verbatim rather
    // than silently swallowing the difference.
    const images = directive(
      contentSecurityPolicy({
        ...INPUTS,
        attachmentOrigin: "https://s3.weebo.test",
      }),
      "img-src",
    );

    expect(images).toContain("https://s3.weebo.test");
  });

  it("stays valid for a deployment that has no object store", () => {
    // An empty value must not become `img-src 'self' data: … undefined`, which is
    // a directive with a garbage source in it rather than one source fewer.
    const policy = contentSecurityPolicy({ ...INPUTS, attachmentOrigin: "" });

    expect(directive(policy, "img-src")).toEqual([
      "'self'",
      "data:",
      "https://notify.test",
    ]);
    expect(policy).not.toContain("undefined");
  });

  it("keeps a page from being repointed by an injected base tag", () => {
    expect(directive(contentSecurityPolicy(INPUTS), "base-uri")).toEqual([
      "'none'",
    ]);
  });

  it("carries the two directives a <meta> tag could never enforce (ADR-0021)", () => {
    const policy = contentSecurityPolicy(INPUTS);

    expect(directive(policy, "frame-ancestors")).toEqual(["'none'"]);
    expect(directive(policy, "report-to")).toEqual(["csp"]);
  });

  it("carries the port when the deployment has one, and drops the path", () => {
    // A source expression with a path in it matches nothing, and does so quietly.
    expect(originOf("http://localhost:8080/api/v1")).toBe(
      "http://localhost:8080",
    );
  });
});

describe("the inline script hashes", () => {
  it("cover the script bytes exactly, whitespace included", () => {
    const body = '\n  console.log("theme");\n';
    const expected = createHash("sha256").update(body, "utf8").digest("base64");

    expect(inlineScriptHashes(`<html><script>${body}</script></html>`)).toEqual(
      [`'sha256-${expected}'`],
    );
  });

  it("ignores scripts loaded from a file, which the policy covers by origin", () => {
    const html =
      '<script type="module" src="/assets/index.js"></script><script>a=1</script>';

    expect(inlineScriptHashes(html)).toHaveLength(1);
  });

  it("covers every inline script the shipped index.html actually has", () => {
    // The test that would have caught the mistake worth catching: the page has
    // one inline script (the pre-paint theme switch, §10.2), and a second one
    // added without a thought for the policy would be blocked in the browser and
    // nowhere else. This fails first instead.
    const html = readFileSync(resolve(process.cwd(), "index.html"), "utf8");
    const hashes = inlineScriptHashes(html);

    expect(hashes).toHaveLength(1);
    expect(hashes[0]).toMatch(/^'sha256-[A-Za-z0-9+/]+=*'$/);
  });
});
