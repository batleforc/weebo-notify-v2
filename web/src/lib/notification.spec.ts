import { beforeEach, describe, expect, it } from "vitest";

import { i18n } from "@/i18n";
import {
  authorLabel,
  isLoud,
  lookOf,
  stateLabel,
  unreadLabel,
} from "./notification";

beforeEach(() => {
  i18n.global.locale.value = "fr";
});

describe("unreadLabel", () => {
  it("agrees with the count, which the digit alone never had to", () => {
    // Plural agreement for the screen-reader label.
    expect(unreadLabel(1)).toBe("1 non lue");
    expect(unreadLabel(2)).toBe("2 non lues");
    expect(unreadLabel(11)).toBe("11 non lues");
  });
});

describe("lookOf", () => {
  it("gives every status an icon, not only a colour (§10.3)", () => {
    for (const status of [
      "success",
      "failure",
      "warning",
      "info",
      "cancelled",
    ] as const) {
      const look = lookOf(status);
      expect(look.icon).toBeTruthy();
      expect(look.label).toBeTruthy();
      // Semantic tokens only — a raw colour here is what breaks the monofolio
      // swap (§10.6), and it is the one blocking rule of a front-end review.
      expect(look.color).toMatch(/^text-status-/);
    }
  });
});

describe("isLoud", () => {
  it("only singles out the loud end of the scale", () => {
    expect(isLoud("high")).toBe(true);
    expect(isLoud("default")).toBe(false);
    expect(isLoud("urgent")).toBe(true);
  });
});

describe("authorLabel", () => {
  it("prefers the profile name (§2.4)", () => {
    expect(
      authorLabel({ kind: "user", id: "usr_1", display_name: "Alice Martin" }),
    ).toBe("Alice Martin");
  });

  it("says a channel key published, rather than pretending somebody did (§5.2)", () => {
    expect(
      authorLabel({
        kind: "channel_key",
        key: "wnk_ci4x",
        channel: "ci-infra",
      }),
    ).toBe("clé wnk_ci4x du canal ci-infra");
  });

  it("names a CI job by its subject claim", () => {
    expect(
      authorLabel({ kind: "ci_job", subject: "repo:weebo/infra:ref:main" }),
    ).toBe("repo:weebo/infra:ref:main");
  });

  it("falls back to the reference when there is no profile", () => {
    expect(authorLabel({ kind: "service_account", id: "bot-argocd" })).toBe(
      "bot-argocd",
    );
  });
});

describe("in English", () => {
  beforeEach(() => {
    i18n.global.locale.value = "en";
  });

  it("unreadLabel uses the English singular/plural split", () => {
    expect(unreadLabel(1)).toBe("1 unread");
    expect(unreadLabel(2)).toBe("2 unread");
  });

  it("lookOf and stateLabel answer in English", () => {
    expect(lookOf("failure").label).toBe("Failure");
    expect(stateLabel("open")).toBe("open");
  });

  it("says a channel key published, in English", () => {
    expect(
      authorLabel({
        kind: "channel_key",
        key: "wnk_ci4x",
        channel: "ci-infra",
      }),
    ).toBe("key wnk_ci4x on channel ci-infra");
  });
});
