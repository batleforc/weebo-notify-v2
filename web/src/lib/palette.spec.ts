/**
 * The half of the palette this workspace can verify.
 *
 * There is no browser here, so what the palette *looks* like is reviewed by whoever
 * runs it. What it *offers*, in what order, and on which keystroke is decided by
 * pure functions, and those are pinned below.
 */

import { describe, expect, it } from "vitest";

import {
  channelItems,
  notificationItems,
  opensPalette,
  rank,
  score,
  staticItems,
} from "./palette";

const channels = [
  { id: "cluster-ovh-1", display_name: "Cluster OVH 1 — incidents" },
  { id: "ci-infra", display_name: null },
  { id: "ci", display_name: null },
];

describe("matching", () => {
  it("finds a channel from the letters of its name, in order", () => {
    // The "go to file" behaviour everybody already has: `cov` → `cluster-ovh-1`.
    expect(score("cluster-ovh-1", "cov")).not.toBeNull();
    expect(score("cluster-ovh-1", "ovc")).toBeNull();
  });

  it("ranks a prefix above a word start above a scattered match", () => {
    const prefix = score("cluster-ovh-1", "clu");
    const wordStart = score("cluster-ovh-1", "ovh");
    const scattered = score("cluster-ovh-1", "cov");

    expect(prefix?.rank).toBeLessThan(wordStart?.rank ?? Infinity);
    expect(wordStart?.rank).toBeLessThan(scattered?.rank ?? Infinity);
  });

  it("prefers the shorter of two equally good matches", () => {
    // Typing `ci` and getting `ci-infra-staging` before `ci` is the small thing
    // that makes a palette feel wrong.
    const ranked = rank(channelItems(channels), "ci");

    expect(ranked[0]?.id).toBe("channel:ci");
    expect(ranked[1]?.id).toBe("channel:ci-infra");
  });

  it("matches a channel by its id even when it shows a display name", () => {
    // The id is what a permalink and a CI log carry, so it has to be typeable.
    const ranked = rank(channelItems(channels), "cluster-ovh-1");

    expect(ranked[0]?.id).toBe("channel:cluster-ovh-1");
  });

  it("matches a screen through its hint rather than only its title", () => {
    // Somebody looking for quiet hours types `dnd`, not `Settings`.
    const ranked = rank(staticItems(), "dnd");

    expect(ranked.map((item) => item.id)).toContain("screen:settings");
  });

  it("offers everything when nothing is typed", () => {
    const items = staticItems();

    expect(rank(items, "")).toHaveLength(items.length);
  });

  it("offers nothing that does not match", () => {
    expect(rank(channelItems(channels), "zzz")).toHaveLength(0);
  });
});

describe("what the palette carries", () => {
  it("names the verbs the TUI uses, so one vocabulary covers both clients", () => {
    // §10.4 says "mêmes verbes que le mode `:` du TUI (§9.2)". A palette with its
    // own words would be a second interface to learn.
    const verbs = staticItems()
      .filter((item) => item.action.kind === "command")
      .map((item) => (item.action as { verb: string }).verb);

    expect(verbs).toContain("dnd:1h");
    expect(verbs).toContain("seen");
  });

  it("sends a channel to its permalink and a notification to its own", () => {
    // The same URLs a sink mail and a CI log point at (§10.4).
    expect(channelItems(channels)[0]?.action).toEqual({
      kind: "navigate",
      to: "/c/cluster-ovh-1",
    });
    expect(
      notificationItems([
        { id: "ntf_1", head: { content: { title: "etcd" } } },
      ])[0]?.action,
    ).toEqual({ kind: "navigate", to: "/n/ntf_1" });
  });

  it("falls back to an id when a notification has no title to show", () => {
    expect(notificationItems([{ id: "ntf_2" }])[0]?.label).toBe("ntf_2");
  });
});

describe("the shortcut", () => {
  it("opens on both Cmd+K and Ctrl+K", () => {
    // Accepted on every platform rather than sniffing the user agent: a person on
    // a foreign layout should not have to guess which one this build believes in.
    expect(opensPalette({ key: "k", metaKey: true, ctrlKey: false })).toBe(
      true,
    );
    expect(opensPalette({ key: "K", metaKey: false, ctrlKey: true })).toBe(
      true,
    );
  });

  it("ignores a bare k, which is a character somebody is typing", () => {
    expect(opensPalette({ key: "k", metaKey: false, ctrlKey: false })).toBe(
      false,
    );
    expect(opensPalette({ key: "j", metaKey: true, ctrlKey: false })).toBe(
      false,
    );
  });
});
