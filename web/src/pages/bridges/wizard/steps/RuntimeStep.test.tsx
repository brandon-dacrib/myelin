import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { setDeploymentTarget } from "@/mocks/data/bridge-offerings";
import { bridgeTypes } from "@/mocks/data/bridge-types";
import { signIn, signOut } from "@/lib/auth";
import { initialOfferState, stateForKind, type OfferFormState } from "../offer-state";
import { RuntimeStep } from "./RuntimeStep";

function renderStep(typeId: string, patch: Partial<OfferFormState> = {}) {
  const type = bridgeTypes.find((t) => t.id === typeId)!;
  const state = { ...initialOfferState, ...stateForKind(type), ...patch };
  const onChange = vi.fn();
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={client}>
      <RuntimeStep state={state} onChange={onChange} type={type} />
    </QueryClientProvider>,
  );
  return { onChange };
}

beforeEach(async () => {
  await signIn();
});
afterEach(() => signOut());

describe("Offer a bridge: Runtime", () => {
  it("offers this cluster, chosen, when the server can deploy", async () => {
    const { onChange } = renderStep("mautrix-whatsapp");
    const cluster = await screen.findByRole("radio", { name: /Runs in this cluster/ });
    await vi.waitFor(() => expect(cluster).toBeEnabled());
    expect(cluster).toHaveAttribute("aria-checked", "true");
    expect(cluster).toHaveTextContent("in myelin");
    expect(screen.getByRole("radio", { name: /Runs elsewhere/ })).toHaveAttribute(
      "aria-checked",
      "false",
    );
    expect(screen.queryByText(/Why this server can't run bridges itself/)).toBeNull();

    await userEvent.click(screen.getByRole("radio", { name: /Runs elsewhere/ }));
    expect(onChange).toHaveBeenCalledWith({ runtime: "elsewhere" });
  });

  it("explains, with the server's reason, when the server cannot deploy", async () => {
    setDeploymentTarget(false, "MYELIN_BRIDGES_NAMESPACE is not set.");
    renderStep("mautrix-whatsapp");
    expect(await screen.findByText("MYELIN_BRIDGES_NAMESPACE is not set.")).toBeInTheDocument();
    expect(screen.getByText("Why this server can't run bridges itself")).toBeInTheDocument();
    const cluster = screen.getByRole("radio", { name: /Runs in this cluster/ });
    expect(cluster).toBeDisabled();
    expect(cluster).toHaveTextContent("This server can't deploy bridges.");
    // What will be sent is elsewhere, whatever was chosen before.
    expect(screen.getByRole("radio", { name: /Runs elsewhere/ })).toHaveAttribute(
      "aria-checked",
      "true",
    );
  });

  it("keeps a type that cannot run from its config out of the cluster, and says why", async () => {
    renderStep("mautrix-imessage");
    const cluster = await screen.findByRole("radio", { name: /Runs in this cluster/ });
    // The catalogue's own reason, not the interface's fallback wording.
    await vi.waitFor(() =>
      expect(cluster).toHaveTextContent(/run on a Mac, beside the Messages app they read/),
    );
    expect(cluster).toBeDisabled();
    expect(screen.queryByText(/Why this server can't run bridges itself/)).toBeNull();
  });

  it("shows the pinned build, not an image tag, for a bridge that runs on a Mac", async () => {
    renderStep("mautrix-imessage");
    expect(await screen.findByRole("radio", { name: /Runs on each person's Mac/ })).toBeVisible();
    expect(screen.queryByLabelText("Image tag")).toBeNull();
    expect(screen.getByTestId("mac-build")).toHaveTextContent(
      "Universal macOS build (Apple silicon and Intel) of mautrix-imessage commit 300ba6d0",
    );
  });

  it("edits the image tag", async () => {
    const { onChange } = renderStep("mautrix-whatsapp");
    const tag = await screen.findByLabelText("Image tag");
    await userEvent.type(tag, "x");
    expect(onChange).toHaveBeenLastCalledWith({ imageTag: "latestx" });
  });
});
