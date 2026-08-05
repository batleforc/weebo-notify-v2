/**
 * Routes, and the one guard that matters.
 *
 * The permalinks are `/n/<id>` and `/c/<id>` (§10.4) — deliberately short, because
 * they are pasted into CI logs, sink emails and chat messages, and they mirror the
 * `weebo-notify://` deep links a desktop client will register (§10bis.3quater).
 */

import { createRouter, createWebHistory } from "vue-router";

import { useSession } from "@/auth/session";
import AdminView from "@/views/AdminView.vue";
import AppLayout from "@/views/AppLayout.vue";
import CallbackView from "@/views/CallbackView.vue";
import ConsoleView from "@/views/ConsoleView.vue";
import InboxView from "@/views/InboxView.vue";
import LoginView from "@/views/LoginView.vue";
import SettingsView from "@/views/SettingsView.vue";

export const router = createRouter({
  history: createWebHistory(),
  routes: [
    {
      path: "/",
      component: AppLayout,
      children: [
        { path: "", name: "inbox", component: InboxView },
        { path: "n/:id", name: "notification", component: InboxView },
        { path: "c/:id", name: "channel", component: InboxView },
        { path: "settings", name: "settings", component: SettingsView },
        { path: "admin", name: "admin", component: AdminView },
        { path: "console", name: "console", component: ConsoleView },
      ],
    },
    { path: "/login", name: "login", component: LoginView },
    { path: "/auth/callback", name: "callback", component: CallbackView },
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
