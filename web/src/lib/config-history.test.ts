import { describe, expect, it } from "vitest";
import type { ConfigSettingChange } from "@/api/config";
import type { SettingField, SettingGroup } from "./config-model";
import { describeChange, describeRevert, labelFor, settingLabels } from "./config-history";

function field(fullPath: string, label: string): SettingField {
  const path = fullPath.split(".").slice(1).join(".");
  return {
    path,
    fullPath,
    key: path.split(".").pop() ?? path,
    label,
    kind: "number",
    hasDefault: true,
    required: false,
    readOnly: false,
    editable: true,
    bootstrap: false,
  } as SettingField;
}

const model: SettingGroup = {
  path: "",
  label: "Rate limits",
  fields: [field("rate_limits.enabled", "Enabled")],
  groups: [
    {
      path: "login",
      label: "Login",
      fields: [field("rate_limits.login.per_second", "Per second")],
      groups: [],
    },
  ],
};

function row(overrides: Partial<ConfigSettingChange>): ConfigSettingChange {
  return {
    pointer: "/rate_limits/login/per_second",
    path: "rate_limits.login.per_second",
    secret: false,
    from: { set: true, value: 5 },
    to: { set: true, value: 10 },
    ...overrides,
  };
}

describe("setting labels", () => {
  it("name a setting by the groups it sits in and its own label, not the section", () => {
    const labels = settingLabels(model);
    expect(labels.get("rate_limits.login.per_second")).toBe("Login · Per second");
    expect(labels.get("rate_limits.enabled")).toBe("Enabled");
  });

  it("fall back to the path made readable for a setting the form does not show", () => {
    expect(labelFor(new Map(), "rate_limits.joins_remote.burst_count")).toBe(
      "Joins remote · Burst count",
    );
  });
});

describe("describing a change", () => {
  it("reads before and after", () => {
    expect(describeChange(row({}))).toEqual({ from: "5", to: "10" });
  });

  it("says a setting the database did not hold came from the file or the default", () => {
    expect(describeChange(row({ from: { set: false } }))).toEqual({
      from: "file or default",
      to: "10",
    });
    expect(describeChange(row({ to: { set: false } })).to).toBe("file or default");
  });

  it("says when the earlier value was never recorded", () => {
    expect(describeChange(row({ from: null })).from).toBe("not recorded");
  });

  it("never shows a secret, only that it was set", () => {
    const secret = row({
      secret: true,
      from: { set: true, value: { $secret: true } },
      to: { set: true, value: { $secret: true } },
    });
    expect(describeChange(secret)).toEqual({ from: "set, hidden", to: "set, hidden" });
    expect(describeRevert(secret)).toEqual({
      from: "set, hidden",
      to: "its earlier value, hidden",
    });
  });

  it("describes a revert as the change run backwards", () => {
    expect(describeRevert(row({}))).toEqual({ from: "10", to: "5" });
    expect(describeRevert(row({ from: { set: false } }))).toEqual({
      from: "10",
      to: "file or default",
    });
  });
});
