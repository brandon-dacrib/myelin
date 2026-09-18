import type { Meta, StoryObj } from "@storybook/react-vite";
import { Dialog, DialogTrigger, DialogClose, DialogContent } from "./Dialog";
import { Button } from "../button/Button";

const meta: Meta = { title: "Primitives/Dialog" };
export default meta;

export const Confirmation: StoryObj = {
  render: () => (
    <Dialog>
      <DialogTrigger asChild>
        <Button variant="danger">Suspend user</Button>
      </DialogTrigger>
      <DialogContent
        title="Suspend @alice:example.org?"
        description="They can read but not send until you lift it. This action is recorded in the audit log."
        footer={
          <>
            <DialogClose asChild>
              <Button variant="secondary">Cancel</Button>
            </DialogClose>
            <DialogClose asChild>
              <Button variant="danger">Suspend</Button>
            </DialogClose>
          </>
        }
      />
    </Dialog>
  ),
};

export const Form: StoryObj = {
  render: () => (
    <Dialog>
      <DialogTrigger asChild>
        <Button>Send server notice</Button>
      </DialogTrigger>
      <DialogContent
        title="Send a server notice"
        description="Delivered as a message from the server notices bot."
        size="form"
        footer={
          <>
            <DialogClose asChild>
              <Button variant="secondary">Cancel</Button>
            </DialogClose>
            <Button>Send</Button>
          </>
        }
      >
        <textarea
          className="h-24 w-full rounded-sm border border-border-strong bg-surface p-3 text-base text-text"
          placeholder="Scheduled maintenance begins at 02:00 UTC."
        />
      </DialogContent>
    </Dialog>
  ),
};
