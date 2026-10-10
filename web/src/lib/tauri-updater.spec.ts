import { describe, expect, it } from "vitest";

import { updaterConfig } from "./tauri-updater";

const endpoint = "https://releases.example/{{target}}-{{arch}}/latest.json";

describe("updaterConfig", () => {
  it("leaves an unsigned build without an updater", () => {
    expect(updaterConfig({})).toEqual({});
  });

  it("signs the artefacts when the private key is there", () => {
    expect(updaterConfig({ TAURI_SIGNING_PRIVATE_KEY: "k" })).toEqual({
      bundle: { createUpdaterArtifacts: true },
    });
  });

  it("enables the updater with its public key", () => {
    expect(
      updaterConfig({
        TAURI_SIGNING_PRIVATE_KEY: "k",
        WNK_UPDATER_ENDPOINT: endpoint,
        WNK_UPDATER_PUBKEY: "pub",
      }),
    ).toEqual({
      bundle: { createUpdaterArtifacts: true },
      plugins: { updater: { pubkey: "pub", endpoints: [endpoint] } },
    });
  });

  it("refuses an endpoint without a public key", () => {
    expect(() => updaterConfig({ WNK_UPDATER_ENDPOINT: endpoint })).toThrow(
      /WNK_UPDATER_PUBKEY/,
    );
    expect(() =>
      updaterConfig({
        WNK_UPDATER_ENDPOINT: endpoint,
        WNK_UPDATER_PUBKEY: " ",
      }),
    ).toThrow(/WNK_UPDATER_PUBKEY/);
  });

  it("refuses a public key without an endpoint, and a plain-http endpoint", () => {
    expect(() => updaterConfig({ WNK_UPDATER_PUBKEY: "pub" })).toThrow(
      /WNK_UPDATER_ENDPOINT/,
    );
    expect(() =>
      updaterConfig({
        WNK_UPDATER_ENDPOINT: "http://releases.example/latest.json",
        WNK_UPDATER_PUBKEY: "pub",
      }),
    ).toThrow(/https/);
  });
});
