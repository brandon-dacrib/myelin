import type { Meta, StoryObj } from "@storybook/react-vite";
import { Select } from "./Select";

const meta: Meta<typeof Select> = {
  title: "Primitives/Select",
  component: Select,
  args: {
    "aria-label": "Filter by state",
    placeholder: "All states",
    options: [
      { value: "running", label: "Running" },
      { value: "paused", label: "Paused" },
      { value: "bridge_unreachable", label: "Unreachable" },
      { value: "waiting_for_ping", label: "Waiting for ping" },
      { value: "unconfigured", label: "Unconfigured", disabled: true },
    ],
  },
  render: (args) => (
    <div className="w-64">
      <Select {...args} />
    </div>
  ),
};
export default meta;
type Story = StoryObj<typeof Select>;

export const Default: Story = {};
export const WithValue: Story = { args: { defaultValue: "running" } };
export const Disabled: Story = { args: { disabled: true, defaultValue: "paused" } };
