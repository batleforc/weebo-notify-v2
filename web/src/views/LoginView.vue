<script setup lang="ts">
/**
 * The login screen: one button, because there is exactly one way in (§5.1).
 *
 * No password field, ever — this app never sees a credential. It sends the browser
 * to the identity provider and comes back holding a token, which is what makes the
 * server able to keep zero sessions.
 */
import { LogIn, RotateCcw } from "@lucide/vue";
import { ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRoute, useRouter } from "vue-router";

import { openExternal } from "@/auth/loopback";
import { useSession } from "@/auth/session";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardFooter,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";

const session = useSession();
const route = useRoute();
const router = useRouter();
const { t } = useI18n();

const failure = ref<string | null>(null);
const going = ref(false);

async function signIn(): Promise<void> {
  going.value = true;
  failure.value = null;
  try {
    const returnTo =
      typeof route.query.returnTo === "string" ? route.query.returnTo : "/";
    // A browser never gets here — the page has navigated to the provider by now.
    // The shell does: it signs in without leaving, so this is where the desktop
    // flow ends, and `target` is the permalink that survived it.
    const target = await session.login(returnTo);
    if (target) {
      await router.replace(target);
    }
  } catch (error) {
    // A provider that is down, or a discovery document we cannot read: the
    // message says which, because "connexion impossible" helps nobody debug a
    // development stack that is missing `task dev:oidc`.
    failure.value = error instanceof Error ? error.message : String(error);
    going.value = false;
  }
}

/**
 * What "retry" means depends on whether there is still something listening.
 *
 * While a desktop sign-in is pending, the flow is fine and only the browser is
 * missing — the loopback listener is up and holding port 1420, so re-running
 * `signIn` would not retry anything, it would fail on `EADDRINUSE` against our own
 * first attempt. Reopening the same URL is the actual repair, and it costs nothing
 * if a browser did open: the provider sees one more request for a page it is
 * already showing.
 *
 * Once that attempt is over — provider down, listener timed out — there is nothing
 * left to reopen, and asking again is the whole of what retry can mean.
 */
async function retry(): Promise<void> {
  const pending = session.pendingAuthorizeUrl;
  if (!pending) {
    await signIn();
    return;
  }

  failure.value = null;
  try {
    await openExternal(pending);
  } catch (error) {
    // The shell could not reach a launcher at all. Say so, and leave the URL on
    // screen: copying it into a browser by hand finishes the same flow.
    failure.value = error instanceof Error ? error.message : String(error);
  }
}
</script>

<template>
  <main class="flex min-h-dvh items-center justify-center p-6">
    <Card class="w-full max-w-sm">
      <CardHeader>
        <CardTitle>{{ t("app.name") }}</CardTitle>
        <p class="text-sm text-muted-foreground">
          {{ t("login.tagline") }}
        </p>
      </CardHeader>

      <CardContent class="space-y-3">
        <p v-if="session.expired" class="text-sm text-muted-foreground">
          {{ t("login.expired") }}
        </p>
        <p v-if="failure" class="text-sm text-destructive">{{ failure }}</p>
      </CardContent>

      <!--
        `flex-col`: the footer stacks these rather than sitting them side by side,
        so the second button reads as the smaller thing to do after the first one
        failed, not as an equal alternative to it.
      -->
      <CardFooter class="flex-col items-stretch">
        <Button class="w-full" :disabled="going" @click="signIn">
          <LogIn />
          {{ going ? t("login.redirecting") : t("login.signIn") }}
        </Button>

        <!--
          Shown while a desktop sign-in is *pending* as well as after a failure, and
          the pending half is the one that matters: that is the state with no way
          out. `going` disables the button above, nothing has failed yet, and the
          shell sits on a spinner for the five minutes `oauth.rs` gives the listener
          — which is exactly how long a browser that never opened takes to look like
          a broken app.

          Never disabled by `going`, for the same reason: the whole point is to be
          clickable while an attempt is in flight.
        -->
        <Button
          v-if="failure || session.pendingAuthorizeUrl"
          variant="outline"
          class="w-full"
          @click="retry"
        >
          <RotateCcw />
          {{
            session.pendingAuthorizeUrl ? t("login.reopen") : t("login.retry")
          }}
        </Button>

        <!--
          The last resort, and deliberately selectable text rather than a link: if
          no launcher works, this is the only way through, and the shell must not be
          the thing that navigates to it (its own window is the app, not a tab to
          spend on somebody else's login page — `auth/loopback.ts`).
        -->
        <p
          v-if="session.pendingAuthorizeUrl"
          class="w-full select-all break-all text-xs text-muted-foreground"
        >
          {{ session.pendingAuthorizeUrl }}
        </p>
      </CardFooter>
    </Card>
  </main>
</template>
