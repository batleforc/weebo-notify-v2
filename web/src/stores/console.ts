/**
 * The platform console (§5.4, §10.4bis): metadata and operations, never content.
 * Visibility comes from one probe; the server decides the role (§14.2).
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
  /** `null` until probed, so the nav waits rather than flickers. */
  const allowed = ref<boolean | null>(null);
  const channels = ref<ChannelCard[]>([]);
  const policies = ref<TrustPolicyCard[]>([]);
  const entries = ref<AuditEntry[]>([]);
  /** Last GitOps pass on the answering replica; `null` if none finished yet (§11). */
  const gitops = ref<Reconciliation | null>(null);
  /** Given-up sink deliveries (§7.1bis). */
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
      allowed.value = false;
      // 403 is the expected answer for non-admins, not an error.
      if (failure.status !== 403) {
        error.value = failure.message;
      }
    }
  }

  async function load(): Promise<void> {
    loading.value = true;
    error.value = null;
    try {
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

  /** Join as this admin (§5.4); reload to show the audit entry it produces. */
  async function join(channel: string): Promise<void> {
    try {
      await postSelfJoin({ path: { channel }, throwOnError: true });
      await load();
    } catch (caught) {
      error.value = failureOf(caught).message;
    }
  }

  /** Requeue a dead letter (§7.1bis, T17); reload for the audit entry. A 409 means someone else did. */
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
