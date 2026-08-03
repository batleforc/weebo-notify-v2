/**
 * The visual language of a notification (design §10.3), in one place.
 *
 * Both mappings below are colour-blind-safe by construction, because neither of
 * them returns a colour alone: a status is a token **and** an icon, a priority is a
 * token **and** a rank. §10.3 is explicit that colour is never the only carrier —
 * a failure that reads as "grey circle" to one reader in twelve is a failure that
 * did not get reported.
 */

import {
  Ban,
  CircleAlert,
  CircleCheck,
  CircleX,
  Info,
  type LucideIcon,
} from "lucide-vue-next";

import type { Priority, State, Status } from "@/api/generated/types.gen";

interface StatusLook {
  icon: LucideIcon;
  /** The semantic token, never a raw colour (§10.6). */
  color: string;
  label: string;
}

const STATUS: Record<Status, StatusLook> = {
  success: { icon: CircleCheck, color: "text-status-success", label: "Succès" },
  failure: { icon: CircleX, color: "text-status-failure", label: "Échec" },
  warning: {
    icon: CircleAlert,
    color: "text-status-warning",
    label: "Avertissement",
  },
  info: { icon: Info, color: "text-status-info", label: "Info" },
  cancelled: { icon: Ban, color: "text-status-cancelled", label: "Annulé" },
};

export function lookOf(status: Status): StatusLook {
  return STATUS[status] ?? STATUS.info;
}

const STATE_LABEL: Record<State, string> = {
  open: "ouverte",
  working: "en cours",
  closed: "fermée",
};

export function stateLabel(state: State): string {
  return STATE_LABEL[state] ?? state;
}

/** The border accent on the left of a row — priority, discretely (§10.3). */
const PRIORITY_ACCENT: Record<Priority, string> = {
  min: "border-l-transparent",
  low: "border-l-transparent",
  default: "border-l-transparent",
  high: "border-l-priority-high",
  urgent: "border-l-priority-urgent",
};

export function accentOf(priority: Priority): string {
  return PRIORITY_ACCENT[priority] ?? PRIORITY_ACCENT.default;
}

const PRIORITY_LABEL: Record<Priority, string> = {
  min: "minimale",
  low: "basse",
  default: "normale",
  high: "haute",
  urgent: "urgente",
};

export function priorityLabel(priority: Priority): string {
  return PRIORITY_LABEL[priority] ?? priority;
}

/** Whether a priority earns a badge of its own — only the loud end does. */
export function isLoud(priority: Priority): boolean {
  return priority === "high" || priority === "urgent";
}

/** How an author is rendered (§2.4): the profile name, or the reference. */
export function authorLabel(author: {
  kind: string;
  display_name?: string | null;
  id?: string;
  key?: string;
  channel?: string;
  subject?: string;
}): string {
  if (author.display_name) {
    return author.display_name;
  }
  switch (author.kind) {
    case "channel_key":
      // §5.2: an anonymous channel key is attributed to the key itself, and the
      // wording says so rather than pretending somebody published it.
      return `clé ${author.key ?? "?"} du canal ${author.channel ?? "?"}`;
    case "ci_job":
      return author.subject ?? "job CI";
    default:
      return author.id ?? "inconnu";
  }
}
