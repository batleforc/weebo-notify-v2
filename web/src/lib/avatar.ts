/**
 * Getting an avatar onto the page (design §2.4, §5.1).
 *
 * # Why this exists rather than `<img src="/api/v1/avatars/{id}">`
 *
 * That is what the settings page did, and it could never have worked. The route
 * is authenticated — its credential comes from the `Authorization` header and
 * from nowhere else — and design §5.1 keeps **zero cookies** so that there is no
 * CSRF surface. A browser issuing an `<img>` request sends neither, so every
 * avatar in this application answered `401` and rendered as a broken image.
 *
 * Avatars are the one upload the server serves itself (§2.4, §6.1): the bytes
 * come *through* the API rather than from a presigned bucket URL, precisely so
 * they can be cached hard and rendered on every row of an inbox without a
 * signing round trip. That decision is still the right one — what was missing is
 * that fetching them has to go through the same client as every other call, so
 * the bearer token and its refresh interceptor apply.
 *
 * Attachment images are deliberately *not* handled here: `GET /attachments/{id}`
 * answers with a **presigned** URL, and a signature travels in the URL, so those
 * can go straight into an `<img src>` with no fetch and no blob.
 */

import { readAvatar } from "@/api/generated";
import { objectUrlCache } from "./object-url";

/**
 * The object URL for an avatar, or `null` when it cannot be had.
 *
 * Never rejects. A missing or unreadable avatar is a face that does not render —
 * the fallback initials take over — and it must not take a settings page or an
 * inbox row down with it. Caching and its bounds: `./object-url`.
 */
export const avatarObjectUrl = objectUrlCache((id) =>
  // Through the generated client, which is the whole point: it carries the
  // bearer token and retries once behind the refresh interceptor. `blob`,
  // because without it axios decodes the image as text and mangles it.
  readAvatar({ path: { id }, responseType: "blob" }),
);
