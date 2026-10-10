<script setup lang="ts">
import { ConfigProvider } from "reka-ui";
import { watch } from "vue";
import { useRoute, useRouter } from "vue-router";

import { useSession } from "@/auth/session";

const session = useSession();
const router = useRouter();
const route = useRoute();

// The guard only runs on navigation, so a session that ends while the person sits
// on one page (a refresh the provider refused) would otherwise leave them looking
// at a screen whose every request now fails. `expired` is set exactly there.
watch(
  () => session.expired,
  (expired) => {
    if (expired && route.name !== "login" && route.name !== "callback") {
      void router.replace({
        name: "login",
        query: { returnTo: route.fullPath },
      });
    }
  },
);
</script>

<template>
  <!-- `useId` keeps Reka's generated ids stable, which matters for the ARIA
       relations the components emit. -->
  <ConfigProvider>
    <RouterView />
  </ConfigProvider>
</template>
