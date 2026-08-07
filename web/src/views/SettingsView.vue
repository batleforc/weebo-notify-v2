<script setup lang="ts">
/**
 * Settings — profile, avatar, theme, density (§10.4, §2.4).
 *
 * The screen exists to make one thing visible that the API models and no other
 * client shows: **which fields you own**. §2.4 says a field you edited stops being
 * overwritten by your identity provider, and without a way to see and undo that,
 * the rule is invisible until it surprises somebody. So every field says whose it
 * is, and offers to hand it back.
 */
import { RotateCcw, Upload } from "lucide-vue-next";
import { computed, onMounted, ref } from "vue";

import { readProfile, setAvatar, updateProfile } from "@/api/generated";
import type { Profile, ProfileField } from "@/api/generated/types.gen";
import { failureOf } from "@/api/problem";
import { avatarObjectUrl } from "@/lib/avatar";
import { useSession } from "@/auth/session";
import DndPanel from "@/components/settings/DndPanel.vue";
import PushPanel from "@/components/settings/PushPanel.vue";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Separator } from "@/components/ui/separator";
import { useUi } from "@/stores/ui";

const session = useSession();
const ui = useUi();

const profile = ref<Profile | null>(null);
const loading = ref(true);
const saving = ref(false);
const error = ref<string | null>(null);
const file = ref<HTMLInputElement | null>(null);

const draft = ref({ display_name: "", timezone: "", locale: "" });

const claimed = computed(
  () => new Set<ProfileField>(profile.value?.overridden ?? []),
);

/**
 * The avatar, as an object URL.
 *
 * Not `<img src="${apiBaseUrl}/api/v1/avatars/${id}">`, which is what this was
 * and which could never have worked: that route is authenticated and this
 * application keeps zero cookies (§5.1), so a browser `<img>` request carries no
 * credential and answered 401 every time. The bytes are fetched through the API
 * client instead — see `lib/avatar`.
 */
const avatarUrl = ref<string | null>(null);

async function showAvatar(id: string | null | undefined): Promise<void> {
  avatarUrl.value = id ? await avatarObjectUrl(id) : null;
}

function hydrate(next: Profile): void {
  profile.value = next;
  draft.value = {
    display_name: next.display_name ?? "",
    timezone: next.timezone ?? "",
    locale: next.locale ?? "",
  };
}

async function load(): Promise<void> {
  loading.value = true;
  try {
    const answer = await readProfile({ throwOnError: true });
    hydrate(answer.data);
  } catch (caught) {
    error.value = failureOf(caught).message;
  } finally {
    loading.value = false;
  }
}

/**
 * Send only what changed.
 *
 * An absent field means "leave it alone" and an explicit `null` means "hand it
 * back to the provider" (§2.4) — so sending the whole form on every save would
 * claim three fields when somebody edited one.
 */
async function save(): Promise<void> {
  if (!profile.value) {
    return;
  }
  saving.value = true;
  error.value = null;

  const body: Record<string, string | null> = {};
  const current = profile.value;
  if (draft.value.display_name !== (current.display_name ?? "")) {
    body.display_name = draft.value.display_name || null;
  }
  if (draft.value.timezone !== (current.timezone ?? "")) {
    body.timezone = draft.value.timezone || null;
  }
  if (draft.value.locale !== (current.locale ?? "")) {
    body.locale = draft.value.locale || null;
  }

  try {
    const answer = await updateProfile({ body, throwOnError: true });
    hydrate(answer.data);
  } catch (caught) {
    error.value = failureOf(caught).message;
  } finally {
    saving.value = false;
  }
}

/** Hand one field back to the identity provider. */
async function release(
  field: "display_name" | "timezone" | "locale",
): Promise<void> {
  saving.value = true;
  error.value = null;
  try {
    const answer = await updateProfile({
      body: { [field]: null },
      throwOnError: true,
    });
    hydrate(answer.data);
  } catch (caught) {
    error.value = failureOf(caught).message;
  } finally {
    saving.value = false;
  }
}

async function upload(event: globalThis.Event): Promise<void> {
  const picked = (event.target as HTMLInputElement).files?.[0];
  if (!picked) {
    return;
  }

  saving.value = true;
  error.value = null;
  try {
    const answer = await setAvatar({
      // The `File` itself: the generated type is binary, and reading it into an
      // ArrayBuffer first would only copy 512 KiB for nothing.
      body: picked,
      headers: { "content-type": "application/octet-stream" },
      throwOnError: true,
    });
    hydrate(answer.data);
  } catch (caught) {
    // The server refuses a file that is too big, animated or not an image at all
    // (§2.4) — and its `code` is what turns that into a sentence.
    error.value = failureOf(caught).message;
  } finally {
    saving.value = false;
  }
}

onMounted(load);
</script>

<template>
  <div class="mx-auto max-w-2xl space-y-4 overflow-y-auto p-6">
    <h1 class="text-lg font-semibold">Réglages</h1>

    <p v-if="error" class="text-sm text-destructive">{{ error }}</p>

    <Card>
      <CardHeader>
        <CardTitle>Profil</CardTitle>
        <p class="text-sm text-muted-foreground">
          Un champ que tu modifies devient le tien : ton fournisseur d'identité
          cesse de l'écraser à chaque connexion.
        </p>
      </CardHeader>

      <CardContent class="space-y-4">
        <div class="flex items-center gap-4">
          <img
            v-if="avatarUrl"
            :src="avatarUrl"
            alt=""
            class="size-16 rounded-full border border-border object-cover"
          />
          <div
            v-else
            class="flex size-16 items-center justify-center rounded-full border border-dashed border-border text-xs text-muted-foreground"
          >
            aucun
          </div>

          <div class="space-y-1">
            <Button variant="outline" size="sm" :disabled="saving" @click="file?.click()">
              <Upload />
              Changer d'avatar
            </Button>
            <p class="text-xs text-muted-foreground">
              PNG, JPEG ou WebP, 512 KiB au plus. Redimensionné en 256×256 et
              débarrassé de ses métadonnées côté serveur.
            </p>
          </div>
          <input
            ref="file"
            type="file"
            class="hidden"
            accept="image/png,image/jpeg,image/webp"
            @change="upload"
          />
        </div>

        <Separator />

        <label class="block space-y-1">
          <span class="flex items-center gap-2 text-sm font-medium">
            Nom affiché
            <Badge v-if="claimed.has('display_name')" variant="secondary">à toi</Badge>
            <Badge v-else variant="muted">suit ton IdP</Badge>
            <Button
              v-if="claimed.has('display_name')"
              variant="ghost"
              size="sm"
              class="h-6 px-2 text-xs"
              @click="release('display_name')"
            >
              <RotateCcw class="size-3" />
              rendre à l'IdP
            </Button>
          </span>
          <Input v-model="draft.display_name" :disabled="loading || saving" />
        </label>

        <label class="block space-y-1">
          <span class="flex items-center gap-2 text-sm font-medium">
            Fuseau
            <Badge v-if="claimed.has('timezone')" variant="secondary">à toi</Badge>
            <Button
              v-if="claimed.has('timezone')"
              variant="ghost"
              size="sm"
              class="h-6 px-2 text-xs"
              @click="release('timezone')"
            >
              <RotateCcw class="size-3" />
              rendre à l'IdP
            </Button>
          </span>
          <Input
            v-model="draft.timezone"
            placeholder="Europe/Paris"
            :disabled="loading || saving"
          />
          <span class="text-xs text-muted-foreground">
            Sert de défaut aux heures calmes. Vérifié contre la base IANA.
          </span>
        </label>

        <label class="block space-y-1">
          <span class="flex items-center gap-2 text-sm font-medium">
            Langue
            <Badge v-if="claimed.has('locale')" variant="secondary">à toi</Badge>
          </span>
          <Input v-model="draft.locale" placeholder="fr-FR" :disabled="loading || saving" />
        </label>

        <Button :disabled="loading || saving" @click="save">
          {{ saving ? "Enregistrement…" : "Enregistrer" }}
        </Button>
      </CardContent>
    </Card>

    <Card>
      <CardHeader>
        <CardTitle>Apparence</CardTitle>
      </CardHeader>
      <CardContent class="flex flex-wrap items-center gap-2">
        <Button variant="outline" size="sm" @click="ui.cycleTheme()">
          Thème : {{ ui.theme }}
        </Button>
        <Button
          variant="outline"
          size="sm"
          @click="ui.density = ui.density === 'compact' ? 'comfortable' : 'compact'"
        >
          Densité : {{ ui.density }}
        </Button>
      </CardContent>
    </Card>

    <PushPanel />

    <DndPanel />

    <Card>
      <CardHeader>
        <CardTitle>Session</CardTitle>
      </CardHeader>
      <CardContent class="space-y-2 text-sm">
        <p class="text-muted-foreground">
          Connecté en tant que {{ session.displayName }}.
        </p>
        <p v-if="session.groups.length" class="text-xs text-muted-foreground">
          Groupes portés par ton jeton : {{ session.groups.join(", ") }} — l'accès
          aux canaux en découle à chaque requête.
        </p>
        <Button variant="outline" size="sm" @click="session.logout()">
          Se déconnecter
        </Button>
      </CardContent>
    </Card>
  </div>
</template>
