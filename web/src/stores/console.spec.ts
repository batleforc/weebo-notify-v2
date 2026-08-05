/**
 * The console's half of §5.4.
 *
 * What is worth locking down is not the tables — the server builds those — but the
 * two ways the client could quietly betray the boundary: treating the `403` that
 * almost every user gets as an error worth showing, and letting the nav claim a
 * role the API would refuse.
 */

import { createPinia, setActivePinia } from "pinia";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { useConsole } from "./console";

const {
  inventoryMock,
  trustPoliciesMock,
  auditMock,
  reconciliationMock,
  selfJoinMock,
} = vi.hoisted(() => ({
  inventoryMock: vi.fn(),
  trustPoliciesMock: vi.fn(),
  auditMock: vi.fn(),
  reconciliationMock: vi.fn(),
  selfJoinMock: vi.fn(),
}));

vi.mock("@/api/generated", () => ({
  inventory: inventoryMock,
  trustPolicies: trustPoliciesMock,
  audit: auditMock,
  reconciliation: reconciliationMock,
  selfJoin: selfJoinMock,
}));

/**
 * What the axios client throws for a `problem+json` refusal.
 *
 * `isAxiosError` is the flag `failureOf` narrows on, so a bare object with a
 * `response` would be read as "unexpected error" — which is the failure this file
 * exists to distinguish from a `403`.
 */
function refusal(status: number, code: string) {
  return {
    isAxiosError: true,
    response: { status, data: { status, code, title: "refused" } },
  };
}

const CARD = {
  id: "ci-infra",
  origin_managed: true,
  members: 2,
  group_bindings: 1,
  receipts_enabled: false,
};

beforeEach(() => {
  setActivePinia(createPinia());
  for (const mock of [
    inventoryMock,
    trustPoliciesMock,
    auditMock,
    reconciliationMock,
    selfJoinMock,
  ]) {
    mock.mockReset();
  }
});

describe("who may see the console", () => {
  it("asks the server once rather than trusting a local flag", async () => {
    inventoryMock.mockResolvedValue({ data: [CARD] });
    const platform = useConsole();

    await platform.probe();
    await platform.probe();

    expect(inventoryMock).toHaveBeenCalledTimes(1);
    expect(platform.allowed).toBe(true);
  });

  it("treats the 403 almost everybody gets as the answer, not an error", async () => {
    inventoryMock.mockRejectedValue(refusal(403, "platform.role_required"));
    const platform = useConsole();

    await platform.probe();

    expect(platform.allowed).toBe(false);
    expect(platform.error).toBeNull();
  });

  it("does not claim the role when the server was merely unreachable", async () => {
    inventoryMock.mockRejectedValue(refusal(503, "unavailable"));
    const platform = useConsole();

    await platform.probe();

    expect(platform.allowed).toBe(false);
    expect(platform.error).not.toBeNull();
  });

  it("starts as 'not asked', which is not 'not allowed'", () => {
    // A nav that flickers from hidden to shown is worse than one that waits.
    expect(useConsole().allowed).toBeNull();
  });
});

describe("what it shows", () => {
  it("loads the four panels in one go", async () => {
    inventoryMock.mockResolvedValue({ data: [CARD] });
    trustPoliciesMock.mockResolvedValue({
      data: [
        {
          issuer: "https://forgejo.weebo.poc",
          subject: "repo:weebo/*",
          channels: ["ci-*"],
          scopes: ["publish"],
          channel_scoped: false,
        },
      ],
    });
    auditMock.mockResolvedValue({
      data: {
        items: [
          {
            actor: { kind: "user", id: "usr_1" },
            action: "apikey.revoked",
            at: "2026-08-01T10:00:00Z",
          },
        ],
        next: null,
      },
    });
    reconciliationMock.mockResolvedValue({
      data: {
        at: "2026-08-01T10:00:00Z",
        channels: 2,
        accounts: 1,
        removed: 0,
        pending: ["carol@weebo.local"],
      },
    });

    const platform = useConsole();
    await platform.load();

    expect(platform.channels).toHaveLength(1);
    expect(platform.managed).toBe(1);
    expect(platform.policies[0].subject).toBe("repo:weebo/*");
    expect(platform.entries).toHaveLength(1);
    expect(platform.gitops?.pending).toEqual(["carol@weebo.local"]);
  });

  it("says 'no pass' rather than zeroes when none has finished", async () => {
    // Zeroes would render as a healthy instance that has applied its
    // configuration, which is the opposite of what `null` means here.
    inventoryMock.mockResolvedValue({ data: [CARD] });
    trustPoliciesMock.mockResolvedValue({ data: [] });
    auditMock.mockResolvedValue({ data: { items: [], next: null } });
    reconciliationMock.mockResolvedValue({ data: null });

    const platform = useConsole();
    await platform.load();

    expect(platform.gitops).toBeNull();
  });

  it("reloads after a self-join, because the audit entry is the point", async () => {
    inventoryMock.mockResolvedValue({ data: [CARD] });
    trustPoliciesMock.mockResolvedValue({ data: [] });
    auditMock.mockResolvedValue({ data: { items: [], next: null } });
    reconciliationMock.mockResolvedValue({ data: null });
    selfJoinMock.mockResolvedValue({ data: undefined });

    const platform = useConsole();
    await platform.join("ci-infra");

    expect(selfJoinMock).toHaveBeenCalledWith(
      expect.objectContaining({ path: { channel: "ci-infra" } }),
    );
    expect(auditMock).toHaveBeenCalled();
  });
});
