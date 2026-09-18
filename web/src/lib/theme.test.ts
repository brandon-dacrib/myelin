import { afterEach, describe, expect, it } from "vitest";
import { applyAccent, applyTheme, DEFAULT_ACCENT, readAccent, readTheme } from "./theme";

afterEach(() => {
  localStorage.clear();
  document.documentElement.removeAttribute("data-theme");
  document.documentElement.removeAttribute("data-accent");
});

describe("theme", () => {
  it("defaults to system when nothing is stored", () => {
    expect(readTheme()).toBe("system");
  });

  it("persists an explicit theme and applies it to the root element", () => {
    applyTheme("dark");
    expect(readTheme()).toBe("dark");
    expect(document.documentElement.getAttribute("data-theme")).toBe("dark");
  });

  it("clears the stored theme and the attribute when set back to system", () => {
    applyTheme("light");
    applyTheme("system");
    expect(readTheme()).toBe("system");
    expect(document.documentElement.hasAttribute("data-theme")).toBe(false);
  });
});

describe("accent", () => {
  it("defaults to indigo", () => {
    expect(readAccent()).toBe(DEFAULT_ACCENT);
  });

  it("rejects an unknown stored value and falls back to the default", () => {
    localStorage.setItem("hs-admin:accent", "not-a-real-accent");
    expect(readAccent()).toBe(DEFAULT_ACCENT);
  });

  it("persists a curated accent choice", () => {
    applyAccent("teal");
    expect(readAccent()).toBe("teal");
    expect(document.documentElement.getAttribute("data-accent")).toBe("teal");
  });
});
