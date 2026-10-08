import { afterEach, describe, expect, it } from "vitest";
import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { server } from "@/mocks/node";
import { getRateLimit } from "@/mocks/data/user-moderation";
import { signIn, signOut } from "@/lib/auth";
import { describeRateLimit } from "@/api/user-moderation";
import { configValues } from "@/mocks/data/config";
import { renderRoutes } from "@/test/render-route";
import { RateLimitSection } from "./RateLimitSection";

const ALICE = "@alice:example.org";
const BRIDGED = "@whatsapp_15551234:example.org";

function renderSection(userId: string, props: { admin?: boolean; appserviceId?: string } = {}) {
  renderRoutes(
    [{ path: "/", component: () => <RateLimitSection userId={userId} {...props} /> }],
    "/",
    ["/configuration/$section", "/bridges/$bridgeId"],
  );
}

afterEach(() => signOut());

describe("describeRateLimit", () => {
  it("words an override as an operator would read it", () => {
    expect(describeRateLimit({ messages_per_second: 0 })).toBe("Exempt from message rate limits");
    expect(describeRateLimit({ messages_per_second: 1 })).toBe("1 message a second, bursts of 10");
    expect(describeRateLimit({ messages_per_second: 0.5, burst_count: 3 })).toBe(
      "0.5 messages a second, bursts of 3",
    );
  });
});

describe("RateLimitSection", () => {
  it("says the server's limits apply when there is no override, then sets and clears one", async () => {
    await signIn();
    const user = userEvent.setup();
    renderSection(ALICE);
    expect(
      await screen.findByText("No override: the server's own limits apply."),
    ).toBeInTheDocument();
    // The server-wide limit it would replace, from the answer's server_wide.
    expect(screen.getByTestId("rate-limit-server-wide")).toHaveTextContent(
      "Server-wide limit: 0.5 messages a second, bursts of 25.",
    );
    expect(screen.getByText(/Server-wide: 0\.5\./)).toBeInTheDocument();
    expect(screen.getByText(/Server-wide: 25\./)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Clear override" })).not.toBeInTheDocument();

    await user.type(screen.getByLabelText("Messages per second"), "2");
    await user.clear(screen.getByLabelText("Burst"));
    await user.type(screen.getByLabelText("Burst"), "5");
    await user.click(screen.getByRole("button", { name: "Save limit" }));
    expect(
      await screen.findByText("Override: 2 messages a second, bursts of 5"),
    ).toBeInTheDocument();
    expect(getRateLimit(ALICE)).toEqual({ messages_per_second: 2, burst_count: 5 });
    // The save's answer has no server_wide; the section keeps the one it read.
    expect(screen.getByTestId("rate-limit-server-wide")).toHaveTextContent(
      "Server-wide limit: 0.5 messages a second, bursts of 25. This override replaces it for them; clearing the override puts them back on it.",
    );

    await user.click(screen.getByRole("button", { name: "Clear override" }));
    expect(
      await screen.findByText("No override: the server's own limits apply."),
    ).toBeInTheDocument();
    expect(screen.getByTestId("rate-limit-server-wide")).toHaveTextContent(
      "Server-wide limit: 0.5 messages a second, bursts of 25.",
    );
    expect(getRateLimit(ALICE)).toEqual({});
  });

  it("shows an existing exemption in the fields", async () => {
    await signIn();
    renderSection(BRIDGED);
    expect(
      await screen.findByText("Override: Exempt from message rate limits"),
    ).toBeInTheDocument();
    expect(screen.getByLabelText("Messages per second")).toHaveValue(0);
  });

  it("refuses a negative rate beside the field before asking the server", async () => {
    await signIn();
    const user = userEvent.setup();
    renderSection(ALICE);
    await user.type(await screen.findByLabelText("Messages per second"), "-1");
    await user.click(screen.getByRole("button", { name: "Save limit" }));
    expect(await screen.findByText(/0 or more\. 0 exempts them/)).toBeInTheDocument();
    expect(getRateLimit(ALICE)).toEqual({});
  });

  it("puts the server's refusal beside the field its pointer names", async () => {
    await signIn();
    server.use(
      http.put("*/api/v1/users/:user_id/rate-limit", () =>
        HttpResponse.json(
          {
            type: "urn:hs:problem:validation-failed",
            title: "Validation failed",
            status: 400,
            errors: [{ pointer: "/burst_count", detail: "burst_count is too large" }],
          },
          { status: 400 },
        ),
      ),
    );
    const user = userEvent.setup();
    renderSection(ALICE);
    await user.type(await screen.findByLabelText("Messages per second"), "3");
    await user.click(screen.getByRole("button", { name: "Save limit" }));
    expect(await screen.findByText("burst_count is too large")).toBeInTheDocument();
    expect(screen.getByLabelText("Burst")).toHaveAttribute("aria-invalid", "true");
  });

  it("is read-only without admin:write", async () => {
    await signIn(["admin:read"]);
    renderSection(BRIDGED);
    expect(await screen.findByText("Changing it needs admin:write.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Save limit" })).not.toBeInTheDocument();
  });
  it("says an override still applies when rate limits are off server-wide", async () => {
    await signIn();
    const limits = configValues.rate_limits as { enabled: boolean };
    limits.enabled = false;
    try {
      renderSection(BRIDGED, { appserviceId: "whatsapp" });
      const line = await screen.findByTestId("rate-limit-server-wide");
      expect(line).toHaveTextContent(
        "Server-wide, rate limits are switched off, so nobody without an override is limited. This override still applies to them; clearing it leaves them unlimited.",
      );
      expect(screen.getByTestId("rate-limit-bridge")).toHaveTextContent(
        "This account belongs to the bridge whatsapp.",
      );
      expect(screen.getByRole("link", { name: "whatsapp" })).toBeInTheDocument();
    } finally {
      limits.enabled = true;
    }
  });

  it("gives a server administrator's redaction limit", async () => {
    await signIn();
    renderSection(ALICE, { admin: true });
    expect(await screen.findByTestId("rate-limit-redactions")).toHaveTextContent(
      "As a server administrator, their redactions have the administrator redaction limit instead: 1 redaction a second, bursts of 50.",
    );
  });

  it("says so when the server-wide limit is not in the answer", async () => {
    await signIn();
    server.use(http.get("*/api/v1/users/:user_id/rate-limit", () => HttpResponse.json({})));
    renderSection(ALICE);
    expect(await screen.findByTestId("rate-limit-server-wide")).toHaveTextContent(
      "The server-wide limit could not be read here.",
    );
    expect(screen.getByLabelText("Messages per second")).toBeInTheDocument();
  });
});
