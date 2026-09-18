import type { Meta, StoryObj } from "@storybook/react-vite";
import { Badge } from "./Badge";

const meta: Meta<typeof Badge> = {
  title: "Primitives/Badge",
  component: Badge,
};
export default meta;
type Story = StoryObj<typeof Badge>;

export const AllStatuses: Story = {
  render: () => (
    <div className="flex flex-wrap gap-2">
      <Badge status="success">Running</Badge>
      <Badge status="warning">Backing off</Badge>
      <Badge status="danger">Unreachable</Badge>
      <Badge status="info">Waiting for ping</Badge>
      <Badge status="muted">Paused</Badge>
      <Badge status="neutral">Self-managed</Badge>
    </div>
  ),
};

export const WithoutIcon: Story = {
  args: { status: "neutral", children: "Kubernetes", hideIcon: true },
};
