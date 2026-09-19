import type { Meta, StoryObj } from "@storybook/react-vite";
import { ErrorState, ForbiddenState, NotImplementedState } from "./ErrorState";

const meta: Meta = { title: "Primitives/ErrorState" };
export default meta;

export const Default: StoryObj = {
  render: () => (
    <ErrorState
      title="Couldn't load bridges"
      problem={{ detail: "The server returned a 503.", requestId: "req_8f21ac" }}
      onRetry={() => {}}
    />
  ),
};

export const Forbidden: StoryObj = {
  render: () => <ForbiddenState scope="bridges:read" />,
};

/**
 * The 501 treatment (docs/status/16-management-web-interface.md, "Degrade honestly"): plain,
 * neutral, `role="status"` rather than `role="alert"` — an unbuilt operation on this server is
 * not a fault the operator needs to worry about.
 */
export const NotImplemented: StoryObj = {
  render: () => <NotImplementedState resource="Bridges" />,
};

/** The 503 treatment: the handler exists but has no data source wired up yet. Offers "Check
 * again" since this one can plausibly resolve without a deploy. */
export const Unavailable: StoryObj = {
  render: () => (
    <NotImplementedState
      resource="Users"
      variant="unavailable"
      problem={{ detail: "No user directory is attached to this server." }}
      onRetry={() => {}}
    />
  ),
};

/** The same 501 state embedded inline within a smaller region (a dashboard tile, a detail-page
 * section) rather than taking a full page — see `compact` on every state in this file. */
export const CompactInline: StoryObj = {
  render: () => (
    <div className="max-w-xs rounded-md border border-border p-3">
      <NotImplementedState resource="The backlog" compact />
    </div>
  ),
};
