import { isDesktop } from "@/lib/desktop";

// On desktop the chosen instance (RFC 0001) replaces any baked `env.js`, and has to
// before the app's modules load: `config.ts` reads `window.__ENV__` on import.
if (isDesktop()) {
  const { adoptChosenInstance } = await import("@/lib/instance");
  await adoptChosenInstance().catch((error: unknown) => {
    console.error("weebo-notify: the chosen server could not be read", error);
  });
}

await import("./app");
