<script setup lang="ts">
/** Redeems the IdP's code (§12.5), replacing the URL so Back can't land on a spent code. */
import { onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";

import { useSession } from "@/auth/session";
import { Button } from "@/components/ui/button";

const router = useRouter();
const session = useSession();
const { t } = useI18n();

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
      <p class="font-medium text-signal">{{ t("callback.failed") }}</p>
      <p class="text-ink-dim">{{ failure }}</p>
      <Button variant="outline" size="sm" @click="router.replace('/login')">
        {{ t("callback.retry") }}
      </Button>
    </div>
    <p v-else class="text-ink-dim">{{ t("callback.inProgress") }}</p>
  </main>
</template>
