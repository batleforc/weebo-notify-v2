import { isDesktop } from "@/lib/desktop";

// On desktop the chosen instance (RFC 0001) replaces any baked `env.js`, and has to
// before the app's modules load: `config.ts` reads `window.__ENV__` on import.
// With neither a chosen nor a baked instance, the first-launch screen asks for one.
let chosen = false;
if (isDesktop()) {
  const { adoptChosenInstance } = await import("@/lib/instance");
  chosen = await adoptChosenInstance().catch((error: unknown) => {
    console.error("weebo-notify: the chosen server could not be read", error);
    return false;
  });
}

if (isDesktop() && !chosen && !window.__ENV__) {
  await import("./instance-app");
} else {
  await import("./app");
}
