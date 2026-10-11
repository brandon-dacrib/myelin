import { beforeEach, describe, expect, it } from "vitest";
import {
  addressRefusal,
  getInstance,
  instanceFiles,
  instanceUrlFor,
  putInstance,
  resetBridgeOfferings,
} from "./bridge-offerings";

/**
 * The mock's side of OpenAPI 0.1.15's `BridgeInstanceRequest.address` and
 * `BridgeInstanceFiles.run_sh`, as the real server behaves: the url is the placeholder named
 * after the appservice until an address is set, a bare host takes the type's port, and a type
 * that runs on a Mac gets a run.sh beside its two YAML files.
 */
describe("the mock bridge instances' address", () => {
  beforeEach(() => resetBridgeOfferings());

  it("starts as the placeholder named after the appservice, and the files carry it", () => {
    expect(getInstance("mautrix-imessage", "@alice:example.org")?.url).toBe(
      "http://imessage-alice:29331",
    );
    const files = instanceFiles("mautrix-imessage", "@alice:example.org")!;
    expect(files.registration_yaml).toContain("url: http://imessage-alice:29331");
    expect(files.config_yaml).toContain("address: http://imessage-alice:29331");
    expect(files.run_sh).toContain("xattr -d com.apple.quarantine");
    expect(files.run_sh).toContain("mau.dev/mautrix/imessage/-/jobs/95972/artifacts/download");
  });

  it("gives a container type no run.sh, and a cluster instance its Service url", () => {
    expect(instanceFiles("mautrix-whatsapp", "@alice:example.org")?.run_sh).toBeNull();
    expect(getInstance("mautrix-whatsapp", "@alice:example.org")?.url).toMatch(
      /^http:\/\/bridge-[0-9a-f]{8}\.myelin\.svc:29318$/,
    );
  });

  it("takes a host, host:port or URL, a bare host taking the type's port", () => {
    expect(instanceUrlFor("mautrix-imessage", "alices-mac.local")).toEqual({
      url: "http://alices-mac.local:29331",
    });
    expect(instanceUrlFor("mautrix-imessage", "100.64.0.7:29400")).toEqual({
      url: "http://100.64.0.7:29400",
    });
    expect(instanceUrlFor("mautrix-imessage", "https://mac.example.net/bridge")).toEqual({
      url: "https://mac.example.net/bridge",
    });
    expect(instanceUrlFor("mautrix-imessage", "")).toMatchObject({ refusal: expect.any(String) });
    expect(instanceUrlFor("mautrix-imessage", "ssh://mac.local")).toMatchObject({
      refusal: expect.stringContaining("http://"),
    });
  });

  it("rewrites the instance's url and its files when an address is PUT", () => {
    const saved = putInstance("mautrix-imessage", "@alice:example.org", "alices-mac.local")!;
    expect(saved.url).toBe("http://alices-mac.local:29331");
    expect(getInstance("mautrix-imessage", "@alice:example.org")?.url).toBe(
      "http://alices-mac.local:29331",
    );
    const files = instanceFiles("mautrix-imessage", "@alice:example.org")!;
    expect(files.registration_yaml).toContain("url: http://alices-mac.local:29331");
    expect(files.config_yaml).toContain("address: http://alices-mac.local:29331");
    // Without an address the PUT is the old idempotent create-or-return.
    expect(putInstance("mautrix-imessage", "@alice:example.org")?.url).toBe(
      "http://alices-mac.local:29331",
    );
  });

  it("refuses an address for an instance this server deploys", () => {
    expect(addressRefusal("mautrix-whatsapp", "somewhere.local")).toMatch(/runs in this cluster/);
    expect(addressRefusal("mautrix-imessage", "alices-mac.local")).toBeNull();
    expect(addressRefusal("mautrix-imessage", "")).toMatch(/names no host/);
    const before = getInstance("mautrix-whatsapp", "@alice:example.org")?.url;
    putInstance("mautrix-whatsapp", "@alice:example.org", "somewhere.local");
    expect(getInstance("mautrix-whatsapp", "@alice:example.org")?.url).toBe(before);
  });
});
