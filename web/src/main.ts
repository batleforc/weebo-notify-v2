import { createPinia } from "pinia";
import { createApp } from "vue";
import App from "@/App.vue";
import { installAuth } from "@/api/client";
import { router } from "@/router";
import "@/styles/theme.css";

const app = createApp(App);

app.use(createPinia());
// Before the router: the first navigation runs a guard that restores the session,
// and the guard needs a client that knows how to carry a token.
installAuth();
app.use(router);

app.mount("#app");
