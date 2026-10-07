<script setup lang="ts">
/** One button, one way in (§5.1): the app never sees a credential. */
import { LogIn, RotateCcw } from "@lucide/vue";
import { ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRoute, useRouter } from "vue-router";

import { openExternal } from "@/auth/loopback";
import { useSession } from "@/auth/session";
import Wordmark from "@/components/layout/Wordmark.vue";
import StatusDot from "@/components/notification/StatusDot.vue";
import { Button } from "@/components/ui/button";

const session = useSession();
const route = useRoute();
const router = useRouter();
const { t } = useI18n();

const failure = ref<string | null>(null);

const STEPS = [
  { state: "open", status: "info", word: t("state.open"), class: "text-ink" },
  {
    state: "working",
    status: "info",
    word: t("state.working"),
    class: "text-copper",
  },
  {
    state: "closed",
    status: "success",
    word: t("status.success"),
    class: "text-status-success",
  },
] as const;
const going = ref(false);

async function signIn(): Promise<void> {
  going.value = true;
  failure.value = null;
  try {
    const returnTo =
      typeof route.query.returnTo === "string" ? route.query.returnTo : "/";
    // Desktop only: the shell signs in without navigating away.
    const target = await session.login(returnTo);
    if (target) {
      await router.replace(target);
    }
  } catch (error) {
    // Say which failure (provider down, unreadable discovery) to help debugging.
    failure.value = error instanceof Error ? error.message : String(error);
    going.value = false;
  }
}

/**
 * While a desktop sign-in is pending, the loopback listener holds port 1420, so
 * "retry" reopens the browser; afterwards it starts a new sign-in.
 */
async function retry(): Promise<void> {
  const pending = session.pendingAuthorizeUrl;
  if (!pending) {
    await signIn();
    return;
  }

  failure.value = null;
  try {
    await openExternal(pending);
  } catch (error) {
    // The shell could not reach a launcher at all. Say so, and leave the URL on
    // screen: copying it into a browser by hand finishes the same flow.
    failure.value = error instanceof Error ? error.message : String(error);
  }
}
</script>

<template>
  <main class="grid min-h-dvh grid-rows-[1fr_auto] lg:grid-cols-[minmax(0,1fr)_minmax(0,1fr)]">
    <section class="flex items-center px-6 py-12 sm:px-10 lg:px-16">
      <div class="w-full max-w-md space-y-8">
        <Wordmark large />

        <div class="space-y-3">
          <h1 class="display uppercase">{{ t("login.headline") }}</h1>
          <p class="max-w-[52ch] text-sm text-ink-dim">{{ t("login.tagline") }}</p>
        </div>

        <!-- The mechanism, shown: one notification, three revisions. -->
        <ol class="border-y border-rule" aria-hidden="true">
          <li v-for="step in STEPS" :key="step.state + step.status" class="rule-row flex items-center gap-3 py-2.5 last:border-b-0">
            <StatusDot :status="step.status" :state="step.state" />
            <span class="min-w-0 flex-1 truncate text-sm">deploy api · #4812</span>
            <span :class="['meta', step.class]">{{ step.word }}</span>
          </li>
        </ol>

        <div class="space-y-3">
          <p v-if="session.expired" class="text-sm text-copper">{{ t("login.expired") }}</p>
          <p v-if="failure" class="text-sm text-signal">{{ failure }}</p>

          <Button size="lg" class="w-full" :disabled="going" @click="signIn">
            <LogIn />
            {{ going ? t("login.redirecting") : t("login.signIn") }}
          </Button>

          <!-- Also while a desktop sign-in is pending: that state has no other way out. -->
          <Button v-if="failure || session.pendingAuthorizeUrl" variant="outline" class="w-full" @click="retry">
            <RotateCcw />
            {{ session.pendingAuthorizeUrl ? t("login.reopen") : t("login.retry") }}
          </Button>

          <!-- Last resort: selectable text, never navigated to by the shell. -->
          <p v-if="session.pendingAuthorizeUrl" class="select-all text-xs text-ink-dim [overflow-wrap:anywhere]">
            {{ session.pendingAuthorizeUrl }}
          </p>
        </div>
      </div>
    </section>

    <!-- The plate: the mark at poster scale, texture only where no text can land. -->
    <aside class="relative hidden items-center justify-center border-l border-rule lg:flex" aria-hidden="true">
      <div class="dot-field absolute inset-0 opacity-60" />
      <div class="relative grid grid-cols-3 gap-6 bg-ground p-10">
        <span v-for="i in 9" :key="i" :class="['size-16', i === 5 ? 'bg-signal' : 'bg-ink']" />
      </div>
    </aside>

    <footer class="flex items-center gap-4 border-t border-rule px-6 py-3 sm:px-10 lg:col-span-2">
      <span class="meta">OIDC · PKCE</span>
      <span class="dot-field h-3 flex-1" aria-hidden="true" />
      <span class="meta">self-hosted</span>
    </footer>
  </main>
</template>
