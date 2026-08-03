<script setup lang="ts">
/**
 * Where the identity provider sends the browser back.
 *
 * The redirect URI is registered in the provider (§12.5), so this route exists at
 * one address and one only. It redeems the code and leaves — the URL carrying the
 * code is replaced rather than pushed, so the back button cannot land on a spent
 * authorization code.
 */
import { onMounted, ref } from "vue";
import { useRouter } from "vue-router";

import { useSession } from "@/auth/session";
import { Button } from "@/components/ui/button";

const router = useRouter();
const session = useSession();

const failure = ref<string | null>(null);

onMounted(async () => {
  try {
    const target = await session.completeLogin(
      new URLSearchParams(window.location.search),
    );
    await router.replace(target);
  } catch (error) {
    failure.value = error instanceof Error ? error.message : String(error);
  }
});
</script>

<template>
  <main class="flex min-h-dvh items-center justify-center p-6 text-sm">
    <div v-if="failure" class="max-w-md space-y-3 text-center">
      <p class="font-medium text-destructive">La connexion a échoué.</p>
      <p class="text-muted-foreground">{{ failure }}</p>
      <Button variant="outline" size="sm" @click="router.replace('/login')">
        Réessayer
      </Button>
    </div>
    <p v-else class="text-muted-foreground">Connexion en cours…</p>
  </main>
</template>
