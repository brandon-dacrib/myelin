import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { ResetPasswordDialog } from "./ResetPasswordDialog";
import { userDevices } from "@/mocks/data/users";
import { signIn, signOut } from "@/lib/auth";

const ALICE = "@alice:example.org";

function renderDialog() {
  const onOpenChange = vi.fn();
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={client}>
      <ResetPasswordDialog userId={ALICE} open onOpenChange={onOpenChange} />
    </QueryClientProvider>,
  );
  return { onOpenChange };
}

const devicesBefore = userDevices[ALICE] ? [...userDevices[ALICE]] : [];

beforeEach(async () => {
  await signIn();
});

afterEach(() => {
  userDevices[ALICE] = [...devicesBefore];
  signOut();
});

describe("ResetPasswordDialog", () => {
  it("refuses a weak password beside the field and keeps the dialog open", async () => {
    const user = userEvent.setup();
    renderDialog();
    const dialog = within(await screen.findByRole("dialog", { name: "Reset password" }));
    await user.type(dialog.getByLabelText(/new password/i), "short");
    await user.click(dialog.getByRole("button", { name: "Reset password" }));
    expect(await dialog.findByText(/at least 8 characters/)).toBeInTheDocument();
    expect(screen.getByRole("dialog", { name: "Reset password" })).toBeInTheDocument();
  });

  it("generates a password, resets it, signs everything out and hands it over once", async () => {
    const user = userEvent.setup();
    renderDialog();
    const dialog = within(await screen.findByRole("dialog", { name: "Reset password" }));
    await user.click(dialog.getByRole("button", { name: "Generate" }));
    const field = dialog.getByLabelText(/new password/i) as HTMLInputElement;
    expect(field.value.length).toBeGreaterThanOrEqual(16);
    expect(field.type).toBe("text");
    const generated = field.value;

    await user.click(dialog.getByRole("button", { name: "Reset password" }));
    const done = within(await screen.findByRole("dialog", { name: "Password reset" }));
    expect(done.getByText(generated)).toBeInTheDocument();
    expect(done.getByText(/every device .* signed out/i)).toBeInTheDocument();
    expect(userDevices[ALICE]).toEqual([]);
  });

  it("keeps sessions when told to", async () => {
    const user = userEvent.setup();
    renderDialog();
    const dialog = within(await screen.findByRole("dialog", { name: "Reset password" }));
    await user.type(dialog.getByLabelText(/new password/i), "a perfectly good password");
    await user.click(dialog.getByRole("switch", { name: /sign out everywhere/i }));
    await user.click(dialog.getByRole("button", { name: "Reset password" }));
    const done = within(await screen.findByRole("dialog", { name: "Password reset" }));
    expect(done.getByText(/still signed in/i)).toBeInTheDocument();
    expect(userDevices[ALICE]).toEqual(devicesBefore);
  });
});
