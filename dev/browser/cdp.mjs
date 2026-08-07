#!/usr/bin/env node
/**
 * Drive the browser that lives in the `browser` sidecar (design §12.4).
 *
 * # Why this exists
 *
 * Three debts — D19, D21, D26 — are open with the same three words, « écrit,
 * jamais vu ». The web suite runs under jsdom, which answers questions about
 * logic and cannot answer whether a command palette looks right or whether an
 * image shifts the row it lands in. M4tervicies put a real Chrome in the pod;
 * this is the half that drives it, and it lives in `tools` for the reason the
 * devfile gives: the browser renders, whatever asks it questions does not have
 * to be in the same container.
 *
 * # Why no dependency
 *
 * Node 24 ships a global `WebSocket`, and CDP is a JSON protocol over one. A
 * Playwright or puppeteer install would pull a browser download this pod has no
 * business fetching — the browser is already here, scheduled by the devfile —
 * and both would then be asked to *connect over CDP* anyway. So the whole client
 * is the sixty lines below, which is less than the lockfile entry would be.
 *
 * This is the same rule `xtask` follows for the VAPID and bcrypt keys: nothing
 * here assumes a tool the image might not ship. Node is not an assumption — the
 * frontend cannot be built without it.
 *
 * # It drives the page a human is looking at
 *
 * Deliberately the *existing* target rather than a fresh one from `/json/new`.
 * The point of the sidecar is `novnc` on a public route: someone watches the
 * browser while this script works it. A driver that opens its own headless
 * target would move exactly the thing the human is trying to see out of view.
 *
 * # Usage
 *
 *   node dev/browser/cdp.mjs open <url>          navigate, wait for load
 *   node dev/browser/cdp.mjs shot <file.png>     screenshot the viewport
 *   node dev/browser/cdp.mjs eval <expression>   evaluate, print the JSON result
 *   node dev/browser/cdp.mjs key <combo>         e.g. ctrl+k, Escape, Enter
 *   node dev/browser/cdp.mjs type <text>         insert text as if typed
 *   node dev/browser/cdp.mjs where                print the current URL and title
 *   node dev/browser/cdp.mjs logs                 drain console + page errors
 */

import { writeFileSync } from "node:fs";

const ENDPOINT = process.env.WNK_CDP ?? "http://127.0.0.1:9222";

/** The one page target, which is the one on the screen. */
async function pageTarget() {
  const response = await fetch(`${ENDPOINT}/json/list`);
  if (!response.ok) {
    throw new Error(
      `CDP at ${ENDPOINT} answered ${response.status}. Is the browser sidecar up? (devfile.yaml)`,
    );
  }
  const targets = await response.json();
  const page = targets.find((t) => t.type === "page");
  if (!page) {
    throw new Error("the browser has no page target — nothing to drive");
  }
  return page;
}

/**
 * A CDP session: `send` resolves on the matching id, `once` on the next event.
 *
 * Errors from the browser are raised rather than resolved, because a command
 * that silently returned `{error}` would produce a screenshot of the wrong page.
 */
async function connect(url) {
  const socket = new WebSocket(url);
  const pending = new Map();
  const waiters = new Map();
  const drained = [];
  let id = 0;

  await new Promise((resolve, reject) => {
    socket.addEventListener("open", resolve, { once: true });
    socket.addEventListener("error", () => reject(new Error(`cannot open ${url}`)), {
      once: true,
    });
  });

  socket.addEventListener("message", (event) => {
    const message = JSON.parse(event.data);
    if (message.id !== undefined) {
      const settle = pending.get(message.id);
      pending.delete(message.id);
      if (settle) {
        message.error
          ? settle.reject(new Error(`${message.error.message} (${message.method ?? ""})`))
          : settle.resolve(message.result);
      }
      return;
    }
    drained.push(message);
    const waiting = waiters.get(message.method);
    if (waiting) {
      waiters.delete(message.method);
      waiting(message.params);
    }
  });

  return {
    send(method, params = {}) {
      id += 1;
      const current = id;
      return new Promise((resolve, reject) => {
        pending.set(current, { resolve, reject });
        socket.send(JSON.stringify({ id: current, method, params }));
      });
    },
    once(method, timeoutMs = 15000) {
      return new Promise((resolve) => {
        waiters.set(method, resolve);
        setTimeout(() => {
          waiters.delete(method);
          resolve(null);
        }, timeoutMs);
      });
    },
    events: drained,
    close: () => socket.close(),
  };
}

/**
 * `ctrl+k` and friends, in the three events a browser actually expects.
 *
 * `rawKeyDown` and not `keyDown`: with a modifier held there is no character to
 * produce, and a `char` event in that case is what makes a shortcut fire twice.
 */
function keyEvents(combo) {
  const parts = combo.split("+");
  const key = parts.pop();
  const modifiers = parts.reduce(
    (mask, name) => mask | ({ alt: 1, ctrl: 2, control: 2, meta: 4, cmd: 4, shift: 8 }[name.toLowerCase()] ?? 0),
    0,
  );

  const named = {
    Escape: { code: "Escape", keyCode: 27 },
    Enter: { code: "Enter", keyCode: 13 },
    Tab: { code: "Tab", keyCode: 9 },
    ArrowDown: { code: "ArrowDown", keyCode: 40 },
    ArrowUp: { code: "ArrowUp", keyCode: 38 },
    Backspace: { code: "Backspace", keyCode: 8 },
  };
  const spec = named[key] ?? {
    code: `Key${key.toUpperCase()}`,
    keyCode: key.toUpperCase().charCodeAt(0),
  };

  const base = {
    key,
    code: spec.code,
    windowsVirtualKeyCode: spec.keyCode,
    nativeVirtualKeyCode: spec.keyCode,
    modifiers,
  };

  return [
    { ...base, type: "rawKeyDown" },
    { ...base, type: "keyUp" },
  ];
}

const [command, ...rest] = process.argv.slice(2);

const target = await pageTarget();
const cdp = await connect(target.webSocketDebuggerUrl);

try {
  switch (command) {
    case "open": {
      await cdp.send("Page.enable");
      // Console and page errors are collected from the moment we navigate, so
      // `logs` afterwards reports what this page did rather than what the last
      // one left behind. A CSP violation shows up here and nowhere else.
      await cdp.send("Runtime.enable");
      await cdp.send("Log.enable");
      await cdp.send("Page.navigate", { url: rest[0] });
      await cdp.once("Page.loadEventFired");
      // The load event fires before a Vue app has mounted anything. One frame is
      // not a guarantee, which is why `shot` is a separate command: look, and if
      // it is mid-render, look again.
      await cdp.send("Runtime.evaluate", {
        expression: "new Promise((r) => requestAnimationFrame(() => setTimeout(r, 250)))",
        awaitPromise: true,
      });
      const where = await cdp.send("Runtime.evaluate", {
        expression: "location.href",
        returnByValue: true,
      });
      console.log(where.result.value);
      break;
    }

    case "shot": {
      const shot = await cdp.send("Page.captureScreenshot", { format: "png" });
      writeFileSync(rest[0], Buffer.from(shot.data, "base64"));
      console.log(rest[0]);
      break;
    }

    case "eval": {
      const result = await cdp.send("Runtime.evaluate", {
        expression: rest.join(" "),
        returnByValue: true,
        awaitPromise: true,
      });
      if (result.exceptionDetails) {
        throw new Error(result.exceptionDetails.exception?.description ?? "evaluation failed");
      }
      console.log(JSON.stringify(result.result.value ?? null, null, 2));
      break;
    }

    case "key": {
      for (const event of keyEvents(rest[0])) {
        await cdp.send("Input.dispatchKeyEvent", event);
      }
      break;
    }

    case "type": {
      await cdp.send("Input.insertText", { text: rest.join(" ") });
      break;
    }

    case "where": {
      const result = await cdp.send("Runtime.evaluate", {
        expression: "JSON.stringify({url: location.href, title: document.title})",
        returnByValue: true,
      });
      console.log(result.result.value);
      break;
    }

    case "logs": {
      // A short listen rather than a snapshot: `Runtime.consoleAPICalled` only
      // arrives while a session is attached, so this is the window in which the
      // page can speak.
      await cdp.send("Runtime.enable");
      await cdp.send("Log.enable");
      await new Promise((r) => setTimeout(r, 1500));
      for (const event of cdp.events) {
        if (event.method === "Runtime.consoleAPICalled") {
          console.log(
            `[${event.params.type}]`,
            event.params.args.map((a) => a.value ?? a.description ?? a.type).join(" "),
          );
        }
        if (event.method === "Log.entryAdded") {
          console.log(`[${event.params.entry.level}]`, event.params.entry.text);
        }
        if (event.method === "Runtime.exceptionThrown") {
          console.log("[exception]", event.params.exceptionDetails.text);
        }
      }
      break;
    }

    default:
      console.error("usage: cdp.mjs open|shot|eval|key|type|where|logs …");
      process.exitCode = 2;
  }
} finally {
  cdp.close();
}
