import type { Meta, StoryObj } from "@storybook/react-vite";
import { Toaster } from "./Toaster";
import { toast } from "./toast-store";
import { Button } from "../button/Button";

const meta: Meta = { title: "Primitives/Toast" };
export default meta;

export const Default: StoryObj = {
  render: () => (
    <div>
      <Button
        onClick={() =>
          toast({ title: "Bridge Discord paused", action: { label: "Undo", onClick: () => {} } })
        }
      >
        Show toast
      </Button>
      <Toaster />
    </div>
  ),
};

export const Danger: StoryObj = {
  render: () => (
    <div>
      <Button
        variant="danger"
        onClick={() =>
          toast({
            title: "Couldn't rotate tokens",
            description: "The bridge did not respond in time.",
            variant: "danger",
            action: { label: "Retry", onClick: () => {} },
          })
        }
      >
        Show error toast
      </Button>
      <Toaster />
    </div>
  ),
};
