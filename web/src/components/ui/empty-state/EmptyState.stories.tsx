import type { Meta, StoryObj } from "@storybook/react-vite";
import { Cable } from "lucide-react";
import { EmptyState } from "./EmptyState";
import { Button } from "../button/Button";

const meta: Meta<typeof EmptyState> = { title: "Primitives/EmptyState", component: EmptyState };
export default meta;
type Story = StoryObj<typeof EmptyState>;

export const NoBridgesYet: Story = {
  args: {
    icon: <Cable aria-hidden="true" />,
    title: "No bridges yet",
    description: "Bridges connect WhatsApp, Signal, Telegram and other networks to this server.",
    action: <Button>Add bridge</Button>,
    docsHref: "#",
  },
};

export const Filtered: Story = {
  args: {
    variant: "filtered",
    icon: <Cable aria-hidden="true" />,
    title: "No bridges match these filters",
    action: <Button variant="ghost">Clear filters</Button>,
  },
};
