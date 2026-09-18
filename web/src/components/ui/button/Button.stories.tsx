import type { Meta, StoryObj } from "@storybook/react-vite";
import { Trash2 } from "lucide-react";
import { Button } from "./Button";

const meta: Meta<typeof Button> = {
  title: "Primitives/Button",
  component: Button,
  args: { children: "Add bridge" },
  argTypes: {
    variant: { control: "select", options: ["primary", "secondary", "ghost", "danger"] },
    size: { control: "select", options: ["sm", "md", "lg", "icon"] },
  },
};
export default meta;
type Story = StoryObj<typeof Button>;

export const Primary: Story = { args: { variant: "primary" } };
export const Secondary: Story = { args: { variant: "secondary", children: "Cancel" } };
export const Ghost: Story = { args: { variant: "ghost", children: "Clear filters" } };
export const Danger: Story = { args: { variant: "danger", children: "Deactivate" } };
export const Disabled: Story = { args: { variant: "primary", disabled: true } };
export const WithIcon: Story = {
  args: {
    variant: "danger",
    children: "Remove bridge",
    leadingIcon: <Trash2 size={16} aria-hidden="true" />,
  },
};
export const IconOnly: Story = {
  args: {
    variant: "ghost",
    size: "icon",
    children: <Trash2 size={16} aria-hidden="true" />,
    "aria-label": "Delete",
  },
};

export const AllSizes: Story = {
  render: () => (
    <div className="flex items-center gap-3">
      <Button size="sm">Small</Button>
      <Button size="md">Medium</Button>
      <Button size="lg">Large</Button>
    </div>
  ),
};
