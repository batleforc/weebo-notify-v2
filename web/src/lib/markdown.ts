/**
 * Markdown with raw HTML never interpreted (§2.3, §10.3). Gates: markdown-it
 * `html: false`, DOMPurify anyway, http(s)-only links opening with `noopener`.
 * The CSP (`lib/csp.ts`) is the fourth.
 */

import DOMPurify from "dompurify";
import MarkdownIt from "markdown-it";

const markdown = new MarkdownIt({
  html: false,
  linkify: true,
  breaks: true,
});

// Anything that is not http(s) is dropped rather than rewritten: a link that
// silently becomes something else is worse than a link that is not a link.
const SAFE_SCHEME = /^https?:\/\//i;

/** Whether a URL is one this app will put behind a link: http(s), nothing else. */
export function isWebUrl(url: string): boolean {
  return SAFE_SCHEME.test(url);
}

const defaultLinkOpen =
  markdown.renderer.rules.link_open ??
  ((tokens, index, options, _env, self) =>
    self.renderToken(tokens, index, options));

markdown.renderer.rules.link_open = (tokens, index, options, env, self) => {
  const token = tokens[index];
  // `attrGet` is typed loosely enough to hand back a number; a scheme check on
  // something that is not a string is exactly the hole this function closes.
  const href = String(token.attrGet("href") ?? "");
  if (!SAFE_SCHEME.test(href)) {
    token.attrSet("href", "#");
    token.attrSet("data-blocked", "true");
  } else {
    token.attrSet("target", "_blank");
    token.attrSet("rel", "noopener noreferrer");
  }
  return defaultLinkOpen(tokens, index, options, env, self);
};

export function renderMarkdown(source: string): string {
  return DOMPurify.sanitize(markdown.render(source), {
    ALLOWED_TAGS: [
      "p",
      "br",
      "strong",
      "em",
      "del",
      "code",
      "pre",
      "blockquote",
      "ul",
      "ol",
      "li",
      "a",
      "h1",
      "h2",
      "h3",
      "h4",
      "hr",
      "table",
      "thead",
      "tbody",
      "tr",
      "th",
      "td",
    ],
    ALLOWED_ATTR: ["href", "target", "rel", "data-blocked"],
    // No images: a notification has its own `image` field (§2.3).
    FORBID_TAGS: ["style", "img", "svg", "iframe", "form", "input"],
  });
}
