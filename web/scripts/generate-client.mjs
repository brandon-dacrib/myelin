#!/usr/bin/env node
// Generates src/api/schema.d.ts from an OpenAPI document.
//
// Priority order (see docs/decisions/0003-web-stack.md):
//   1. crates/hs-admin/openapi/openapi.yaml, once track 15's status file
//      (docs/status/15-admin-api-and-modules.md) marks it usable.
//   2. web/mocks/openapi.yaml, this track's own draft.
//
// The generated file is committed, so `tsc -b` never depends on this script
// having run first; run `npm run generate:client` after either source changes.
import { existsSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { execFileSync } from "node:child_process";

const webRoot = path.resolve(fileURLToPath(import.meta.url), "../..");
const repoRoot = path.resolve(webRoot, "..");

const track15Openapi = path.join(repoRoot, "crates/hs-admin/openapi/openapi.yaml");
const track15Status = path.join(repoRoot, "docs/status/15-admin-api-and-modules.md");
const ownDraft = path.join(webRoot, "mocks/openapi.yaml");

function track15DocIsUsable() {
  // 15's document appeared and its status file explicitly invited track 16
  // to generate against it ("Track 16: generate your client and mock-check
  // your work against openapi.yaml"), validated (redocly lint, 0 errors)
  // and contract-tested against the real router. Reconciled 2026-09-18; see
  // docs/status/16-management-web-interface.md and docs/decisions/0004.
  return existsSync(track15Openapi);
}

const source = track15DocIsUsable() ? track15Openapi : ownDraft;
const out = path.join(webRoot, "src/api/schema.d.ts");

console.log(`[generate-client] source: ${path.relative(repoRoot, source)}`);
console.log(`[generate-client] out:    ${path.relative(repoRoot, out)}`);

execFileSync("npx", ["openapi-typescript", source, "-o", out, "--alphabetize"], {
  stdio: "inherit",
  cwd: webRoot,
});

console.log("[generate-client] done");
