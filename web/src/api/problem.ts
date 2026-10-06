/**
 * Refusals as readable messages (RFC 9457, §8). Branch on `code`, never `status`
 * or `title`; unknown codes fall back to the server's `title`.
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

/** Localised `problem.*` keys by `code` (dots → camelCase). The server doesn't localise titles. */
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

/** Total: never throws, so error paths can render it blindly. */
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
