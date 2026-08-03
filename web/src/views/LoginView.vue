<script setup lang="ts">
/**
 * The login screen: one button, because there is exactly one way in (§5.1).
 *
 * No password field, ever — this app never sees a credential. It sends the browser
 * to the identity provider and comes back holding a token, which is what makes the
 * server able to keep zero sessions.
 */
import { LogIn } from "lucide-vue-next";
import { ref } from "vue";
import { useRoute } from "vue-router";

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

const failure = ref<string | null>(null);
const going = ref(false);

async function signIn(): Promise<void> {
  going.value = true;
  failure.value = null;
  try {
    const returnTo =
      typeof route.query.returnTo === "string" ? route.query.returnTo : "/";
    await session.login(returnTo);
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
        <CardTitle>weebo-notify</CardTitle>
        <p class="text-sm text-muted-foreground">
          Notifications d'infrastructure, en temps réel.
        </p>
      </CardHeader>

      <CardContent class="space-y-3">
        <p v-if="session.expired" class="text-sm text-muted-foreground">
          Ta session a expiré. Reconnecte-toi pour reprendre.
        </p>
        <p v-if="failure" class="text-sm text-destructive">{{ failure }}</p>
      </CardContent>

      <CardFooter>
        <Button class="w-full" :disabled="going" @click="signIn">
          <LogIn />
          {{ going ? "Redirection…" : "Se connecter" }}
        </Button>
      </CardFooter>
    </Card>
  </main>
</template>
