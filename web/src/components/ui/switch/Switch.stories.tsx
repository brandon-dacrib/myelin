import type { Meta, StoryObj } from "@storybook/react-vite";
import { Switch } from "./Switch";

const meta: Meta<typeof Switch> = {
  title: "Primitives/Switch",
  component: Switch,
  args: { "aria-label": "Enable registration" },
};
export default meta;
type Story = StoryObj<typeof Switch>;

export const Off: Story = {};
export const On: Story = { args: { defaultChecked: true } };
export const Disabled: Story = { args: { disabled: true, defaultChecked: true } };

export const InARow: Story = {
  render: (args) => (
    <div className="max-w-md divide-y divide-border rounded-md border border-border">
      {[
        { label: "Enable registration", checked: false },
        { label: "Verify certificates", checked: true },
        { label: "URL preview enabled", checked: false },
      ].map((row) => (
        <div key={row.label} className="flex items-center justify-between gap-4 px-4 py-3">
          <span className="text-sm text-text">{row.label}</span>
          <Switch {...args} aria-label={row.label} defaultChecked={row.checked} />
        </div>
      ))}
    </div>
  ),
};
