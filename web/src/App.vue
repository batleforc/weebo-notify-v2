<script setup lang="ts">
import { ConfigProvider } from "reka-ui";
</script>

<template>
  <!-- `useId` keeps Reka's generated ids stable, which matters for the ARIA
       relations the components emit. -->
  <ConfigProvider>
    <RouterView />
  </ConfigProvider>
</template>
