import { createPinia } from "pinia";
import { createApp } from "vue";
import App from "@/App.vue";
import { installAuth } from "@/api/client";
import { markDesktop } from "@/lib/desktop";
import { router } from "@/router";
import "@/styles/theme.css";

// Before the app mounts, so the first paint is already dressed as a desktop app
// (§10bis.3bis) — a frame of browser styling is exactly the artefact that section
// says must not exist. A no-op in a browser.
markDesktop(document.documentElement);

const app = createApp(App);

app.use(createPinia());
// Before the router: the first navigation runs a guard that restores the session,
// and the guard needs a client that knows how to carry a token.
installAuth();
app.use(router);

app.mount("#app");
