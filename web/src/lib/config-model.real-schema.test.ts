/**
 * The Configuration page against the real server's configuration schema.
 *
 * `src/test/fixtures/hs-config-schema.json` is `schemars::schema_for!(hs_config::Config)`
 * verbatim — exactly what `GET /api/v1/config/schema` serves as its `schema` member
 * (`crates/hs-admin/src/config_schema.rs`'s `config_json_schema`). Regenerate it after a change
 * to `crates/hs-config` with a running server:
 *
 *     curl -s -H "Authorization: Bearer $TOKEN" http://localhost:8008/api/v1/config/schema \
 *       | jq .schema > web/src/test/fixtures/hs-config-schema.json
 *
 * The point of testing against it, rather than only against the hand-written mock: decision 0010
 * says no setting may fall through to a text box, and the only way to know that of the real
 * server is to walk the real schema. Every setting, and every setting nested inside a list entry,
 * a variant or a map, must get a real control.
 */
import { describe, expect, it } from "vitest";
import realSchema from "@/test/fixtures/hs-config-schema.json";
import { normalizeConfigSchema, resolveRef, type JsonValue } from "@/api/config-schema";
import {
  buildSectionModel,
  choiceInfo,
  choicePayloadField,
  chosenChoice,
  switchChoice,
  emptyValue,
  flattenFields,
  itemField,
  mapEntryField,
  propertyFields,
  switchVariant,
  variantInfo,
  type SettingField,
} from "./config-model";

const schema = normalizeConfigSchema({ schema: realSchema });

function defaultsOf(section: string): JsonValue {
  const node = resolveRef(schema.root.properties![section], schema.defs);
  return (node.default ?? {}) as JsonValue;
}

function sectionFields(section: string): Map<string, SettingField> {
  const model = buildSectionModel(
    schema,
    section,
    defaultsOf(section) as Record<string, JsonValue>,
  );
  return new Map(flattenFields(model).map((f) => [f.path, f]));
}

/** Every field nested anywhere inside `field`'s value: entries, variants, map values. */
function nestedFields(field: SettingField, depth = 0): SettingField[] {
  if (depth > 6) return [];
  let children: SettingField[] = [];
  switch (field.kind) {
    case "object":
      children = propertyFields(field, undefined);
      break;
    case "variant":
      children = (variantInfo(field.schema, field.defs ?? {})?.options ?? []).flatMap((option) =>
        propertyFields(field, switchVariant(field, undefined, option.value)),
      );
      break;
    case "choice":
      children = (choiceInfo(field.schema, field.defs ?? {}) ?? [])
        .filter((option) => option.payload)
        .map((option) => choicePayloadField(field, option, undefined));
      break;
    case "object-list":
      children = [itemField(field, 0, undefined)];
      break;
    case "map":
      children = [mapEntryField(field, "key", undefined)];
      break;
  }
  return children.flatMap((child) => [child, ...nestedFields(child, depth + 1)]);
}

const allFields = schema.sections.flatMap((section) => {
  const top = [...sectionFields(section.name).values()];
  return [...top, ...top.flatMap((f) => nestedFields(f))];
});

describe("the real server's configuration schema", () => {
  it("has the ten sections the page lists", () => {
    expect(schema.sections.map((s) => s.name)).toEqual([
      "server",
      "listeners",
      "storage",
      "media",
      "federation",
      "rate_limits",
      "auth",
      "appservices",
      "telemetry",
      "cluster",
    ]);
  });

  it("gives every setting, nested ones included, a real control", () => {
    const unsupported = allFields.filter((f) => f.kind === "unsupported").map((f) => f.fullPath);
    expect(unsupported).toEqual([]);
    // Sanity: the walk reached inside the lists and the variants.
    expect(allFields.map((f) => f.fullPath)).toEqual(
      expect.arrayContaining([
        "listeners.listeners[0].port",
        "listeners.listeners[0].tls.certificate_path",
        "media.storage.bucket",
        "media.thumbnail_sizes[0].method",
        "auth.oidc_providers[0].client_secret",
      ]),
    );
  });

  it("edits what used to be JSON with structured controls", () => {
    expect(sectionFields("listeners").get("listeners")?.kind).toBe("object-list");
    expect(sectionFields("media").get("thumbnail_sizes")?.kind).toBe("object-list");
    expect(sectionFields("media").get("storage")?.kind).toBe("variant");
    expect(sectionFields("auth").get("oidc_providers")?.kind).toBe("object-list");
    // A documented Rust enum reaches the schema as a oneOf of consts, not an `enum` array.
    const level = sectionFields("telemetry").get("logging.level");
    expect(level?.kind).toBe("enum");
    expect(level?.options?.map((o) => o.value)).toEqual([
      "trace",
      "debug",
      "info",
      "warn",
      "error",
    ]);
    expect(level?.options?.[0].description).toBe(
      "Everything, including per-request tracing detail.",
    );
  });

  it("keeps an optional duration a duration", () => {
    // `Option<Duration>` is `anyOf: [{$ref: Duration}, null]`; only `x-duration` survives it.
    expect(sectionFields("auth").get("refresh_token_lifetime")?.kind).toBe("duration");
    expect(sectionFields("media").get("remote_media_retention")?.kind).toBe("duration");
  });

  it("describes a listener entry field by field", () => {
    const listeners = sectionFields("listeners").get("listeners")!;
    const entry = itemField(listeners, 0, undefined);
    expect(entry.label).toBe("Listener 1");
    const fields = new Map(propertyFields(entry, undefined).map((f) => [f.key, f]));
    expect(fields.get("bind_addresses")?.kind).toBe("string-list");
    expect(fields.get("port")?.kind).toBe("integer");
    expect(fields.get("port")?.required).toBe(true);
    expect(fields.get("resources")?.kind).toBe("enum-list");
    expect(fields.get("resources")?.options?.map((o) => o.value)).toEqual([
      "client",
      "federation",
      "media",
      "metrics",
      "admin",
      "health",
    ]);
    expect(fields.get("tls")?.kind).toBe("object");
    expect(fields.get("tls")?.nullable).toBe(true);
    expect(fields.get("x_forwarded")?.kind).toBe("boolean");
  });

  it("starts a new entry from the schema's defaults, with the required rest left empty", () => {
    const listeners = sectionFields("listeners").get("listeners")!;
    expect(emptyValue(itemField(listeners, 1, undefined))).toEqual({
      bind_addresses: ["::"],
      port: "",
      resources: [],
      x_forwarded: false,
    });

    const thumbnails = sectionFields("media").get("thumbnail_sizes")!;
    expect(emptyValue(itemField(thumbnails, 5, undefined))).toEqual({
      width: "",
      height: "",
      method: "crop",
    });

    const oidc = sectionFields("auth").get("oidc_providers")!;
    expect(emptyValue(itemField(oidc, 0, undefined))).toEqual({
      idp_id: "",
      issuer: "",
      client_id: "",
      scopes: ["openid", "profile"],
    });
  });

  it("finds a secret inside a list entry and inside a variant", () => {
    const oidc = sectionFields("auth").get("oidc_providers")!;
    const secret = propertyFields(itemField(oidc, 0, undefined), undefined).find(
      (f) => f.key === "client_secret",
    );
    expect(secret?.kind).toBe("secret");

    const storage = sectionFields("media").get("storage")!;
    const s3 = switchVariant(storage, undefined, "s3");
    expect(s3).toEqual({ backend: "s3", bucket: "" });
    expect(propertyFields(storage, s3).find((f) => f.key === "secret_access_key")?.kind).toBe(
      "secret",
    );
  });

  it("edits the ICAP preview mode, an externally tagged enum, as a choice", () => {
    const preview = sectionFields("media").get("scanning.icap.preview")!;
    expect(preview.fullPath).toBe("media.scanning.icap.preview");
    expect(preview.kind).toBe("choice");
    expect(choiceInfo(preview.schema, preview.defs ?? {})?.map((o) => o.value)).toEqual([
      "negotiate",
      "bytes",
      "off",
    ]);
    expect(chosenChoice(preview, "negotiate")?.label).toBe("Negotiate");
    expect(chosenChoice(preview, { bytes: 4096 })?.value).toBe("bytes");
    expect(switchChoice(preview, "negotiate", "off")).toBe("off");
    expect(switchChoice(preview, "off", "bytes")).toEqual({ bytes: "" });
    expect(switchChoice(preview, { bytes: 4096 }, "bytes")).toEqual({ bytes: 4096 });
    const bytes = chosenChoice(preview, { bytes: 4096 })!;
    expect(choicePayloadField(preview, bytes, { bytes: 4096 }).kind).toBe("integer");
  });
});
