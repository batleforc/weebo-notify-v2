/**
 * What the command palette offers, and in what order (design §10.4).
 *
 * §10.4 asks for "une seule surface pour chercher et switcher partout dans l'app",
 * and names three kinds of thing on it: channels and screens to switch to,
 * notifications to find, and contextual actions with **the same verbs as the TUI's
 * `:` mode** (§9.2) — one vocabulary across clients.
 *
 * # Why the matching lives here and not in the component
 *
 * Everything below is a pure function over data. That is deliberate: the ranking is
 * the part that decides whether the palette feels right, and it is the part that
 * breaks silently. It stays here now that the workspace has a browser (§12.4,
 * `task browser:`) for a reason that has not changed: a screenshot answers what a
 * row looks like, never why one row sorts above another.
 *
 * # The matching rule
 *
 * Subsequence matching, the way every editor's "go to file" works: `cov` finds
 * `cluster-ovh-1`. Anything cleverer (Levenshtein, token weighting) buys accuracy
 * on long lists this palette does not have — a person has tens of channels, not
 * thousands — and costs a behaviour nobody can predict from the keystrokes.
 */

/** What a row on the palette does when it is chosen. */
export type PaletteAction =
  | { kind: "navigate"; to: string }
  | { kind: "command"; verb: string };

/** One offered row. */
export interface PaletteItem {
  /** Stable identity, for keys and tests. */
  id: string;
  /** What the row reads as. */
  label: string;
  /** The group it appears under (§10.4: Canaux / Notifications / Actions). */
  group: "channels" | "screens" | "notifications" | "actions";
  /** What choosing it does. */
  action: PaletteAction;
  /** Extra text that should match but is not the label — a channel id, a tag. */
  hint?: string;
}

/** How well a candidate matched, or `null` when it did not. */
interface Score {
  /** Lower is better. */
  rank: number;
}

/**
 * Score `text` against a typed `query`.
 *
 * Three tiers, in the order a person expects: an exact prefix beats a word-start
 * match, which beats a scattered subsequence. Within a tier, shorter wins — the
 * channel called `ci` should come before `ci-infra-staging` when both match `ci`.
 */
export function score(text: string, query: string): Score | null {
  const haystack = text.toLowerCase();
  const needle = query.toLowerCase();

  if (needle.length === 0) {
    return { rank: 1000 + haystack.length };
  }
  if (haystack.startsWith(needle)) {
    return { rank: haystack.length };
  }

  // A match at the start of any word: `ovh` finds `cluster-ovh-1`, which a plain
  // subsequence would also find but rank below noise.
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

/**
 * The items that match, best first.
 *
 * Groups keep their relative order from the input rather than being sorted against
 * each other: §10.4 wants results *grouped*, and a channel outranking a screen
 * because its name is shorter would make the palette reshuffle as you type.
 */
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

/**
 * The rows that exist whatever is typed: the screens, and the verbs §10.4 lists.
 *
 * The verbs are the TUI's (§9.2) on purpose — `ack`, `silence`, `dnd` — so that
 * somebody who learned one client can guess the other. What each does is the
 * caller's business; the palette only names them.
 */
export function staticItems(): PaletteItem[] {
  return [
    {
      id: "screen:inbox",
      label: "Inbox",
      group: "screens",
      action: { kind: "navigate", to: "/" },
    },
    {
      id: "screen:settings",
      label: "Settings",
      group: "screens",
      hint: "profil thème densité dnd",
      action: { kind: "navigate", to: "/settings" },
    },
    {
      id: "screen:admin",
      label: "Admin",
      group: "screens",
      hint: "canaux devices clés service accounts",
      action: { kind: "navigate", to: "/admin" },
    },
    {
      id: "screen:console",
      label: "Console plateforme",
      group: "screens",
      hint: "dlq audit réconciliation inventaire",
      action: { kind: "navigate", to: "/console" },
    },
    {
      id: "action:dnd-1h",
      label: "Ne pas déranger pendant 1 h",
      group: "actions",
      hint: "dnd silence",
      action: { kind: "command", verb: "dnd:1h" },
    },
    {
      id: "action:mark-seen",
      label: "Marquer la sélection comme vue",
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
  // Both modifiers accepted on every platform rather than sniffing the user agent:
  // `Ctrl+K` on a Mac reaches nothing else here, and a person on a foreign keyboard
  // layout should not have to discover which of the two this build believes in.
  return event.key.toLowerCase() === "k" && (event.metaKey || event.ctrlKey);
}
