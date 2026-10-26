import { createApp } from "vue";
import { i18n } from "@/i18n";
import { markDesktop } from "@/lib/desktop";
import InstanceView from "@/views/InstanceView.vue";
import "@/styles/theme.css";

// The first-launch screen (RFC 0001), on its own: the app proper reads
// `config.ts`, which has nothing to read until an instance is chosen.
markDesktop(document.documentElement);
createApp(InstanceView).use(i18n).mount("#app");
