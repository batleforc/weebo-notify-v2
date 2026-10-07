/** The visual language of a notification (§10.3): never colour alone. Uses global i18n (no component). */

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

export function priorityLabel(priority: Priority): string {
  return i18n.global.t(`priority.${priority}`);
}

/** Screen-reader label for the unread badge, with plural agreement (§7.3). */
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
