import { defineConfig } from "@hey-api/openapi-ts";

// The TypeScript half of ADR-0009. The input is the COMMITTED specification, never
// a running server: `task recu` exports the document from the utoipa annotations
// and then regenerates this client from it, so the `contract` CI job can prove the
// two never drift.
export default defineConfig({
  input: "../docs/content/docs/api/openapi.json",
  output: {
    path: "src/api/generated",
    // Nothing post-processes this directory on purpose — see the biome ignore for
    // it. Whatever tool touched it last would decide the diff.
    postProcess: [],
  },
  plugins: [
    {
      name: "@hey-api/client-axios",
      // The instance is created and configured in `src/api/client.ts`: bearer
      // token, base URL and the problem+json unwrapping all live there rather
      // than in generated code.
      runtimeConfigPath: "./src/api/runtime.ts",
    },
    {
      name: "@hey-api/sdk",
    },
    {
      name: "@hey-api/typescript",
      enums: "javascript",
    },
  ],
});
