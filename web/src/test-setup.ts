/** Defines a fake `window.__ENV__` before `src/config.ts` is imported (ADR-0021). Never contacted. */
window.__ENV__ = {
  apiBaseUrl: "https://notify.test",
  oidcIssuer: "https://idp.test",
  oidcClientId: "weebo-notify",
};
