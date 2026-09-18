/**
 * Carries the wizard's rendered artifacts (`BridgeTypeRenderResult`) from
 * the Review step to the Created page across a route navigation. The admin
 * API only persists `registration`/`registration_yaml` (retrievable later
 * via `GET /appservices/{id}/registration`, `bridges:write` only);
 * `compose_yaml`/`bridge_resource_yaml` are render-time-only previews with
 * nowhere else to live, so this is a one-shot handoff, not a cache.
 */
const KEY = "hs-admin:last-created-bridge-artifacts";

export interface CreatedArtifacts {
  registrationYaml: string;
  composeYaml?: string;
  bridgeResourceYaml?: string;
}

export function stashCreatedArtifacts(id: string, artifacts: CreatedArtifacts): void {
  try {
    sessionStorage.setItem(KEY, JSON.stringify({ id, artifacts }));
  } catch {
    /* storage unavailable: the Created page falls back to re-fetching what it can */
  }
}

export function takeCreatedArtifacts(id: string): CreatedArtifacts | null {
  try {
    const raw = sessionStorage.getItem(KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as { id: string; artifacts: CreatedArtifacts };
    if (parsed.id !== id) return null;
    sessionStorage.removeItem(KEY);
    return parsed.artifacts;
  } catch {
    return null;
  }
}
