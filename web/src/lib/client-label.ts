/** "Firefox · Android" from a User-Agent; the raw string when nothing matches. */
export function clientLabel(userAgent: string): string {
  if (/^notify\//i.test(userAgent)) return "notify CLI";
  const browser = (
    [
      ["Edg/", "Edge"],
      ["Firefox/", "Firefox"],
      ["Chrome/", "Chrome"],
      ["Safari/", "Safari"],
    ] as const
  ).find(([token]) => userAgent.includes(token))?.[1];
  const os = (
    [
      [/Android/, "Android"],
      [/iPhone|iPad/, "iOS"],
      [/Windows/, "Windows"],
      [/Mac OS X/, "macOS"],
      [/Linux/, "Linux"],
    ] as const
  ).find(([pattern]) => pattern.test(userAgent))?.[1];
  const label = [/tauri/i.test(userAgent) ? "Desktop app" : browser, os]
    .filter(Boolean)
    .join(" · ");
  return label || userAgent || "?";
}
