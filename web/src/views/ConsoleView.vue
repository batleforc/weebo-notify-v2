<script setup lang="ts">
/**
 * The platform console (§10.4bis): states and counters that can be acted on.
 *
 * Deliberately not a dashboard. §10.1 refuses the generic kind, and §13 keeps time
 * series in Grafana — duplicating them here would be a screen nobody trusts because
 * it disagrees with the one that has the data.
 *
 * Three panels, each answering a question somebody actually arrives with:
 *   * **Canaux** — is this instance configured the way we think it is;
 *   * **Trust policies** — why was that pipeline refused;
 *   * **Audit** — what happened, and who did it.
 *
 * There is no fourth panel for the sinks DLQ, which §10.4bis lists: no sink exists
 * yet (§14.1), and a tab reading "nothing is stuck" from a queue nobody fills is a
 * green light wired to nothing.
 */
import { onMounted } from "vue";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Separator } from "@/components/ui/separator";
import { Skeleton } from "@/components/ui/skeleton";
import { exact } from "@/lib/format";
import { useConsole } from "@/stores/console";

const platform = useConsole();

onMounted(() => {
  void platform.load();
});
</script>

<template>
  <div class="mx-auto max-w-4xl space-y-6 p-6">
    <header class="space-y-1">
      <h1 class="text-lg font-semibold">Console plateforme</h1>
      <p class="text-sm text-muted-foreground">
        Métadonnées et opérations. Le contenu des canaux n'est pas lisible d'ici :
        rejoindre un canal est le chemin explicite, et il est tracé (§5.4).
      </p>
    </header>

    <div v-if="platform.loading" class="space-y-3">
      <Skeleton class="h-5 w-1/3" />
      <Skeleton class="h-24 w-full" />
    </div>

    <!-- The expected answer for almost everybody, and not an error. -->
    <div
      v-else-if="platform.allowed === false"
      class="space-y-2 rounded-md border border-border p-6 text-sm"
    >
      <p class="font-medium">Cette console est réservée au rôle plateforme.</p>
      <p class="text-muted-foreground">
        Il se déclare en GitOps (<code>[admin] oidc_groups</code>, §11) et aucune
        API ne permet de se l'accorder — il n'y a donc personne à qui le demander
        ici : c'est une modification de configuration.
      </p>
    </div>

    <template v-else>
      <p v-if="platform.error" class="text-sm text-destructive">
        {{ platform.error }}
      </p>

      <!-- GitOps: what the last pass did, on the replica that answered. -->
      <section class="space-y-2">
        <h2 class="text-sm font-semibold">Réconciliation</h2>
        <p v-if="!platform.gitops" class="text-xs text-muted-foreground">
          Aucune passe terminée sur ce replica. La passe de démarrage tourne avant
          que le socket soit ouvert, donc voir ceci veut dire qu'elle a échoué.
        </p>
        <div v-else class="space-y-1 rounded-md border border-border p-3 text-xs">
          <p>
            Dernière passe {{ exact(platform.gitops.at) }} ·
            {{ platform.gitops.channels }} canaux ·
            {{ platform.gitops.accounts }} service accounts ·
            {{ platform.gitops.removed }} retirés
          </p>
          <!--
            The one actionable line: a binding by `email` or `oidc_sub` grants
            nothing until that person logs in once (§14.1, M1.8). Without a place
            to see it, the symptom is "alice cannot see the channel" with nothing
            to connect it to.
          -->
          <p
            v-if="platform.gitops.pending.length"
            class="text-status-warning"
          >
            {{ platform.gitops.pending.length }} liaison(s) en attente d'une
            première connexion :
            <span class="font-mono">{{
              platform.gitops.pending.join(", ")
            }}</span>
          </p>
          <p class="text-muted-foreground">
            Chiffres de ce replica : contrairement à la purge, tous réconcilient
            (§11), donc deux lectures peuvent différer d'un tick.
          </p>
        </div>
      </section>

      <Separator />

      <!-- Channels: record cards, never inboxes. -->
      <section class="space-y-2">
        <div class="flex items-baseline justify-between">
          <h2 class="text-sm font-semibold">
            Canaux · {{ platform.channels.length }}
          </h2>
          <span class="text-xs text-muted-foreground">
            dont {{ platform.managed }} managés par la config
          </span>
        </div>
        <div class="overflow-x-auto rounded-md border border-border">
          <table class="w-full text-xs">
            <thead class="bg-muted/50 text-left">
              <tr>
                <th class="p-2 font-medium">Canal</th>
                <th class="p-2 font-medium">Origine</th>
                <th class="p-2 font-medium">Membres</th>
                <th class="p-2 font-medium">Groupes</th>
                <th class="p-2 font-medium">Accusés</th>
                <th class="p-2" />
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="card in platform.channels"
                :key="card.id"
                class="border-t border-border"
              >
                <td class="p-2 font-medium">{{ card.id }}</td>
                <td class="p-2">
                  <Badge :variant="card.origin_managed ? 'secondary' : 'muted'">
                    {{ card.origin_managed ? "managed" : "dynamic" }}
                  </Badge>
                </td>
                <td class="p-2">{{ card.members }}</td>
                <td class="p-2">{{ card.group_bindings }}</td>
                <td class="p-2">{{ card.receipts_enabled ? "oui" : "non" }}</td>
                <td class="p-2 text-right">
                  <Button
                    variant="outline"
                    size="sm"
                    @click="platform.join(card.id)"
                  >
                    Rejoindre
                  </Button>
                </td>
              </tr>
            </tbody>
          </table>
        </div>
        <p class="text-xs text-muted-foreground">
          « Rejoindre » ajoute le lecteur au canal en <code>subscribe</code> seul
          et écrit une entrée d'audit. Sur un canal managé, la réconciliation
          reprendra le membership : un accès durable s'écrit dans la config (§11).
        </p>
      </section>

      <Separator />

      <!-- Trust policies: read-only, because GitOps owns them. -->
      <section class="space-y-2">
        <h2 class="text-sm font-semibold">
          Trust policies · {{ platform.policies.length }}
        </h2>
        <p v-if="!platform.policies.length" class="text-xs text-muted-foreground">
          Aucune. Aucun job CI ne peut échanger de jeton sur cette instance (§5.3).
        </p>
        <div
          v-else
          class="overflow-x-auto rounded-md border border-border"
        >
          <table class="w-full text-xs">
            <thead class="bg-muted/50 text-left">
              <tr>
                <th class="p-2 font-medium">Issuer</th>
                <th class="p-2 font-medium">Subject</th>
                <th class="p-2 font-medium">Canaux</th>
                <th class="p-2 font-medium">Scopes</th>
                <th class="p-2 font-medium">Portée</th>
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="(policy, index) in platform.policies"
                :key="`${policy.issuer}-${policy.subject}-${index}`"
                class="border-t border-border"
              >
                <td class="p-2 font-mono">{{ policy.issuer }}</td>
                <td class="p-2 font-mono">{{ policy.subject }}</td>
                <td class="p-2 font-mono">{{ policy.channels.join(", ") }}</td>
                <td class="p-2">{{ policy.scopes.join(", ") }}</td>
                <td class="p-2">
                  <!-- §11: a channel-scoped policy wins, and that is the first
                       thing to look at when one "should" match and does not. -->
                  <Badge :variant="policy.channel_scoped ? 'secondary' : 'muted'">
                    {{ policy.channel_scoped ? "canal" : "globale" }}
                  </Badge>
                </td>
              </tr>
            </tbody>
          </table>
        </div>
        <p class="text-xs text-muted-foreground">
          En lecture seule : la config les possède (§11), donc un bouton d'édition
          proposerait ce que la prochaine réconciliation reprendrait.
        </p>
      </section>

      <Separator />

      <!-- Audit: the after-incident tool. -->
      <section class="space-y-2">
        <h2 class="text-sm font-semibold">
          Audit · {{ platform.entries.length }} dernière(s) entrée(s)
        </h2>
        <p v-if="!platform.entries.length" class="text-xs text-muted-foreground">
          Rien d'enregistré pour l'instant.
        </p>
        <ul v-else class="space-y-1">
          <li
            v-for="(entry, index) in platform.entries"
            :key="`${entry.at}-${index}`"
            class="flex flex-wrap items-baseline gap-2 rounded-md border border-border p-2 text-xs"
          >
            <Badge variant="muted">{{ entry.action }}</Badge>
            <span class="font-mono">{{ entry.actor.kind }}</span>
            <span v-if="entry.channel" class="text-muted-foreground">
              sur {{ entry.channel }}
            </span>
            <time :datetime="entry.at" class="ml-auto text-muted-foreground">
              {{ exact(entry.at) }}
            </time>
          </li>
        </ul>
      </section>
    </template>
  </div>
</template>
