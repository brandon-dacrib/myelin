import { describe, expect, it } from "vitest";
// The server's operation table, generated alongside crates/hs-admin/openapi/openapi.yaml; the
// router enforces exactly these scopes (crates/hs-admin/tests/scope_contract.rs).
import operationTable from "../../../../crates/hs-admin/openapi/operations.json";
import { navItems, subNavItems } from "./nav";

interface Operation {
  operation_id: string;
  scope: string | null;
}

const documented = new Map(
  (operationTable.operations as Operation[]).map((op) => [op.operation_id, op.scope]),
);

/**
 * The operation each section lands on: what its page asks for first. A section is shown to
 * exactly the tokens the server would serve that operation to, so it is neither hidden from
 * someone who could use it nor shown to someone who would only get a 403.
 */
const landingOperation: Record<string, string> = {
  bridges: "appservices.list",
  users: "users.list",
  rooms: "rooms.list",
  reports: "reports.list",
  federation: "federation.destinations.list",
  media: "media.list",
  statistics: "statistics.overview",
  cluster: "cluster.get",
  migration: "migration.get",
  tasks: "tasks.list",
  audit: "audit_log.list",
  configuration: "config.list",
  settings: "registration_tokens.list",
  "settings-registration-tokens": "registration_tokens.list",
  "settings-server-notices": "server_notices.list",
};

describe("navigation scopes", () => {
  const gated = [...navItems, ...subNavItems].filter((item) => item.scope);

  it.each(gated.map((item) => [item.id, item.scope]))(
    "%s is gated on the scope the server documents for its landing operation",
    (id, scope) => {
      const operation = landingOperation[id as string];
      expect(operation, `${id} has no landing operation in this test`).toBeDefined();
      expect(documented.has(operation), `${operation} is not in operations.json`).toBe(true);
      expect(scope).toBe(documented.get(operation));
    },
  );
});
