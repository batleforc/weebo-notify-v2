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

import type { Problem } from "./generated/types.gen";

/** A refusal, in the shape the UI wants. */
export interface Failure {
  /** The stable code, when the server sent one (§8). */
  code?: string;
  /** HTTP status, `0` when the request never reached anybody. */
  status: number;
  /** What to show. Already in French, already a sentence. */
  message: string;
  /** Seconds to wait, on a `429`. */
  retryAfter?: number;
}

/** French sentences for the codes the web client can actually provoke. */
const MESSAGES: Record<string, string> = {
  "auth.missing": "Session expirée — reconnecte-toi.",
  "auth.invalid": "Session expirée — reconnecte-toi.",
  "auth.expired": "Session expirée — reconnecte-toi.",
  "scope.missing": "Tu n'as pas les droits pour cette action sur ce canal.",
  "channel.unknown": "Ce canal n'existe pas, ou tu n'y as pas accès.",
  "notification.unknown": "Cette notification n'existe plus.",
  "notification.deleted": "Cette notification a été supprimée.",
  "field.missing": "Il manque une information dans le formulaire.",
  "field.empty": "Un champ obligatoire est vide.",
  "field.invalid": "Une valeur du formulaire n'est pas valide.",
  "rate.limited": "Trop de requêtes — réessaie dans un instant.",
  "action.unknown": "Cette action n'existe plus sur cette révision.",
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
      return {
        status,
        message:
          "Le serveur est injoignable. Vérifie ta connexion, ou qu'il tourne bien.",
      };
    }

    const code = problem?.code ?? undefined;
    return {
      code,
      status,
      retryAfter: problem?.retry_after_seconds ?? undefined,
      message:
        (code ? MESSAGES[code] : undefined) ??
        problem?.title ??
        `Le serveur a répondu ${status}.`,
    };
  }

  if (error instanceof Error) {
    return { status: 0, message: error.message };
  }

  return { status: 0, message: "Une erreur inattendue s'est produite." };
}
