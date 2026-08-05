/**
 * The local half of "stop notifying me" (§7.4).
 *
 * `muted` is a second implementation of `Dnd::is_muted`, written because deciding
 * whether to draw a toast cannot be a round trip. A second implementation is only
 * safe while somebody checks it against the first, so these are the scenarios the
 * Rust tests use — the on-call exception, the window that crosses midnight, the
 * timezone that belongs to the preference rather than to the device.
 */

import { createPinia, setActivePinia } from "pinia";
import { beforeEach, describe, expect, it } from "vitest";

import type { Dnd } from "@/api/generated";
import { active, covers, muted, usePreferences } from "@/stores/preferences";

const USER = "usr_01J";

function dnd(overrides: Partial<Dnd> = {}): Dnd {
  return { user: USER, ...overrides };
}

/** A UTC instant, so every expectation below is about the *zone*, not the clock. */
function at(iso: string): Date {
  return new Date(iso);
}

describe("a recurring window", () => {
  const night = {
    start: "22:00",
    end: "08:00",
    timezone: "Europe/Paris",
    days: [],
  };

  it("covers the small hours of the next morning", () => {
    // 02:00 in Paris, which is 01:00 UTC in winter.
    expect(covers(night, at("2026-01-15T01:00:00Z"))).toBe(true);
  });

  it("does not cover the afternoon", () => {
    expect(covers(night, at("2026-01-15T13:00:00Z"))).toBe(false);
  });

  it("reads the clock in its own timezone and not the browser's", () => {
    // 23:30 in Tokyo is 14:30 UTC — quiet for a Tokyo preference, and the same
    // instant is the middle of the afternoon in Paris.
    const tokyo = { ...night, timezone: "Asia/Tokyo" };

    expect(covers(tokyo, at("2026-01-15T14:30:00Z"))).toBe(true);
    expect(covers(night, at("2026-01-15T14:30:00Z"))).toBe(false);
  });

  it("belongs to the day it starts on when it crosses midnight", () => {
    // A Friday-only 22:00–08:00 is the night from Friday to Saturday: quiet at
    // 02:00 on Saturday, not quiet at 02:00 on Friday.
    const friday = { ...night, days: ["fri"] };

    // 2026-01-17 is a Saturday; 01:00 UTC is 02:00 in Paris.
    expect(covers(friday, at("2026-01-17T01:00:00Z"))).toBe(true);
    // 2026-01-16 is the Friday itself, at 02:00 local — that is Thursday's night.
    expect(covers(friday, at("2026-01-16T01:00:00Z"))).toBe(false);
    // And Friday 23:00 local is covered, being the head of the window.
    expect(covers(friday, at("2026-01-16T22:00:00Z"))).toBe(true);
  });

  it("treats a window inside one day as the plain interval it is", () => {
    const lunch = {
      start: "12:00",
      end: "14:00",
      timezone: "Europe/Paris",
      days: [],
    };

    expect(covers(lunch, at("2026-01-15T12:00:00Z"))).toBe(true); // 13:00 Paris
    expect(covers(lunch, at("2026-01-15T13:30:00Z"))).toBe(false); // 14:30 Paris
  });

  it("is not quiet when the timezone is one this browser cannot resolve", () => {
    // Refused at the API door, so reaching here means a preference older than
    // this browser's IANA data. "Not quiet" loses a toast at worst; throwing
    // would take the panel down.
    expect(
      covers({ ...night, timezone: "Europe/Pris" }, at("2026-01-15T01:00:00Z")),
    ).toBe(false);
  });
});

describe("whether a do-not-disturb is on", () => {
  it("is on while the quick toggle has not expired", () => {
    const until = new Date(Date.now() + 60_000).toISOString();

    expect(active(dnd({ until }), new Date())).toBe(true);
  });

  it("is off once it has", () => {
    const until = new Date(Date.now() - 60_000).toISOString();

    expect(active(dnd({ until }), new Date())).toBe(false);
  });

  it("is off with nothing set at all", () => {
    expect(active(dnd(), new Date())).toBe(false);
    expect(active(null, new Date())).toBe(false);
  });
});

describe("whether an arrival should make a noise", () => {
  const quiet = dnd({
    quiet_hours: [
      { start: "22:00", end: "08:00", timezone: "Europe/Paris", days: [] },
    ],
  });
  const night = at("2026-01-15T01:00:00Z");
  const afternoon = at("2026-01-15T13:00:00Z");

  it("is muted inside a quiet window", () => {
    expect(muted(quiet, "ci-infra", "default", night)).toBe(true);
  });

  it("is not muted outside one", () => {
    expect(muted(quiet, "ci-infra", "default", afternoon)).toBe(false);
  });

  it("lets an excepted channel through — the on-call case", () => {
    const oncall = { ...quiet, exceptions: ["cluster-ovh-1"] };

    expect(muted(oncall, "cluster-ovh-1", "default", night)).toBe(false);
    expect(muted(oncall, "ci-infra", "default", night)).toBe(true);
  });

  it("reaches only the channels in its scope", () => {
    const narrow = {
      ...quiet,
      scope: { kind: "channels" as const, channels: ["ci-infra"] },
    };

    expect(muted(narrow, "ci-infra", "default", night)).toBe(true);
    expect(muted(narrow, "cluster-ovh-1", "default", night)).toBe(false);
  });

  it("keeps `urgent` quiet unless the recipient opted in", () => {
    // The publisher chooses how insistent a notification is; the person receiving
    // it chooses what may wake them (§2.3, §7.4).
    expect(muted(quiet, "ci-infra", "urgent", night)).toBe(true);
    expect(
      muted({ ...quiet, urgent_bypasses: true }, "ci-infra", "urgent", night),
    ).toBe(false);
    // And the opt-in is about `urgent` alone.
    expect(
      muted({ ...quiet, urgent_bypasses: true }, "ci-infra", "high", night),
    ).toBe(true);
  });
});

describe("the store", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
  });

  it("folds a preference event whole rather than merging it", () => {
    const preferences = usePreferences();

    // Spelled in full rather than cast: the point of ADR-0013 is that the wire
    // shape is a type, and a fixture that opts out of it proves nothing about the
    // event the server actually sends.
    preferences.ingest({
      event: "preference",
      audience: "user",
      user_id: USER,
      cursor: "1",
      logged_at: "2026-01-15T01:00:00Z",
      user: USER,
      dnd: dnd({ urgent_bypasses: true }),
      silences: [],
    });

    expect(preferences.dnd?.urgent_bypasses).toBe(true);
  });

  it("stays quiet for a notification somebody silenced", () => {
    const preferences = usePreferences();
    preferences.silences = [
      {
        user: USER,
        since: "2026-01-15T00:00:00Z",
        target: { scope: "notification", id: "ntf_01J" },
      },
    ];

    expect(
      preferences.stayQuiet({
        channel: "ci-infra",
        priority: "default",
        notification: "ntf_01J",
      }),
    ).toBe(true);
    expect(
      preferences.stayQuiet({
        channel: "ci-infra",
        priority: "default",
        notification: "ntf_01K",
      }),
    ).toBe(false);
  });

  it("stays quiet for a whole group when the silence is group-scoped", () => {
    const preferences = usePreferences();
    preferences.silences = [
      {
        user: USER,
        since: "2026-01-15T00:00:00Z",
        target: { scope: "group", key: "deploy:prod" },
      },
    ];

    expect(
      preferences.stayQuiet({
        channel: "ci-infra",
        priority: "default",
        notification: "ntf_01K",
        groupKey: "deploy:prod",
      }),
    ).toBe(true);
    // An arrival with no group key matches no group silence — the comparison is
    // against a value, never against `undefined`.
    expect(
      preferences.stayQuiet({
        channel: "ci-infra",
        priority: "default",
        notification: "ntf_01K",
        groupKey: null,
      }),
    ).toBe(false);
  });
});
