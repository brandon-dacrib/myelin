import { describe, expect, it } from "vitest";
import { normalizeConfigSchema } from "@/api/config-schema";
import { configSchemaDocument, configValues } from "@/mocks/data/config";
import {
  applyDraft,
  buildMergePatch,
  buildSectionModel,
  changeEntries,
  cleanDoc,
  deletePath,
  fieldErrorsFor,
  flattenFields,
  formatValue,
  getPath,
  humanizeKey,
  isChanged,
  normalizeErrorPath,
  setPath,
  summarize,
  type SettingField,
} from "./config-model";

const schema = normalizeConfigSchema(configSchemaDocument);

function fieldsOf(section: string): Map<string, SettingField> {
  const model = buildSectionModel(schema, section, configValues[section]);
  return new Map(flattenFields(model).map((f) => [f.path, f]));
}

describe("normalizeConfigSchema", () => {
  it("keeps the per-section reloadable and bootstrap flags", () => {
    expect(schema.sections.find((s) => s.name === "rate_limits")).toMatchObject({
      name: "rate_limits",
      reloadable: true,
      bootstrap: false,
    });
    expect(schema.sections.find((s) => s.name === "storage")).toMatchObject({
      name: "storage",
      reloadable: false,
      bootstrap: true,
    });
  });

  it("reads the shipped settings[] rows, including the server's own editable answer", () => {
    // `ConfigSettingInfo` in crates/hs-admin/openapi/openapi.yaml.
    expect(schema.settings["auth.enable_registration"]).toEqual({
      origin: "database",
      secret: false,
      reloadable: false,
      editable: true,
    });
    // Pinned by an HS__ variable: the server says it will not take a change.
    expect(schema.settings["server.server_name"]).toMatchObject({
      origin: "environment",
      editable: false,
    });
    // Bootstrap section: same answer, different reason.
    expect(schema.settings["storage.data_dir"]).toMatchObject({
      origin: "file",
      editable: false,
    });
    expect(schema.settings["auth.session_secret"]).toMatchObject({ secret: true });
  });

  it("still accepts the older origins map, inferring editable from the origin", () => {
    const older = normalizeConfigSchema({
      schema: configSchemaDocument.schema,
      sections: configSchemaDocument.sections,
      origins: {
        "/auth/enable_registration": "database",
        "/server/server_name": "environment",
      },
    });
    expect(older.settings["auth.enable_registration"].editable).toBe(true);
    expect(older.settings["server.server_name"].editable).toBe(false);
    expect(older.settings["storage.data_dir"]).toBeUndefined();
  });

  it("rewrites JSON Pointer origin keys to the dotted paths the form uses", () => {
    expect(schema.origins["auth.enable_registration"]).toBe("database");
    expect(schema.origins["server.server_name"]).toBe("environment");
    expect(schema.origins["rate_limits.message.per_second"]).toBe("database");
  });

  it("falls back to the known section flags when the API sends a bare schema document", () => {
    const bare = normalizeConfigSchema(configSchemaDocument.schema);
    expect(bare.sections.find((s) => s.name === "federation")?.reloadable).toBe(true);
    expect(bare.sections.find((s) => s.name === "storage")?.bootstrap).toBe(true);
    expect(bare.origins).toEqual({});
  });

  it("accepts origins keyed by dotted path or nested, not only by pointer", () => {
    const dotted = normalizeConfigSchema({
      schema: configSchemaDocument.schema,
      origins: { "auth.enable_registration": "file" },
    });
    expect(dotted.origins["auth.enable_registration"]).toBe("file");

    const nested = normalizeConfigSchema({
      schema: configSchemaDocument.schema,
      origins: { auth: { enable_registration: "environment" } },
    });
    expect(nested.origins["auth.enable_registration"]).toBe("environment");
  });
});

describe("buildSectionModel", () => {
  it("picks a control for each kind of setting from the schema alone", () => {
    const federation = fieldsOf("federation");
    expect(federation.get("enabled")?.kind).toBe("boolean");
    expect(federation.get("client_timeout")?.kind).toBe("duration");
    expect(federation.get("custom_ca_certificates")?.kind).toBe("string-list");

    const media = fieldsOf("media");
    expect(media.get("max_upload_size")?.kind).toBe("bytes");
    // An array of objects has no better control than JSON.
    expect(media.get("thumbnail_sizes")?.kind).toBe("json");

    const telemetry = fieldsOf("telemetry");
    expect(telemetry.get("logging.level")?.kind).toBe("enum");
    expect(telemetry.get("logging.level")?.options?.map((o) => o.value)).toEqual([
      "trace",
      "debug",
      "info",
      "warn",
      "error",
    ]);

    const cluster = fieldsOf("cluster");
    expect(cluster.get("room_shards")?.kind).toBe("integer");
    expect(cluster.get("mesh.tls_enabled")?.kind).toBe("boolean");
  });

  it("renders a redacted value as a secret even where the schema does not say so", () => {
    const auth = fieldsOf("auth");
    expect(auth.get("registration_shared_secret")?.kind).toBe("secret");
    // `password.pepper` has no value set at all; the schema's own marker and
    // the settings row's `secret` flag both still classify it.
    expect(auth.get("password.pepper")?.kind).toBe("secret");
  });

  it("carries the server's editable answer onto the field", () => {
    expect(fieldsOf("server").get("server_name")?.editable).toBe(false);
    expect(fieldsOf("server").get("report_stats")?.editable).toBe(true);
    expect(fieldsOf("storage").get("data_dir")?.editable).toBe(false);
  });

  it("unwraps Option<T> so an optional duration is still a duration", () => {
    expect(fieldsOf("auth").get("refresh_token_lifetime")?.kind).toBe("duration");
    expect(fieldsOf("server").get("public_baseurl")?.kind).toBe("string");
  });

  it("nests an object-valued setting as its own group", () => {
    const model = buildSectionModel(schema, "auth", configValues.auth);
    const password = model.groups.find((g) => g.path === "password");
    expect(password).toBeDefined();
    expect(password?.groups.map((g) => g.path)).toContain("password.policy");
    expect(flattenFields(model).map((f) => f.path)).toContain("password.policy.minimum_length");
  });

  it("renders the live variant of an internally tagged enum", () => {
    // `storage` is a Rust enum tagged by `backend`; the running value is
    // `embedded`, so the form is the embedded variant, not a union of three.
    const storage = fieldsOf("storage");
    expect([...storage.keys()]).toEqual(["backend", "data_dir"]);
    expect(storage.get("backend")?.readOnly).toBe(true);
  });

  it("carries the schema default and the full path", () => {
    const field = fieldsOf("appservices").get("tracking_failure_threshold");
    expect(field?.defaultValue).toBe(50);
    expect(field?.hasDefault).toBe(true);
    expect(field?.fullPath).toBe("appservices.tracking_failure_threshold");
  });

  it("returns an empty model for a section this build has never heard of", () => {
    const model = buildSectionModel(schema, "quantum_entanglement", {});
    expect(model.label).toBe("Quantum entanglement");
    expect(flattenFields(model)).toEqual([]);
  });
});

describe("paths", () => {
  it("reads, writes and removes a dotted path", () => {
    const doc = { a: { b: { c: 1 } } };
    expect(getPath(doc, "a.b.c")).toBe(1);
    expect(getPath(doc, "a.x.c")).toBeUndefined();
    expect(setPath(doc, "a.b.d", 2)).toEqual({ a: { b: { c: 1, d: 2 } } });
    expect(deletePath(doc, "a.b.c")).toEqual({ a: { b: {} } });
    // The original is untouched.
    expect(doc).toEqual({ a: { b: { c: 1 } } });
  });

  it("leaves a document alone when removing a path it does not have", () => {
    expect(deletePath({ a: 1 }, "b.c")).toEqual({ a: 1 });
  });
});

describe("buildMergePatch", () => {
  it("nests edits and keeps null as RFC 7396's removal", () => {
    expect(
      buildMergePatch({
        "message.per_second": 0.5,
        "message.burst_count": 25,
        "login.burst_count": null,
        enabled: true,
      }),
    ).toEqual({
      message: { per_second: 0.5, burst_count: 25 },
      login: { burst_count: null },
      enabled: true,
    });
  });

  it("is empty for an empty draft", () => {
    expect(buildMergePatch({})).toEqual({});
  });
});

describe("applyDraft", () => {
  it("previews the document the server will end up with", () => {
    const values = { a: 1, nested: { keep: true, drop: "x" } };
    expect(applyDraft(values, { a: 2, "nested.drop": null, "nested.added": "y" })).toEqual({
      a: 2,
      nested: { keep: true, added: "y" },
    });
  });
});

describe("changeEntries", () => {
  const fields = [...fieldsOf("federation").values()];
  const values = configValues.federation;

  it("ignores an edit that sets a setting to what it already is", () => {
    expect(changeEntries(fields, values, { enabled: true })).toEqual([]);
  });

  it("reports a real edit with what it was and what it becomes", () => {
    const [change] = changeEntries(fields, values, { client_timeout: "90s" });
    expect(change.path).toBe("client_timeout");
    expect(change.from).toBe("45s");
    expect(change.to).toBe("90s");
    expect(change.isReset).toBe(false);
  });

  it("reports a reset, and the default it reverts to", () => {
    const [change] = changeEntries(fields, values, { custom_ca_certificates: null });
    expect(change.isReset).toBe(true);
    expect(change.defaultValue).toEqual([]);
  });

  it("ignores a reset of something already at its default", () => {
    expect(changeEntries(fields, values, { ip_range_allowlist: null })).toEqual([]);
  });
});

describe("isChanged", () => {
  const field = fieldsOf("federation").get("verify_certificates")!;

  it("believes the server's origin over the schema default", () => {
    // The effective value equals the default, but the database set it: the
    // operator did choose this, and the page should say so.
    expect(isChanged(field, true, "database")).toBe(true);
    expect(isChanged(field, true, "default")).toBe(false);
  });

  it("falls back to comparing against the default when there is no origin", () => {
    expect(isChanged(field, false, undefined)).toBe(true);
    expect(isChanged(field, true, undefined)).toBe(false);
  });

  it("does not call an unset Option<T> changed", () => {
    // `domain_allowlist` has no schema default and nothing sets it; the API
    // reports the effective value as an explicit null, which is not a choice
    // anyone made.
    const optional = fieldsOf("federation").get("domain_allowlist")!;
    expect(optional.hasDefault).toBe(false);
    expect(isChanged(optional, null, undefined)).toBe(false);
    expect(isChanged(optional, undefined, undefined)).toBe(false);
    expect(isChanged(optional, ["example.org"], undefined)).toBe(true);
  });
});

describe("normalizeErrorPath", () => {
  it("accepts every spelling the two halves of the system use", () => {
    // A JSON Pointer into the PATCH body, which is the section document.
    expect(normalizeErrorPath("/login/per_second", "rate_limits")).toBe("login.per_second");
    // A JSON Pointer into the whole config, from POST /config/validate.
    expect(normalizeErrorPath("/rate_limits/login/per_second", "rate_limits")).toBe(
      "login.per_second",
    );
    // hs-config's own dotted path.
    expect(normalizeErrorPath("rate_limits.login.per_second", "rate_limits")).toBe(
      "login.per_second",
    );
    expect(normalizeErrorPath("login.per_second", "rate_limits")).toBe("login.per_second");
    // An error about the section as a whole.
    expect(normalizeErrorPath("/rate_limits", "rate_limits")).toBe("");
  });

  it("unescapes RFC 6901 tokens", () => {
    expect(normalizeErrorPath("/a~1b/c", "section")).toBe("a/b.c");
  });

  it("groups a problem's errors by the field they land on", () => {
    expect(
      fieldErrorsFor(
        [
          { pointer: "/login/per_second", detail: "must be greater than zero" },
          { pointer: "/message/burst_count", detail: "must be at least 1" },
        ],
        "rate_limits",
      ),
    ).toEqual([
      { path: "login.per_second", detail: "must be greater than zero" },
      { path: "message.burst_count", detail: "must be at least 1" },
    ]);
  });
});

describe("labels and documentation", () => {
  it("humanises a snake_case key, keeping acronyms", () => {
    expect(humanizeKey("enable_registration")).toBe("Enable registration");
    expect(humanizeKey("ip_range_blocklist")).toBe("IP range blocklist");
    expect(humanizeKey("trust_os_root_store")).toBe("Trust OS root store");
  });

  it("strips rustdoc link syntax out of a doc comment", () => {
    expect(cleanDoc("See [`crate::reload`] and `Config`.")).toBe("See crate::reload and Config.");
  });

  it("summarises a long doc comment at a sentence boundary", () => {
    const long = `Short first sentence. ${"and then a great deal more prose ".repeat(20)}`;
    expect(summarize(long)).toBe("Short first sentence.");
  });
});

describe("formatValue", () => {
  it("says what a value is in words, and never reveals a secret", () => {
    expect(formatValue(undefined)).toBe("not set");
    expect(formatValue(null)).toBe("none");
    expect(formatValue(true)).toBe("on");
    expect(formatValue([])).toBe("empty list");
    expect(formatValue(["a", "b"])).toBe("a, b");
    expect(formatValue({ $secret: true })).toBe("set, hidden");
  });
});
