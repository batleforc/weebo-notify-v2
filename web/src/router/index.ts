/** Routes and the auth guard. Permalinks `/n/<id>`, `/c/<id>` mirror desktop deep links (§10.4). */

import { createRouter, createWebHistory } from "vue-router";

import { useSession } from "@/auth/session";
import AppLayout from "@/views/AppLayout.vue";
import InboxView from "@/views/InboxView.vue";
import LoginView from "@/views/LoginView.vue";

export const router = createRouter({
  history: createWebHistory(),
  routes: [
    {
      path: "/",
      component: AppLayout,
      children: [
        { path: "", name: "inbox", component: InboxView },
        // Context-free permalink; inside a channel the nested route keeps the channel in the URL.
        { path: "n/:id", name: "notification", component: InboxView },
        { path: "c/:channel", name: "channel", component: InboxView },
        {
          path: "c/:channel/n/:id",
          name: "channel-notification",
          component: InboxView,
        },
        {
          path: "settings",
          name: "settings",
          component: () => import("@/views/SettingsView.vue"),
        },
        {
          path: "admin",
          name: "admin",
          component: () => import("@/views/AdminView.vue"),
        },
        {
          path: "console",
          name: "console",
          component: () => import("@/views/ConsoleView.vue"),
        },
      ],
    },
    { path: "/login", name: "login", component: LoginView },
    {
      path: "/auth/callback",
      name: "callback",
      component: () => import("@/views/CallbackView.vue"),
    },
    // Anything else is an old link or a typo; the inbox is a better answer than a
    // 404 page nobody maintains.
    { path: "/:pathMatch(.*)*", redirect: { name: "inbox" } },
  ],
});

router.beforeEach(async (to) => {
  if (to.name === "login" || to.name === "callback") {
    return true;
  }

  const session = useSession();
  // `restore` uses the refresh token this tab holds, so a reload lands back in
  // the inbox instead of bouncing through the provider (see `auth/session.ts`).
  if (await session.restore()) {
    return true;
  }

  // `returnTo` is what makes a permalink survive a cold browser: open `/n/<id>`
  // signed out, come back to `/n/<id>` signed in.
  return { name: "login", query: { returnTo: to.fullPath } };
});
