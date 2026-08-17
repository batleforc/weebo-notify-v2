<script setup lang="ts">
/**
 * The login screen: one button, because there is exactly one way in (§5.1).
 *
 * No password field, ever — this app never sees a credential. It sends the browser
 * to the identity provider and comes back holding a token, which is what makes the
 * server able to keep zero sessions.
 */
import { LogIn } from "@lucide/vue";
import { ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRoute, useRouter } from "vue-router";

import { useSession } from "@/auth/session";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardFooter,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";

const session = useSession();
const route = useRoute();
const router = useRouter();
const { t } = useI18n();

const failure = ref<string | null>(null);
const going = ref(false);

async function signIn(): Promise<void> {
  going.value = true;
  failure.value = null;
  try {
    const returnTo =
      typeof route.query.returnTo === "string" ? route.query.returnTo : "/";
    // A browser never gets here — the page has navigated to the provider by now.
    // The shell does: it signs in without leaving, so this is where the desktop
    // flow ends, and `target` is the permalink that survived it.
    const target = await session.login(returnTo);
    if (target) {
      await router.replace(target);
    }
  } catch (error) {
    // A provider that is down, or a discovery document we cannot read: the
    // message says which, because "connexion impossible" helps nobody debug a
    // development stack that is missing `task dev:oidc`.
    failure.value = error instanceof Error ? error.message : String(error);
    going.value = false;
  }
}
</script>

<template>
  <main class="flex min-h-dvh items-center justify-center p-6">
    <Card class="w-full max-w-sm">
      <CardHeader>
        <CardTitle>{{ t("app.name") }}</CardTitle>
        <p class="text-sm text-muted-foreground">
          {{ t("login.tagline") }}
        </p>
      </CardHeader>

      <CardContent class="space-y-3">
        <p v-if="session.expired" class="text-sm text-muted-foreground">
          {{ t("login.expired") }}
        </p>
        <p v-if="failure" class="text-sm text-destructive">{{ failure }}</p>
      </CardContent>

      <CardFooter>
        <Button class="w-full" :disabled="going" @click="signIn">
          <LogIn />
          {{ going ? t("login.redirecting") : t("login.signIn") }}
        </Button>
      </CardFooter>
    </Card>
  </main>
</template>
