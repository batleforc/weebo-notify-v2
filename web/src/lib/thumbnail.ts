/**
 * Row thumbnails (§2.3, D30), fetched through the API with the bearer (unlike
 * full attachments, which are presigned). Ids are immutable, so never stale.
 */

import { thumbnail } from "@/api/generated";
import { objectUrlCache } from "./object-url";

/** Never rejects; `null` when no thumbnail exists (D30). Caching: `./object-url`. */
export const thumbnailObjectUrl = objectUrlCache((id) =>
  thumbnail({ path: { id }, responseType: "blob" }),
);
