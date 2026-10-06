<script setup lang="ts">
/**
 * The platform console (§10.4bis): states and counters that can be acted on.
 *
 * Deliberately not a dashboard. §10.1 refuses the generic kind, and §13 keeps time
 * series in Grafana — duplicating them here would be a screen nobody trusts because
 * it disagrees with the one that has the data.
 *
 * Four panels, each answering a question somebody actually arrives with:
 *   * **Canaux** — is this instance configured the way we think it is;
 *   * **Trust policies** — why was that pipeline refused;
 *   * **DLQ** — what did we fail to deliver, and can it be sent again;
 *   * **Audit** — what happened, and who did it.
 *
 * The DLQ panel shows the delivery and never the event: an admin who is not a
 * member of the channel would otherwise read its notifications through a failed
 * webhook, which is the door §5.4 closes.
 */
import { onMounted } from "vue";
import { useI18n } from "vue-i18n";
import type { Trigger } from "@/api/generated/types.gen";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { exact } from "@/lib/format";
import { useConsole } from "@/stores/console";

const platform = useConsole();
const { t } = useI18n();

/** What made the last pass happen (§11, §10.4bis). */
function triggerLabel(trigger: Trigger): string {
  return t(`console.reconciliation.trigger.${trigger}`);
}

onMounted(() => {
  void platform.load();
});
</script>

<template>
  <div class="h-full overflow-y-auto">
  <div class="mx-auto max-w-4xl space-y-8 px-4 pb-16 pt-6 sm:px-8 sm:pt-10">
    <header class="space-y-3">
      <h1 class="display uppercase">{{ t("console.title") }}</h1>
      <p class="max-w-[64ch] text-sm text-ink-dim">
        {{ t("console.description") }}
      </p>
    </header>

    <div v-if="platform.loading" class="space-y-3">
      <Skeleton class="h-5 w-1/3" />
      <Skeleton class="h-24 w-full" />
    </div>

    <!-- The expected answer for almost everybody, and not an error. -->
    <div
      v-else-if="platform.allowed === false"
      class="space-y-2 border border-rule-strong px-6 py-10 text-sm"
    >
      <p class="pixel text-base font-bold uppercase">{{ t("console.forbidden.title") }}</p>
      <p class="text-ink-dim">
        {{ t("console.forbidden.description") }}
      </p>
    </div>

    <template v-else>
      <p v-if="platform.error" class="text-sm text-signal">
        {{ platform.error }}
      </p>

      <!-- GitOps: what the last pass did, on the replica that answered. -->
      <section class="space-y-3 border-t border-rule pt-5">
        <h2 class="pixel text-base font-bold uppercase">{{ t("console.reconciliation.title") }}</h2>
        <p v-if="!platform.gitops" class="text-xs text-ink-dim">
          {{ t("console.reconciliation.none") }}
        </p>
        <div v-else class="space-y-1 border border-rule p-3 text-xs">
          <!--
            The trigger, not only the timestamp: §10.4bis asks this screen for « le
            dernier SIGHUP », and the reconciler also runs every minute — so a bare
            time is almost always the timer, and an operator who has just sent a
            signal cannot tell whether it landed.
          -->
          <p>
            {{
              t("console.reconciliation.lastPass", {
                at: exact(platform.gitops.at),
                trigger: triggerLabel(platform.gitops.trigger),
                channels: platform.gitops.channels,
                accounts: platform.gitops.accounts,
                removed: platform.gitops.removed,
              })
            }}
          </p>
          <!--
            The one actionable line: a binding by `email` or `oidc_sub` grants
            nothing until that person logs in once (§14.1, M1.8). Without a place
            to see it, the symptom is "alice cannot see the channel" with nothing
            to connect it to.
          -->
          <p
            v-if="platform.gitops.pending.length"
            class="text-copper"
          >
            {{
              t("console.reconciliation.pending", {
                count: platform.gitops.pending.length,
              })
            }}
            <span class="font-mono">{{
              platform.gitops.pending.join(", ")
            }}</span>
          </p>
          <p class="text-ink-dim">
            {{ t("console.reconciliation.perReplica") }}
          </p>
        </div>
      </section>

      <!-- Channels: record cards, never inboxes. -->
      <section class="space-y-3 border-t border-rule pt-5">
        <div class="flex items-baseline justify-between">
          <h2 class="pixel text-base font-bold uppercase">
            {{ t("console.channels.title", { count: platform.channels.length }) }}
          </h2>
          <span class="text-xs text-ink-dim">
            {{ t("console.channels.managedCount", { count: platform.managed }) }}
          </span>
        </div>
        <div class="overflow-x-auto border border-rule">
          <table class="w-full text-xs">
            <thead class="bg-muted/50 text-left">
              <tr>
                <th class="p-2 font-medium">{{ t("console.channels.headers.channel") }}</th>
                <th class="p-2 font-medium">{{ t("console.channels.headers.origin") }}</th>
                <th class="p-2 font-medium">{{ t("console.channels.headers.members") }}</th>
                <th class="p-2 font-medium">{{ t("console.channels.headers.groups") }}</th>
                <th class="p-2 font-medium">{{ t("console.channels.headers.receipts") }}</th>
                <th class="p-2" />
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="card in platform.channels"
                :key="card.id"
                class="border-t border-rule"
              >
                <td class="p-2 font-medium">{{ card.id }}</td>
                <td class="p-2">
                  <Badge :variant="card.origin_managed ? 'secondary' : 'muted'">
                    {{ card.origin_managed ? "managed" : "dynamic" }}
                  </Badge>
                </td>
                <td class="p-2">{{ card.members }}</td>
                <td class="p-2">{{ card.group_bindings }}</td>
                <td class="p-2">
                  {{ card.receipts_enabled ? t("console.yes") : t("console.no") }}
                </td>
                <td class="p-2 text-right">
                  <Button
                    variant="outline"
                    size="sm"
                    @click="platform.join(card.id)"
                  >
                    {{ t("console.channels.join") }}
                  </Button>
                </td>
              </tr>
            </tbody>
          </table>
        </div>
        <p class="text-xs text-ink-dim">
          {{ t("console.channels.joinHint") }}
        </p>
      </section>

      <!-- Trust policies: read-only, because GitOps owns them. -->
      <section class="space-y-3 border-t border-rule pt-5">
        <h2 class="pixel text-base font-bold uppercase">
          {{ t("console.policies.title", { count: platform.policies.length }) }}
        </h2>
        <p v-if="!platform.policies.length" class="text-xs text-ink-dim">
          {{ t("console.policies.none") }}
        </p>
        <div
          v-else
          class="overflow-x-auto border border-rule"
        >
          <table class="w-full text-xs">
            <thead class="bg-muted/50 text-left">
              <tr>
                <th class="p-2 font-medium">Issuer</th>
                <th class="p-2 font-medium">Subject</th>
                <th class="p-2 font-medium">{{ t("console.policies.headers.channels") }}</th>
                <th class="p-2 font-medium">Scopes</th>
                <th class="p-2 font-medium">{{ t("console.policies.headers.scope") }}</th>
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="(policy, index) in platform.policies"
                :key="`${policy.issuer}-${policy.subject}-${index}`"
                class="border-t border-rule"
              >
                <td class="p-2 font-mono">{{ policy.issuer }}</td>
                <td class="p-2 font-mono">{{ policy.subject }}</td>
                <td class="p-2 font-mono">{{ policy.channels.join(", ") }}</td>
                <td class="p-2">{{ policy.scopes.join(", ") }}</td>
                <td class="p-2">
                  <!-- §11: a channel-scoped policy wins, and that is the first
                       thing to look at when one "should" match and does not. -->
                  <Badge :variant="policy.channel_scoped ? 'secondary' : 'muted'">
                    {{ policy.channel_scoped ? t("console.policies.scoped") : t("console.policies.global") }}
                  </Badge>
                </td>
              </tr>
            </tbody>
          </table>
        </div>
        <p class="text-xs text-ink-dim">
          {{ t("console.policies.readOnlyHint") }}
        </p>
      </section>

      <!-- DLQ: deliveries given up on, and the one button that can act on them. -->
      <section class="space-y-3 border-t border-rule pt-5">
        <h2 class="pixel text-base font-bold uppercase">
          {{ t("console.dlq.title", { count: platform.stuck.length }) }}
        </h2>
        <p v-if="!platform.stuck.length" class="text-xs text-ink-dim">
          {{ t("console.dlq.none") }}
        </p>
        <div v-else class="overflow-x-auto border border-rule">
          <table class="w-full text-xs">
            <thead class="bg-muted/50 text-left">
              <tr>
                <th class="p-2 font-medium">{{ t("console.dlq.headers.channel") }}</th>
                <th class="p-2 font-medium">Sink</th>
                <th class="p-2 font-medium">{{ t("console.dlq.headers.attempts") }}</th>
                <th class="p-2 font-medium">{{ t("console.dlq.headers.lastError") }}</th>
                <th class="p-2 font-medium">{{ t("console.dlq.headers.deadAt") }}</th>
                <th class="p-2" />
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="letter in platform.stuck"
                :key="letter.id"
                class="border-t border-rule"
              >
                <td class="p-2 font-medium">{{ letter.channel }}</td>
                <!--
                  The destination in full: the first thing an operator needs is
                  where to go and look, and the sink id carries it by construction
                  (§7.1bis — it is derived from where the sink points).
                -->
                <td class="p-2 font-mono break-all">{{ letter.sink }}</td>
                <td class="p-2">{{ letter.attempts }}</td>
                <td class="p-2 text-status-failure">{{ letter.last_error }}</td>
                <td class="p-2 whitespace-nowrap">{{ exact(letter.dead_at) }}</td>
                <td class="p-2 text-right">
                  <Button
                    variant="outline"
                    size="sm"
                    @click="platform.requeue(letter.id)"
                  >
                    {{ t("console.dlq.retry") }}
                  </Button>
                </td>
              </tr>
            </tbody>
          </table>
        </div>
        <p v-if="platform.stuck.length" class="text-xs text-ink-dim">
          {{ t("console.dlq.retryHint") }}
        </p>
      </section>

      <!-- Audit: the after-incident tool. -->
      <section class="space-y-3 border-t border-rule pt-5">
        <h2 class="pixel text-base font-bold uppercase">
          {{ t("console.audit.title", { count: platform.entries.length }) }}
        </h2>
        <p v-if="!platform.entries.length" class="text-xs text-ink-dim">
          {{ t("console.audit.none") }}
        </p>
        <ul v-else class="space-y-1">
          <li
            v-for="(entry, index) in platform.entries"
            :key="`${entry.at}-${index}`"
            class="flex flex-wrap items-baseline gap-2 border border-rule p-2 text-xs"
          >
            <Badge variant="muted">{{ entry.action }}</Badge>
            <span class="font-mono">{{ entry.actor.kind }}</span>
            <span v-if="entry.channel" class="text-ink-dim">
              {{ t("console.audit.on", { channel: entry.channel }) }}
            </span>
            <time :datetime="entry.at" class="ml-auto text-ink-dim">
              {{ exact(entry.at) }}
            </time>
          </li>
        </ul>
      </section>
    </template>
  </div>
  </div>
</template>
