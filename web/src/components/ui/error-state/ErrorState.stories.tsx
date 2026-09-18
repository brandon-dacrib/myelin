import type { Meta, StoryObj } from "@storybook/react-vite";
import { ErrorState, ForbiddenState } from "./ErrorState";

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
