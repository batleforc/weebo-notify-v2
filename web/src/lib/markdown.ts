/**
 * Markdown rendering, under the rule §2.3 sets: **raw HTML is never interpreted**.
 *
 * The threat is concrete and stated in §10.3: a channel's publishers can be less
 * trustworthy than its readers. A CI job publishes a body containing whatever a
 * branch name or a test failure put in it, and the people reading it hold an
 * access token in memory.
 *
 * Three gates, and each one alone would be a defence:
 *
 *   1. markdown-it runs with `html: false`, so a `<script>` in the source comes out
 *      as text rather than as a tag;
 *   2. the result goes through DOMPurify anyway, because "the parser cannot emit
 *      HTML" is a property of a dependency and dependencies get upgraded;
 *   3. links are restricted to `http(s)` — no `javascript:`, no `data:` — and open
 *      in a new tab with `noopener`, so a notification cannot navigate the app.
 *
 * The strict CSP §10.3 asks for is the fourth gate, and it lives in the server that
 * serves this bundle rather than in the bundle.
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

/** Render a notification body to sanitised HTML. */
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
    // Images are not in the list on purpose: §2.3 gives a notification one
    // `image` field, which the client renders itself and can size. An `<img>`
    // hidden in a body is an uncontrolled request to a third party.
    FORBID_TAGS: ["style", "img", "svg", "iframe", "form", "input"],
  });
}
