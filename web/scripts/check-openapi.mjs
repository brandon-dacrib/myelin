#!/usr/bin/env node
// Reports which OpenAPI document the client is currently generated from.
// Track 15's document was reconciled against on 2026-09-18 (see
// docs/status/16-management-web-interface.md); this now just confirms it is
// still present and reminds you to re-check after it changes.
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const webRoot = path.resolve(fileURLToPath(import.meta.url), "../..");
const repoRoot = path.resolve(webRoot, "..");

const track15Openapi = path.join(repoRoot, "crates/hs-admin/openapi/openapi.yaml");
const ownDraft = path.join(webRoot, "mocks/openapi.yaml");

console.log(
  `Own draft (fallback): ${path.relative(repoRoot, ownDraft)} (${existsSync(ownDraft) ? "present" : "MISSING"})`,
);
console.log(
  `Track 15 OpenAPI:     ${path.relative(repoRoot, track15Openapi)} (${existsSync(track15Openapi) ? "present — used as the generation source" : "not present — falling back to the own draft"})`,
);

if (existsSync(track15Openapi)) {
  console.log(
    "\nIf it has changed since 2026-09-18: run `npm run generate:client`, diff src/api/schema.d.ts, " +
      "re-check src/api/*.ts and src/mocks/handlers.ts against it, and record what changed in " +
      "docs/status/16-management-web-interface.md.",
  );
} else {
  console.log("\nAction: none yet. Keep developing against web/mocks/openapi.yaml.");
}
