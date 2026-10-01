import { describe, expect, it } from "vitest";
import { normalizeConfigSchema } from "@/api/config-schema";
import { configSchemaDocument, configValues } from "@/mocks/data/config";
import {
  applyDraft,
  buildMergePatch,
  buildSectionModel,
  changeEntries,
  chosenVariant,
  cleanDoc,
  containsSecret,
  deletePath,
  describeSubPath,
  entryNoun,
  entrySummary,
  fieldErrorsFor,
  flattenFields,
  formatValue,
  getPath,
  humanizeKey,
  isChanged,
  itemField,
  mapEntryField,
  normalizeErrorPath,
  nounFromTypeName,
  ownerFieldPath,
  propertyFields,
  setPath,
  summarize,
  switchVariant,
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
    // Hot since 2026-10-01: the auth routes read it through the live configuration.
    expect(schema.settings["auth.enable_registration"]).toEqual({
      origin: "database",
      secret: false,
      reloadable: true,
      bootstrap: false,
      editable: true,
    });
    // A bootstrap setting inside an administered section (decision 0010).
    expect(schema.settings["server.signing_key_path"]).toMatchObject({
      bootstrap: true,
      editable: false,
    });
    expect(schema.settings["listeners.listeners"]).toMatchObject({ bootstrap: true });
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
    // An array of objects is a form per entry, never JSON (decision 0010).
    expect(media.get("thumbnail_sizes")?.kind).toBe("object-list");
    expect(media.get("storage")?.kind).toBe("variant");
    expect(media.get("remote_media_retention")?.kind).toBe("duration");
    expect(fieldsOf("listeners").get("listeners")?.kind).toBe("object-list");
    expect(fieldsOf("auth").get("oidc_providers")?.kind).toBe("object-list");

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

  it("says an object in words rather than as JSON", () => {
    expect(formatValue({ backend: "local", path: "/srv/media" })).toBe(
      "Backend: local · Path: /srv/media",
    );
    expect(formatValue([{ width: 32 }])).toBe("1 entry");
    expect(formatValue([{ width: 32 }, { width: 96 }])).toBe("2 entries");
    expect(formatValue({})).toBe("empty");
  });
});

describe("nested forms", () => {
  const media = () => fieldsOf("media");

  it("names a list's entries after the entry type", () => {
    expect(nounFromTypeName("Listener")).toBe("Listener");
    expect(nounFromTypeName("ThumbnailSize")).toBe("Thumbnail size");
    expect(nounFromTypeName("OidcProviderConfig")).toBe("OIDC provider");
    expect(entryNoun(media().get("thumbnail_sizes")!)).toBe("Thumbnail size");
  });

  it("builds an entry's own fields from the item schema", () => {
    const thumbnails = media().get("thumbnail_sizes")!;
    const entry = itemField(thumbnails, 2, { width: 320, height: 240, method: "scale" });
    expect(entry.label).toBe("Thumbnail size 3");
    expect(entry.fullPath).toBe("media.thumbnail_sizes[2]");
    const fields = propertyFields(entry, { width: 320, height: 240, method: "scale" });
    expect(fields.map((f) => [f.key, f.kind])).toEqual([
      ["width", "integer"],
      ["height", "integer"],
      ["method", "enum"],
    ]);
    expect(fields[2].options?.map((o) => o.label)).toEqual(["Crop", "Scale"]);
    expect(entrySummary(entry, { width: 320, height: 240, method: "scale" })).toBe(
      "320 · 240 · scale",
    );
  });

  it("switches a tagged variant, keeping what the two variants share", () => {
    const storage = media().get("storage")!;
    expect(chosenVariant(storage, { backend: "local", path: "/srv" })?.label).toBe("Local");
    expect(propertyFields(storage, { backend: "local", path: "/srv" }).map((f) => f.key)).toEqual([
      "path",
    ]);
    const s3 = switchVariant(storage, { backend: "local", path: "/srv" }, "s3");
    expect(s3).toEqual({ backend: "s3", bucket: "" });
    const gcs = switchVariant(storage, { backend: "s3", bucket: "media", region: "eu" }, "gcs");
    expect(gcs).toEqual({ backend: "gcs", bucket: "media" });
    expect(entrySummary(storage, gcs)).toBe("GCS · media");
  });

  it("never carries a redacted secret into another variant", () => {
    const storage = media().get("storage")!;
    const next = switchVariant(
      storage,
      { backend: "s3", bucket: "b", secret_access_key: { $secret: true } },
      "s3",
    );
    expect(next).toEqual({ backend: "s3", bucket: "b" });
  });

  it("finds a hidden secret anywhere inside a value", () => {
    expect(containsSecret([{ a: 1 }, { b: { $secret: true } }])).toBe(true);
    expect(containsSecret([{ a: 1 }])).toBe(false);
  });

  it("edits a map with one field per key", () => {
    const field: SettingField = {
      ...media().get("storage")!,
      kind: "map",
      schema: { type: "object", additionalProperties: { $ref: "#/$defs/Duration" } },
    };
    const entry = mapEntryField(field, "eu-west", "5s");
    expect(entry.kind).toBe("duration");
    expect(entry.label).toBe("eu-west");
  });

  it("classifies a map and an unknown shape from the schema", () => {
    const custom = normalizeConfigSchema({
      schema: {
        type: "object",
        properties: {
          extra: {
            type: "object",
            properties: {
              weights: { type: "object", additionalProperties: { type: "integer" } },
              anything: { type: "object", additionalProperties: true },
              mixed: { oneOf: [{ type: "string" }, { type: "object", properties: {} }] },
            },
          },
        },
      },
    });
    const fields = new Map(
      flattenFields(buildSectionModel(custom, "extra", {})).map((f) => [f.key, f.kind]),
    );
    expect(fields.get("weights")).toBe("map");
    // Neither is a text box: both are read-only with a note.
    expect(fields.get("anything")).toBe("unsupported");
    expect(fields.get("mixed")).toBe("unsupported");
  });
});

describe("errors inside a list", () => {
  it("lands on the list's own row", () => {
    const paths = ["listeners", "password.enabled", "password"];
    expect(ownerFieldPath("listeners.0.port", paths)).toBe("listeners");
    expect(ownerFieldPath("listeners[0].port", paths)).toBe("listeners");
    expect(ownerFieldPath("password.enabled", paths)).toBe("password.enabled");
    expect(ownerFieldPath("elsewhere", paths)).toBeUndefined();
  });

  it("says which part of the setting was meant, in words", () => {
    expect(describeSubPath(".2.width")).toBe("Entry 3, width");
    expect(describeSubPath("[0].idp_id")).toBe("Entry 1, IdP ID");
    expect(describeSubPath(".bucket")).toBe("Bucket");
  });
});
