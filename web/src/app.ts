import { createPinia } from "pinia";
import { createApp } from "vue";
import App from "@/App.vue";
import { installAuth } from "@/api/client";
import { i18n } from "@/i18n";
import { markDesktop } from "@/lib/desktop";
import { router } from "@/router";
import "@/styles/theme.css";

// Before mount, so the first paint already has desktop styling (§10bis.3bis).
markDesktop(document.documentElement);

const app = createApp(App);

app.use(createPinia());
app.use(i18n);
// Before the router: the first navigation runs a guard that restores the session,
// and the guard needs a client that knows how to carry a token.
installAuth();
app.use(router);

app.mount("#app");
