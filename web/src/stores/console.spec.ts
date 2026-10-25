/** The console's side of §5.4: a 403 is not an error, and the nav never claims a refused role. */

import { createPinia, setActivePinia } from "pinia";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { useConsole } from "./console";

const {
  inventoryMock,
  trustPoliciesMock,
  auditMock,
  reconciliationMock,
  dlqMock,
  selfJoinMock,
  requeueMock,
} = vi.hoisted(() => ({
  inventoryMock: vi.fn(),
  trustPoliciesMock: vi.fn(),
  auditMock: vi.fn(),
  reconciliationMock: vi.fn(),
  dlqMock: vi.fn(),
  selfJoinMock: vi.fn(),
  requeueMock: vi.fn(),
}));

vi.mock("@/api/generated", () => ({
  inventory: inventoryMock,
  trustPolicies: trustPoliciesMock,
  audit: auditMock,
  reconciliation: reconciliationMock,
  dlq: dlqMock,
  selfJoin: selfJoinMock,
  requeueDeadLetter: requeueMock,
}));

/** An axios-shaped refusal: `failureOf` narrows on `isAxiosError`. */
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

const DEAD_LETTER = {
  id: "dlv_01J0000000000000000000000",
  channel: "ci-infra",
  sink: "webhook:https://hooks.weebo.poc/incidents",
  last_error: "HTTP 410",
  attempts: 3,
  first_failed_at: "2026-08-01T09:00:00Z",
  dead_at: "2026-08-01T10:00:00Z",
};

beforeEach(() => {
  setActivePinia(createPinia());
  for (const mock of [
    inventoryMock,
    trustPoliciesMock,
    auditMock,
    reconciliationMock,
    dlqMock,
    selfJoinMock,
    requeueMock,
  ]) {
    mock.mockReset();
  }
});

describe("who may see the console", () => {
  it("asks the server once rather than trusting a local flag", async () => {
    inventoryMock.mockResolvedValue({ data: { items: [CARD] } });
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
    dlqMock.mockResolvedValue({ data: [DEAD_LETTER] });
    inventoryMock.mockResolvedValue({ data: { items: [CARD] } });
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
    expect(platform.policies[0]?.subject).toBe("repo:weebo/*");
    expect(platform.entries).toHaveLength(1);
    expect(platform.gitops?.pending).toEqual(["carol@weebo.local"]);
    expect(platform.stuck).toHaveLength(1);
    expect(platform.stuck[0]?.sink).toContain("hooks.weebo.poc");
  });

  it("never receives the event a dead letter failed to deliver", async () => {
    // §5.4: a dead letter carries no payload.
    inventoryMock.mockResolvedValue({ data: { items: [CARD] } });
    trustPoliciesMock.mockResolvedValue({ data: [] });
    auditMock.mockResolvedValue({ data: { items: [], next: null } });
    reconciliationMock.mockResolvedValue({ data: null });
    dlqMock.mockResolvedValue({ data: [DEAD_LETTER] });

    const platform = useConsole();
    await platform.load();

    expect(Object.keys(platform.stuck[0] ?? {})).not.toContain("event");
  });

  it("reloads after a requeue, because the audit entry is the point", async () => {
    inventoryMock.mockResolvedValue({ data: { items: [CARD] } });
    trustPoliciesMock.mockResolvedValue({ data: [] });
    auditMock.mockResolvedValue({ data: { items: [], next: null } });
    reconciliationMock.mockResolvedValue({ data: null });
    dlqMock.mockResolvedValue({ data: [] });
    requeueMock.mockResolvedValue({ data: undefined });

    const platform = useConsole();
    await platform.requeue(DEAD_LETTER.id);

    expect(requeueMock).toHaveBeenCalledWith(
      expect.objectContaining({ path: { id: DEAD_LETTER.id } }),
    );
    expect(auditMock).toHaveBeenCalled();
  });

  it("shows the answer when somebody else requeued it first", async () => {
    requeueMock.mockRejectedValue(refusal(409, "dlq.gone"));

    const platform = useConsole();
    await platform.requeue(DEAD_LETTER.id);

    expect(platform.error).not.toBeNull();
  });

  it("says 'no pass' rather than zeroes when none has finished", async () => {
    // Zeroes would render as a healthy instance that has applied its
    // configuration, which is the opposite of what `null` means here.
    inventoryMock.mockResolvedValue({ data: { items: [CARD] } });
    trustPoliciesMock.mockResolvedValue({ data: [] });
    auditMock.mockResolvedValue({ data: { items: [], next: null } });
    reconciliationMock.mockResolvedValue({ data: null });
    dlqMock.mockResolvedValue({ data: [] });

    const platform = useConsole();
    await platform.load();

    expect(platform.gitops).toBeNull();
  });

  it("reloads after a self-join, because the audit entry is the point", async () => {
    inventoryMock.mockResolvedValue({ data: { items: [CARD] } });
    trustPoliciesMock.mockResolvedValue({ data: [] });
    auditMock.mockResolvedValue({ data: { items: [], next: null } });
    reconciliationMock.mockResolvedValue({ data: null });
    dlqMock.mockResolvedValue({ data: [] });
    selfJoinMock.mockResolvedValue({ data: undefined });

    const platform = useConsole();
    await platform.join("ci-infra");

    expect(selfJoinMock).toHaveBeenCalledWith(
      expect.objectContaining({ path: { channel: "ci-infra" } }),
    );
    expect(auditMock).toHaveBeenCalled();
  });
});
