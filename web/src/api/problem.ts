/**
 * Turning a refusal into something a person can read (RFC 9457, design §8).
 *
 * The contract is explicit about what to branch on: **`code`**, not `status` and
 * definitely not `title`. Statuses are coarse — a `422` covers a missing field and
 * a notification that ran out of revisions — and titles are prose that gets
 * reworded. A code is a promise.
 *
 * So the mapping below is keyed by code, and the server's `title` is the fallback
 * for anything not listed. An unknown code shows the server's own words rather
 * than "an error occurred", which is what makes a new server error usable by an
 * older frontend instead of invisible.
 */

import axios from "axios";

import { i18n } from "@/i18n";
import type { Problem } from "./generated/types.gen";

/** A refusal, in the shape the UI wants. */
export interface Failure {
  /** The stable code, when the server sent one (§8). */
  code?: string;
  /** HTTP status, `0` when the request never reached anybody. */
  status: number;
  /** What to show. Already translated, already a sentence. */
  message: string;
  /** Seconds to wait, on a `429`. */
  retryAfter?: number;
}

/**
 * `problem.*` keys the web client can actually provoke, keyed by the server's
 * stable `code` (§8) with dots turned into camelCase segments.
 *
 * Any `code` the server sends that is NOT in this map falls back to the raw,
 * unlocalized server `title` a few lines down — the backend has no
 * `Accept-Language` handling anywhere, so that fallback stays whatever
 * language the server wrote it in. Out of scope here; growing this map is the
 * only lever the frontend has.
 */
const MESSAGE_KEYS: Record<string, string> = {
  "auth.missing": "problem.authMissing",
  "auth.invalid": "problem.authInvalid",
  "auth.expired": "problem.authExpired",
  "scope.missing": "problem.scopeMissing",
  "channel.unknown": "problem.channelUnknown",
  "notification.unknown": "problem.notificationUnknown",
  "notification.deleted": "problem.notificationDeleted",
  "field.missing": "problem.fieldMissing",
  "field.empty": "problem.fieldEmpty",
  "field.invalid": "problem.fieldInvalid",
  "rate.limited": "problem.rateLimited",
  "action.unknown": "problem.actionUnknown",
};

/**
 * Normalise anything thrown by the API layer.
 *
 * Deliberately total: a caller renders `failureOf(error).message` without ever
 * checking what it caught, because an error path that can itself throw is an
 * error path nobody tests.
 */
export function failureOf(error: unknown): Failure {
  if (axios.isAxiosError(error)) {
    const status = error.response?.status ?? 0;
    const problem = error.response?.data as Problem | undefined;

    if (status === 0) {
      return { status, message: i18n.global.t("problem.networkDown") };
    }

    const code = problem?.code ?? undefined;
    const key = code ? MESSAGE_KEYS[code] : undefined;
    return {
      code,
      status,
      retryAfter: problem?.retry_after_seconds ?? undefined,
      message:
        (key ? i18n.global.t(key) : undefined) ??
        problem?.title ??
        i18n.global.t("problem.serverResponded", { status }),
    };
  }

  if (error instanceof Error) {
    return { status: 0, message: error.message };
  }

  return { status: 0, message: i18n.global.t("problem.unexpected") };
}
