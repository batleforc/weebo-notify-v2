import { describe, expect, it } from "vitest";

import { renderMarkdown } from "./markdown";

/**
 * The rule these tests hold: a notification body is content from a publisher who
 * may be less trusted than the person reading it (§2.3, §10.3). None of it becomes
 * markup that runs.
 */
describe("renderMarkdown", () => {
  it("renders ordinary markdown", () => {
    const html = renderMarkdown("**gras** et `code`");

    expect(html).toContain("<strong>gras</strong>");
    expect(html).toContain("<code>code</code>");
  });

  it("never interprets raw HTML", () => {
    const html = renderMarkdown(
      "<script>alert(1)</script><img src=x onerror=y>",
    );

    // The characters survive as text — that is the point, a body quoting HTML is
    // still readable — but no tag is produced, so nothing runs.
    expect(html).not.toMatch(/<(script|img)\b/);
    expect(html).toContain("&lt;script&gt;");
  });

  it("does not turn a javascript: URL into a link at all", () => {
    const html = renderMarkdown("[clique](javascript:alert(1))");

    expect(html).not.toMatch(/<a\b/);
  });

  it("does not turn a data: URL into a link either", () => {
    const html = renderMarkdown(
      "[doc](data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==)",
    );

    expect(html).not.toMatch(/<a\b/);
  });

  it("blocks a scheme the markdown parser would have allowed", () => {
    // `mailto:` passes markdown-it's own validation, so this is the case our
    // link rule exists for: §2.3 says http(s) and nothing else.
    const html = renderMarkdown("[écris-moi](mailto:alice@weebo.local)");

    expect(html).toContain('data-blocked="true"');
    expect(html).not.toContain("mailto:");
  });

  it("opens an http(s) link in a new tab, without handing it our window", () => {
    const html = renderMarkdown("[runbook](https://wiki.example/runbook)");

    expect(html).toContain('href="https://wiki.example/runbook"');
    expect(html).toContain('target="_blank"');
    expect(html).toContain('rel="noopener noreferrer"');
  });

  it("strips an image even when it comes from markdown syntax", () => {
    // §2.3 gives a notification one `image` field the client controls; an
    // uncontrolled request to a third party is not the same thing.
    expect(
      renderMarkdown("![](https://tracker.example/pixel.gif)"),
    ).not.toContain("<img");
  });
});
