/**
 * The visual language of a notification (design §10.3), in one place.
 *
 * Both mappings below are colour-blind-safe by construction, because neither of
 * them returns a colour alone: a status is a token **and** an icon, a priority is a
 * token **and** a rank. §10.3 is explicit that colour is never the only carrier —
 * a failure that reads as "grey circle" to one reader in twelve is a failure that
 * did not get reported.
 *
 * Labels are looked up through the global i18n composer rather than `useI18n()`:
 * this module has no component instance, and is called from many that do.
 */

import {
  Ban,
  CircleAlert,
  CircleCheck,
  CircleX,
  Info,
  type LucideIcon,
} from "@lucide/vue";

import type { Priority, State, Status } from "@/api/generated/types.gen";
import { i18n } from "@/i18n";

interface StatusLook {
  icon: LucideIcon;
  /** The semantic token, never a raw colour (§10.6). */
  color: string;
  label: string;
}

const STATUS_ICON: Record<Status, { icon: LucideIcon; color: string }> = {
  success: { icon: CircleCheck, color: "text-status-success" },
  failure: { icon: CircleX, color: "text-status-failure" },
  warning: { icon: CircleAlert, color: "text-status-warning" },
  info: { icon: Info, color: "text-status-info" },
  cancelled: { icon: Ban, color: "text-status-cancelled" },
};

export function lookOf(status: Status): StatusLook {
  const known = STATUS_ICON[status] ?? STATUS_ICON.info;
  return { ...known, label: i18n.global.t(`status.${status}`) };
}

export function stateLabel(state: State): string {
  return i18n.global.t(`state.${state}`);
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

export function priorityLabel(priority: Priority): string {
  return i18n.global.t(`priority.${priority}`);
}

/**
 * What a screen reader says for the sidebar's unread badge (§7.3, §10.4).
 *
 * Here rather than inline in the template because the wording agrees with the
 * count: the badge said "1 non lues" until somebody opened the page, which is
 * the one reader who would have been told so is the one who cannot see the digit.
 */
export function unreadLabel(count: number): string {
  return i18n.global.t("sidebar.unreadLabel", { count }, count);
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
      return i18n.global.t("notification.author.channelKey", {
        key: author.key ?? "?",
        channel: author.channel ?? "?",
      });
    case "ci_job":
      return author.subject ?? i18n.global.t("notification.author.ciJob");
    default:
      return author.id ?? i18n.global.t("notification.author.unknown");
  }
}
