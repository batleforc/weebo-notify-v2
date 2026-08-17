/**
 * The platform console (§5.4, §10.4bis) — a console, not a dashboard.
 *
 * # How the app learns it may show this at all
 *
 * With one probe, and deliberately not with a flag on the session. The server
 * decides the role from `[admin] oidc_groups` intersected with the token's claims
 * (§14.2), so it is the only thing that can answer, and asking it once at open is
 * both the cheapest way and the honest one: what the nav shows is then exactly what
 * the API permits, rather than a copy of it that can drift.
 *
 * The `403` a non-admin gets is expected and is not an error worth showing. It is
 * the answer, and it arrives once per session.
 *
 * # What is not here
 *
 * Notifications. §5.4 gives this role metadata and operations and never content,
 * and the endpoints behind this store cannot return a notification at all — see
 * `notify-app/src/platform.rs`. Reading a channel means joining it, which is a
 * button here and a line in the audit log.
 */

import { defineStore } from "pinia";
import { computed, ref } from "vue";

import {
  audit as fetchAudit,
  dlq as fetchDlq,
  inventory as fetchInventory,
  reconciliation as fetchReconciliation,
  trustPolicies as fetchTrustPolicies,
  requeueDeadLetter as postRequeue,
  selfJoin as postSelfJoin,
} from "@/api/generated";
import type {
  AuditEntry,
  ChannelCard,
  DeadLetter,
  Reconciliation,
  TrustPolicyCard,
} from "@/api/generated/types.gen";
import { failureOf } from "@/api/problem";

export const useConsole = defineStore("console", () => {
  /**
   * `null` until the first probe: "we have not asked" and "you may not" are
   * different, and a nav that flickers between them is worse than one that waits.
   */
  const allowed = ref<boolean | null>(null);
  const channels = ref<ChannelCard[]>([]);
  const policies = ref<TrustPolicyCard[]>([]);
  const entries = ref<AuditEntry[]>([]);
  /**
   * The last GitOps pass — `null` when none has finished on the replica that
   * answered (§11 gates only the purge on leadership, so every replica
   * reconciles and two reads can differ by a tick).
   */
  const gitops = ref<Reconciliation | null>(null);
  /**
   * Sink deliveries that were given up on (§7.1bis). Empty is a real answer here
   * and worth showing as one: it means nothing is stuck, on an instance that
   * actually has sinks to get stuck.
   */
  const stuck = ref<DeadLetter[]>([]);
  const loading = ref(false);
  const error = ref<string | null>(null);

  const managed = computed(
    () => channels.value.filter((card) => card.origin_managed).length,
  );

  /** Ask once whether this session may see the console at all. */
  async function probe(): Promise<void> {
    if (allowed.value !== null) {
      return;
    }
    try {
      const answer = await fetchInventory({ throwOnError: true });
      channels.value = answer.data;
      allowed.value = true;
    } catch (caught) {
      const failure = failureOf(caught);
      if (failure.status === 403) {
        // The expected answer for almost everybody. Not an error, and not shown.
        allowed.value = false;
        return;
      }
      allowed.value = false;
      error.value = failure.message;
    }
  }

  /** Load everything the console shows. */
  async function load(): Promise<void> {
    loading.value = true;
    error.value = null;
    try {
      // One round trip per concern, in parallel: five endpoints, none waiting on
      // another.
      const [
        inventoryAnswer,
        policyAnswer,
        auditAnswer,
        gitopsAnswer,
        dlqAnswer,
      ] = await Promise.all([
        fetchInventory({ throwOnError: true }),
        fetchTrustPolicies({ throwOnError: true }),
        fetchAudit({ query: { limit: 50 }, throwOnError: true }),
        fetchReconciliation({ throwOnError: true }),
        fetchDlq({ throwOnError: true }),
      ]);
      channels.value = inventoryAnswer.data;
      policies.value = policyAnswer.data;
      entries.value = auditAnswer.data.items;
      gitops.value = gitopsAnswer.data ?? null;
      stuck.value = dlqAnswer.data;
      allowed.value = true;
    } catch (caught) {
      const failure = failureOf(caught);
      allowed.value = failure.status === 403 ? false : allowed.value;
      error.value = failure.status === 403 ? null : failure.message;
    } finally {
      loading.value = false;
    }
  }

  /**
   * Join a channel as this admin (§5.4).
   *
   * Reloads afterwards rather than patching the card locally, because the point of
   * the act is the **audit entry** it produces, and a console that showed the join
   * without showing the record would be hiding the half that matters.
   */
  async function join(channel: string): Promise<void> {
    try {
      await postSelfJoin({ path: { channel }, throwOnError: true });
      await load();
    } catch (caught) {
      error.value = failureOf(caught).message;
    }
  }

  /**
   * Put one dead letter back in the queue (§7.1bis, T17).
   *
   * Reloads for the same reason `join` does: the act produces an audit entry, and
   * a console that showed the requeue without showing the record would hide the
   * half that a later reader needs. A `409` means somebody else got there first —
   * shown as the answer it is, not as a failure.
   */
  async function requeue(id: string): Promise<void> {
    try {
      await postRequeue({ path: { id }, throwOnError: true });
      await load();
    } catch (caught) {
      error.value = failureOf(caught).message;
    }
  }

  return {
    allowed,
    channels,
    entries,
    error,
    gitops,
    join,
    load,
    loading,
    managed,
    policies,
    probe,
    requeue,
    stuck,
  };
});
