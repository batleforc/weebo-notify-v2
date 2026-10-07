/**
 * Command palette items and ranking (§10.4), pure so the ranking is testable.
 * Verbs match the TUI's `:` mode (§9.2). Matching is subsequence-based.
 */

import { i18n } from "@/i18n";

export type PaletteAction =
  | { kind: "navigate"; to: string }
  | { kind: "command"; verb: string };

export interface PaletteItem {
  id: string;
  label: string;
  /** The group it appears under (§10.4: Canaux / Notifications / Actions). */
  group: "channels" | "screens" | "notifications" | "actions";
  action: PaletteAction;
  /** Extra text that should match but is not the label — a channel id, a tag. */
  hint?: string;
}

/** How well a candidate matched, or `null` when it did not. */
interface Score {
  /** Lower is better. */
  rank: number;
}

/** Exact prefix < word-start < subsequence; shorter wins within a tier. */
export function score(text: string, query: string): Score | null {
  const haystack = text.toLowerCase();
  const needle = query.toLowerCase();

  if (needle.length === 0) {
    return { rank: 1000 + haystack.length };
  }
  if (haystack.startsWith(needle)) {
    return { rank: haystack.length };
  }

  const atWordStart = haystack
    .split(/[\s\-_./]+/)
    .some((word) => word.startsWith(needle));
  if (atWordStart) {
    return { rank: 100 + haystack.length };
  }

  let index = 0;
  for (const character of needle) {
    index = haystack.indexOf(character, index);
    if (index === -1) {
      return null;
    }
    index += 1;
  }
  return { rank: 500 + haystack.length };
}

/** Matching items, best first. */
export function rank(items: PaletteItem[], query: string): PaletteItem[] {
  const scored: Array<{ item: PaletteItem; rank: number }> = [];

  for (const item of items) {
    const label = score(item.label, query);
    const hint = item.hint ? score(item.hint, query) : null;
    const best =
      label && hint
        ? Math.min(label.rank, hint.rank)
        : (label?.rank ?? hint?.rank ?? null);
    if (best !== null) {
      scored.push({ item, rank: best });
    }
  }

  return scored
    .sort((left, right) => left.rank - right.rank)
    .map((entry) => entry.item);
}

/** Screens and TUI verbs (§9.2), offered whatever is typed. */
export function staticItems(): PaletteItem[] {
  const t = i18n.global.t;
  return [
    {
      id: "screen:inbox",
      label: t("sidebar.nav.inbox"),
      group: "screens",
      action: { kind: "navigate", to: "/" },
    },
    {
      id: "screen:settings",
      label: t("sidebar.nav.settings"),
      group: "screens",
      hint: t("palette.hints.settings"),
      action: { kind: "navigate", to: "/settings" },
    },
    {
      id: "screen:admin",
      label: t("sidebar.nav.admin"),
      group: "screens",
      hint: t("palette.hints.admin"),
      action: { kind: "navigate", to: "/admin" },
    },
    {
      id: "screen:console",
      label: t("console.title"),
      group: "screens",
      hint: t("palette.hints.console"),
      action: { kind: "navigate", to: "/console" },
    },
    {
      id: "action:dnd-1h",
      label: t("palette.actions.dnd1h"),
      group: "actions",
      hint: "dnd silence",
      action: { kind: "command", verb: "dnd:1h" },
    },
    {
      id: "action:mark-seen",
      label: t("palette.actions.markSeen"),
      group: "actions",
      hint: "seen receipt",
      action: { kind: "command", verb: "seen" },
    },
  ];
}

/** Channels as palette rows, matching on both display name and id. */
export function channelItems(
  channels: Array<{ id: string; display_name?: string | null }>,
): PaletteItem[] {
  return channels.map((channel) => ({
    id: `channel:${channel.id}`,
    label: channel.display_name || channel.id,
    group: "channels",
    hint: channel.id,
    action: { kind: "navigate", to: `/c/${channel.id}` },
  }));
}

/** Search results as palette rows (§10.4 — `Enter` opens the detail panel). */
export function notificationItems(
  notifications: Array<{ id: string; head?: { content?: { title?: string } } }>,
): PaletteItem[] {
  return notifications.map((notification) => ({
    id: `notification:${notification.id}`,
    label: notification.head?.content?.title || notification.id,
    group: "notifications",
    action: { kind: "navigate", to: `/n/${notification.id}` },
  }));
}

/** Whether a keystroke should open the palette (§10.4: `Cmd+K`, `Ctrl+K` elsewhere). */
export function opensPalette(event: {
  key: string;
  metaKey: boolean;
  ctrlKey: boolean;
}): boolean {
  // Both modifiers on every platform rather than sniffing the UA.
  return event.key.toLowerCase() === "k" && (event.metaKey || event.ctrlKey);
}
