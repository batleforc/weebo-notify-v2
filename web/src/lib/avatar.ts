/** Avatars (§2.4): the route needs the bearer header an `<img src>` can't send (§5.1, zero cookies). */

import { readAvatar } from "@/api/generated";
import { objectUrlCache } from "./object-url";

/** Never rejects; on failure the initials show. Caching: `./object-url`. */
export const avatarObjectUrl = objectUrlCache((id) =>
  // `blob`, or axios decodes the image as text.
  readAvatar({ path: { id }, responseType: "blob" }),
);
