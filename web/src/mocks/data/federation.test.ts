import { describe, expect, it } from "vitest";
import type { components } from "@/api/schema";
import { filterDestinations, sortDestinations } from "./federation";

type Destination = components["schemas"]["Destination"];

const d = (server_name: string, extra: Partial<Destination> = {}): Destination => ({
  server_name,
  last_successful_at: null,
  failing_since: null,
  retry_last_at: null,
  retry_interval_ms: null,
  pending_pdu_count: 0,
  pending_edu_count: 0,
  ...extra,
});

const names = (items: Destination[] | null) => items?.map((i) => i.server_name);

/** The mock orders and filters as `crates/hs-admin/src/router.rs` does. */
describe("mock destinations", () => {
  const items = [
    d("b.example", {
      failing_since: "2026-10-01T00:00:00Z",
      last_successful_at: "2026-09-30T00:00:00Z",
    }),
    d("a.example", { last_successful_at: "2026-10-03T00:00:00Z", pending_pdu_count: 4 }),
    d("c.example", { failing_since: "2026-09-20T00:00:00Z" }),
    d("d.example"),
  ];

  it("puts failing destinations first, then the rest by name, without a sort", () => {
    expect(names(sortDestinations(items, null))).toEqual([
      "b.example",
      "c.example",
      "a.example",
      "d.example",
    ]);
    expect(names(sortDestinations(items, "  "))).toEqual(names(sortDestinations(items, null)));
  });

  it("sorts a timestamp with the absent ones last either way", () => {
    expect(names(sortDestinations(items, "failing_since"))).toEqual([
      "c.example",
      "b.example",
      "a.example",
      "d.example",
    ]);
    expect(names(sortDestinations(items, "-failing_since"))).toEqual([
      "b.example",
      "c.example",
      "a.example",
      "d.example",
    ]);
    expect(names(sortDestinations(items, "-last_successful_at"))).toEqual([
      "a.example",
      "b.example",
      "c.example",
      "d.example",
    ]);
  });

  it("sorts a count and a name both ways", () => {
    expect(names(sortDestinations(items, "-pending_pdu_count"))?.[0]).toBe("a.example");
    expect(names(sortDestinations(items, "pending_pdu_count"))?.[3]).toBe("a.example");
    expect(names(sortDestinations(items, "-server_name"))).toEqual([
      "d.example",
      "c.example",
      "b.example",
      "a.example",
    ]);
  });

  it("refuses a field the server does not sort by", () => {
    expect(sortDestinations(items, "pending")).toBeNull();
    expect(sortDestinations(items, "-mood")).toBeNull();
  });

  it("filters by whether a destination is failing, or not at all", () => {
    expect(names(filterDestinations(items, "true"))).toEqual(["b.example", "c.example"]);
    expect(names(filterDestinations(items, "false"))).toEqual(["a.example", "d.example"]);
    expect(filterDestinations(items, null)).toHaveLength(4);
    expect(filterDestinations(items, "maybe")).toHaveLength(4);
  });
});
