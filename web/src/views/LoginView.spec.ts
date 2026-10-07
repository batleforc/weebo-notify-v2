/**
 * Login failures. On desktop a launcher can open nothing yet report success, so
 * `login()` hangs on the loopback listener: the reopen button must exist then,
 * and a fresh sign-in only once the listener is gone (port 1420).
 */
import { flushPromises, mount } from "@vue/test-utils";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { reactive } from "vue";
import { createI18n } from "vue-i18n";
import { createMemoryHistory, createRouter } from "vue-router";

import en from "@/locales/en.json";
import LoginView from "./LoginView.vue";

// Messages are precompiled ASTs, so labels are written out; `locales.spec.ts` checks the keys.
const SIGN_IN = "Sign in";
const RETRY = "Retry";
const REOPEN = "Reopen the browser";

const AUTHORIZE_URL =
  "http://localhost:9110/authorize?response_type=code&client_id=weebo-notify";

const { login, openExternal } = vi.hoisted(() => ({
  login: vi.fn(),
  openExternal: vi.fn(),
}));

// `session` is referenced lazily — the factory is hoisted above its declaration,
// but nothing reads it until the component calls `useSession()` during mount.
vi.mock("@/auth/session", () => ({ useSession: () => session }));
vi.mock("@/auth/loopback", () => ({ openExternal }));

/** Reactive, so a test can put the view in the pending state and watch it leave. */
const session = reactive({
  expired: false,
  pendingAuthorizeUrl: null as string | null,
  login,
});

const routes = [
  { path: "/", name: "inbox", component: { template: "<div />" } },
  { path: "/login", name: "login", component: { template: "<div />" } },
];

async function loginView() {
  const router = createRouter({ history: createMemoryHistory(), routes });
  await router.push("/login");
  await router.isReady();

  const i18n = createI18n({ legacy: false, locale: "en", messages: { en } });
  return mount(LoginView, { global: { plugins: [router, i18n] } });
}

/** The buttons, in the order the card stacks them. */
function buttons(wrapper: Awaited<ReturnType<typeof loginView>>) {
  return wrapper.findAll("button");
}

describe("LoginView", () => {
  beforeEach(() => {
    login.mockReset();
    openExternal.mockReset();
    openExternal.mockResolvedValue(undefined);
    session.expired = false;
    session.pendingAuthorizeUrl = null;
  });

  it("offers only the sign-in button until something needs a second chance", async () => {
    login.mockResolvedValue(undefined);
    const wrapper = await loginView();

    expect(buttons(wrapper)).toHaveLength(1);
    expect(wrapper.text()).toContain(SIGN_IN);
    expect(wrapper.text()).not.toContain(RETRY);
    expect(wrapper.text()).not.toContain(REOPEN);
  });

  describe("when a sign-in fails outright", () => {
    it("shows the retry button, under the sign-in one", async () => {
      login.mockRejectedValue(new Error("discovery document unreachable"));
      const wrapper = await loginView();

      await buttons(wrapper)[0].trigger("click");
      await flushPromises();

      const after = buttons(wrapper);
      expect(after).toHaveLength(2);
      // Order matters: the retry is the lesser thing to do *after* the first one.
      expect(after[0].text()).toContain(SIGN_IN);
      expect(after[1].text()).toContain(RETRY);
      // The provider's own words, not a generic "connexion impossible" — a dev
      // stack missing `task dev:oidc` has to be debuggable from this screen.
      expect(wrapper.text()).toContain("discovery document unreachable");
    });

    it("re-runs the login when retry is clicked", async () => {
      login.mockRejectedValue(new Error("provider down"));
      const wrapper = await loginView();

      await buttons(wrapper)[0].trigger("click");
      await flushPromises();
      expect(login).toHaveBeenCalledTimes(1);

      await buttons(wrapper)[1].trigger("click");
      await flushPromises();
      expect(login).toHaveBeenCalledTimes(2);
      expect(openExternal).not.toHaveBeenCalled();
    });

    it("hides the retry again once the provider answers", async () => {
      login.mockRejectedValue(new Error("provider down"));
      const wrapper = await loginView();

      await buttons(wrapper)[0].trigger("click");
      await flushPromises();
      expect(buttons(wrapper)).toHaveLength(2);

      login.mockResolvedValue(undefined);
      await buttons(wrapper)[1].trigger("click");
      await flushPromises();

      expect(buttons(wrapper)).toHaveLength(1);
    });
  });

  describe("when a desktop sign-in is pending and no browser opened", () => {
    it("offers to reopen, without waiting for a failure that never comes", async () => {
      // The state the shell is actually in: waiting on loopback, nothing rejected.
      session.pendingAuthorizeUrl = AUTHORIZE_URL;
      const wrapper = await loginView();

      const shown = buttons(wrapper);
      expect(shown).toHaveLength(2);
      expect(shown[1].text()).toContain(REOPEN);
      // Not "Retry": there is nothing to retry, the flow is alive and only the
      // browser is missing.
      expect(wrapper.text()).not.toContain(RETRY);
    });

    it("stays clickable while the primary button is disabled", async () => {
      session.pendingAuthorizeUrl = AUTHORIZE_URL;
      const wrapper = await loginView();

      // Whatever `going` is doing to the button above, this one is the way out.
      expect(buttons(wrapper)[1].attributes("disabled")).toBeUndefined();
    });

    it("reopens the same URL rather than starting a second sign-in", async () => {
      session.pendingAuthorizeUrl = AUTHORIZE_URL;
      const wrapper = await loginView();

      await buttons(wrapper)[1].trigger("click");
      await flushPromises();

      expect(openExternal).toHaveBeenCalledWith(AUTHORIZE_URL);
      // The listener is already holding port 1420. A second `login()` would not
      // retry anything, it would collide with the first attempt.
      expect(login).not.toHaveBeenCalled();
    });

    it("shows the URL itself, for when no launcher works at all", async () => {
      session.pendingAuthorizeUrl = AUTHORIZE_URL;
      const wrapper = await loginView();

      expect(wrapper.text()).toContain(AUTHORIZE_URL);
    });

    it("reports a launcher that cannot be reached", async () => {
      session.pendingAuthorizeUrl = AUTHORIZE_URL;
      openExternal.mockRejectedValue(new Error("no opener available"));
      const wrapper = await loginView();

      await buttons(wrapper)[1].trigger("click");
      await flushPromises();

      expect(wrapper.text()).toContain("no opener available");
      // And the URL stays put: copying it by hand finishes the same flow.
      expect(wrapper.text()).toContain(AUTHORIZE_URL);
    });

    it("goes back to offering a retry once the attempt is over", async () => {
      session.pendingAuthorizeUrl = AUTHORIZE_URL;
      const wrapper = await loginView();
      expect(buttons(wrapper)[1].text()).toContain(REOPEN);

      // `login()`'s `finally` clears this on both paths — the listener timed out.
      session.pendingAuthorizeUrl = null;
      await flushPromises();

      expect(buttons(wrapper)).toHaveLength(1);
      expect(wrapper.text()).not.toContain(REOPEN);
    });
  });
});
