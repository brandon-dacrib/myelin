import { useState } from "react";
import { describe, expect, it } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { normalizeConfigSchema, type JsonValue } from "@/api/config-schema";
import { configSchemaDocument } from "@/mocks/data/config";
import { buildSectionModel, flattenFields, type SettingField } from "@/lib/config-model";
import { SettingControl } from "./SettingControls";
import { ReadOnlyValue } from "./StructuredControls";

const schema = normalizeConfigSchema(configSchemaDocument);

function fieldOf(section: string, path: string): SettingField {
  const field = flattenFields(buildSectionModel(schema, section, {})).find((f) => f.path === path);
  if (!field) throw new Error(`no field ${section}.${path}`);
  return field;
}

/**
 * The section page's draft, reduced to one setting: holds whatever the control reports and feeds
 * it back, and records every value so a test can assert what would be saved.
 */
function Harness({
  field,
  initial,
  seen,
}: {
  field: SettingField;
  initial: JsonValue | undefined;
  seen: (JsonValue | null)[];
}) {
  const [value, setValue] = useState<JsonValue | undefined>(initial);
  return (
    <div>
      <p id="setting-label">{field.label}</p>
      <SettingControl
        field={field}
        value={value}
        id="setting-control"
        labelledBy="setting-label"
        onChange={(next) => {
          seen.push(next);
          setValue(next === null ? undefined : next);
        }}
        onRevert={() => undefined}
      />
    </div>
  );
}

/** Opens a select from the keyboard, the way jsdom can, and picks the named option. */
async function choose(
  user: ReturnType<typeof userEvent.setup>,
  trigger: HTMLElement,
  name: string,
) {
  trigger.focus();
  await user.keyboard("{Enter}");
  await user.click(await screen.findByRole("option", { name }));
}

function renderControl(field: SettingField, initial: JsonValue | undefined) {
  const seen: (JsonValue | null)[] = [];
  const utils = render(<Harness field={field} initial={initial} seen={seen} />);
  return { ...utils, seen, last: () => seen[seen.length - 1] };
}

const THUMBNAILS: JsonValue = [
  { width: 32, height: 32, method: "crop" },
  { width: 96, height: 96, method: "crop" },
  { width: 320, height: 240, method: "scale" },
];

describe("a list of objects", () => {
  it("is a form per entry, labelled by the setting, and never a text box", () => {
    const { container } = renderControl(fieldOf("media", "thumbnail_sizes"), THUMBNAILS);

    expect(screen.getByRole("group", { name: "Thumbnail sizes" })).toBeInTheDocument();
    const entry = screen.getByRole("group", { name: /^Thumbnail size 2/ });
    expect(within(entry).getByLabelText(/^Width/)).toHaveValue("96");
    expect(within(entry).getByLabelText(/^Height/)).toHaveValue("96");
    expect(within(entry).getByLabelText(/^Method/)).toHaveTextContent("Crop");
    expect(container.querySelector("textarea")).toBeNull();
  });

  it("edits one field of one entry and reports the whole list", async () => {
    const user = userEvent.setup();
    const { last } = renderControl(fieldOf("media", "thumbnail_sizes"), THUMBNAILS);

    const width = within(screen.getByRole("group", { name: /^Thumbnail size 3/ })).getByLabelText(
      /^Width/,
    );
    await user.clear(width);
    await user.type(width, "640");
    await user.tab();

    expect(last()).toEqual([
      { width: 32, height: 32, method: "crop" },
      { width: 96, height: 96, method: "crop" },
      { width: 640, height: 240, method: "scale" },
    ]);
  });

  it("chooses an entry's enum value from the schema's options", async () => {
    const user = userEvent.setup();
    const { last } = renderControl(fieldOf("media", "thumbnail_sizes"), THUMBNAILS);

    const entry = within(screen.getByRole("group", { name: /^Thumbnail size 1/ }));
    await choose(user, entry.getByLabelText(/^Method/), "Scale");

    expect((last() as JsonValue[])[0]).toEqual({ width: 32, height: 32, method: "scale" });
  });

  it("adds an entry from the schema's defaults and puts focus in it", async () => {
    const user = userEvent.setup();
    const { last } = renderControl(fieldOf("media", "thumbnail_sizes"), THUMBNAILS);

    await user.click(screen.getByRole("button", { name: "Add thumbnail size" }));

    expect((last() as JsonValue[])[3]).toEqual({ width: "", height: "", method: "crop" });
    const added = within(screen.getByRole("group", { name: /^Thumbnail size 4/ }));
    // The first control in the new entry is its first field.
    expect(added.getByLabelText(/^Width/)).toHaveFocus();
  });

  it("moves an entry up and down, keeping focus on the entry being moved", async () => {
    const user = userEvent.setup();
    const { last } = renderControl(fieldOf("media", "thumbnail_sizes"), THUMBNAILS);

    await user.click(screen.getByRole("button", { name: "Move Thumbnail size 3 up" }));
    expect((last() as { width: number }[]).map((e) => e.width)).toEqual([32, 320, 96]);
    // It is now entry 2, and its "up" button is still the one focused.
    expect(screen.getByRole("button", { name: "Move Thumbnail size 2 up" })).toHaveFocus();

    // Pressed again it reaches the top, where "up" is disabled: focus moves to "down".
    await user.keyboard("{Enter}");
    expect((last() as { width: number }[]).map((e) => e.width)).toEqual([320, 32, 96]);
    expect(screen.getByRole("button", { name: "Move Thumbnail size 1 up" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Move Thumbnail size 1 down" })).toHaveFocus();
  });

  it("removes an entry, and focus stays in the list", async () => {
    const user = userEvent.setup();
    const { last } = renderControl(fieldOf("media", "thumbnail_sizes"), [
      { width: 32, height: 32, method: "crop" },
    ]);

    await user.click(screen.getByRole("button", { name: "Remove Thumbnail size 1" }));

    expect(last()).toEqual([]);
    expect(screen.getByText("Empty list.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Add thumbnail size" })).toHaveFocus();
  });

  it("warns that a hidden secret inside a list does not survive saving it", () => {
    renderControl(fieldOf("auth", "oidc_providers"), [
      {
        idp_id: "google",
        issuer: "https://accounts.google.com/",
        client_id: "c",
        client_secret: { $secret: true },
      },
    ]);
    expect(screen.getByRole("note")).toHaveTextContent(/saving a change to this list clears them/);
  });

  it("replaces a nested secret without ever showing it, and cancelling puts the marker back", async () => {
    const user = userEvent.setup();
    const { last } = renderControl(fieldOf("auth", "oidc_providers"), [
      { idp_id: "google", issuer: "https://a/", client_id: "c", client_secret: { $secret: true } },
    ]);

    const entry = within(screen.getByRole("group", { name: /^OIDC provider 1/ }));
    expect(entry.getByText("Set, hidden")).toBeInTheDocument();
    await user.click(entry.getByRole("button", { name: "Replace Client secret" }));
    expect((last() as Record<string, JsonValue>[])[0].client_secret).toBe("");
    await user.type(entry.getByLabelText("New value for Client secret"), "s3cret");
    expect((last() as Record<string, JsonValue>[])[0].client_secret).toBe("s3cret");

    await user.click(entry.getByRole("button", { name: "Cancel" }));
    expect((last() as Record<string, JsonValue>[])[0].client_secret).toEqual({ $secret: true });
  });
});

describe("a listener", () => {
  const LISTENER: JsonValue = [
    {
      bind_addresses: ["::"],
      port: 8008,
      tls: null,
      resources: ["client", "federation"],
      x_forwarded: false,
    },
  ];

  it("picks resources from checkboxes, one per value the server knows", async () => {
    const user = userEvent.setup();
    const { last } = renderControl(fieldOf("listeners", "listeners"), LISTENER);

    const resources = screen.getByRole("group", { name: /^Resources/ });
    expect(within(resources).getByRole("checkbox", { name: "Client" })).toBeChecked();
    expect(within(resources).getByRole("checkbox", { name: "Admin" })).not.toBeChecked();
    // Each option carries its own doc comment as its description.
    expect(
      within(resources).getByRole("checkbox", { name: "Metrics" }),
    ).toHaveAccessibleDescription("Prometheus text exposition.");

    await user.click(within(resources).getByRole("checkbox", { name: "Admin" }));
    await user.click(within(resources).getByRole("checkbox", { name: "Client" }));
    expect((last() as Record<string, JsonValue>[])[0].resources).toEqual(["federation", "admin"]);
  });

  it("sets up an optional nested object, and removes it again", async () => {
    const user = userEvent.setup();
    const { last } = renderControl(fieldOf("listeners", "listeners"), LISTENER);

    await user.click(screen.getByRole("button", { name: "Set up TLS" }));
    expect((last() as Record<string, JsonValue>[])[0].tls).toEqual({
      certificate_path: "",
      private_key_path: "",
    });

    const tls = within(screen.getByRole("group", { name: /^TLS/ }));
    await user.type(tls.getByLabelText(/^Certificate path/), "/etc/tls/cert.pem");
    expect((last() as Record<string, Record<string, JsonValue>>[])[0].tls.certificate_path).toBe(
      "/etc/tls/cert.pem",
    );

    await user.click(tls.getByRole("button", { name: "Remove TLS" }));
    expect((last() as Record<string, JsonValue>[])[0]).not.toHaveProperty("tls");
  });

  it("starts a new listener from the defaults, with its required port empty", async () => {
    const user = userEvent.setup();
    const { last } = renderControl(fieldOf("listeners", "listeners"), LISTENER);

    await user.click(screen.getByRole("button", { name: "Add listener" }));
    expect((last() as JsonValue[])[1]).toEqual({
      bind_addresses: ["::"],
      port: "",
      resources: [],
      x_forwarded: false,
    });
    const added = within(screen.getByRole("group", { name: /^Listener 2/ }));
    // Required and without a default: marked, and empty until someone fills it in.
    const port = added.getByLabelText(/^Port/);
    expect(port).toHaveValue("");
    expect(document.querySelector(`label[for="${port.id}"]`)).toHaveTextContent("Port *");
  });
});

describe("a tagged variant", () => {
  it("picks the variant, then shows that variant's own settings", async () => {
    const user = userEvent.setup();
    const { last } = renderControl(fieldOf("media", "storage"), {
      backend: "local",
      path: "/var/lib/myelin/media",
    });

    expect(screen.getByLabelText(/^Path/)).toHaveValue("/var/lib/myelin/media");
    expect(screen.getByText(/^Local filesystem/)).toBeInTheDocument();

    await choose(user, screen.getByLabelText("Backend"), "S3");

    expect(last()).toEqual({ backend: "s3", bucket: "" });
    expect(screen.queryByLabelText(/^Path/)).not.toBeInTheDocument();
    await user.type(screen.getByLabelText(/^Bucket/), "media");
    expect(last()).toEqual({ backend: "s3", bucket: "media" });
    // An optional setting of the variant is there to fill in, and its secret is a secret.
    expect(screen.getByLabelText("Region")).toHaveValue("");
    expect(screen.getByRole("button", { name: "Set Secret access key" })).toBeInTheDocument();
  });
});

describe("a map", () => {
  const field: SettingField = {
    ...fieldOf("federation", "client_timeout"),
    path: "per_destination",
    fullPath: "federation.per_destination",
    key: "per_destination",
    label: "Per destination",
    kind: "map",
    schema: { type: "object", additionalProperties: { $ref: "#/$defs/Duration" } },
  };

  it("adds, edits and removes keyed entries, refusing a duplicate key", async () => {
    const user = userEvent.setup();
    const { last } = renderControl(field, { "matrix.org": "30s" });

    expect(screen.getByLabelText("matrix.org")).toHaveValue("30s");

    const newKey = screen.getByLabelText("New key for Per destination");
    await user.type(newKey, "matrix.org");
    expect(screen.getByRole("alert")).toHaveTextContent("matrix.org is already in the list.");
    expect(screen.getByRole("button", { name: "Add entry" })).toBeDisabled();

    await user.clear(newKey);
    await user.type(newKey, "example.com{Enter}");
    expect(last()).toEqual({ "matrix.org": "30s", "example.com": "" });

    await user.type(screen.getByLabelText("example.com"), "5s");
    expect(last()).toEqual({ "matrix.org": "30s", "example.com": "5s" });

    await user.click(screen.getByRole("button", { name: "Remove matrix.org" }));
    expect(last()).toEqual({ "example.com": "5s" });
  });
});

describe("a shape the interface does not recognise", () => {
  it("is shown read-only with a note, never as text to edit", () => {
    const field: SettingField = {
      ...fieldOf("federation", "client_timeout"),
      label: "Something new",
      kind: "unsupported",
      schema: { type: "object", additionalProperties: true },
    };
    const { container } = renderControl(field, { a: 1, b: ["x", "y"] });

    expect(screen.getByRole("note")).toHaveTextContent(
      "This interface cannot edit something new yet",
    );
    expect(screen.getByText("A")).toBeInTheDocument();
    expect(screen.getByText("x, y")).toBeInTheDocument();
    expect(container.querySelector("input, textarea")).toBeNull();
  });
});

describe("ReadOnlyValue", () => {
  it("renders nested values as lists and name/value pairs, and hides secrets", () => {
    render(
      <ReadOnlyValue
        value={[
          { idp_id: "google", client_secret: { $secret: true } },
          { idp_id: "okta", scopes: ["openid"] },
        ]}
      />,
    );
    expect(screen.getAllByRole("listitem")).toHaveLength(2);
    expect(screen.getByText("google")).toBeInTheDocument();
    expect(screen.getByText("set, hidden")).toBeInTheDocument();
    expect(screen.getAllByText("IdP ID")).toHaveLength(2);
  });
});
