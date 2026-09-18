import type { Meta, StoryObj } from "@storybook/react-vite";
import { Sheet, SheetTrigger, SheetClose, SheetContent } from "./Sheet";
import { Button } from "../button/Button";

const meta: Meta = { title: "Primitives/Sheet" };
export default meta;

export const Right: StoryObj = {
  render: () => (
    <Sheet>
      <SheetTrigger asChild>
        <Button variant="secondary">Filters</Button>
      </SheetTrigger>
      <SheetContent
        title="Filter bridges"
        description="Narrow the list by state, kind or pause status."
        footer={
          <>
            <SheetClose asChild>
              <Button variant="ghost">Clear</Button>
            </SheetClose>
            <SheetClose asChild>
              <Button>Apply</Button>
            </SheetClose>
          </>
        }
      >
        <p className="text-sm text-text-muted">Filter controls go here.</p>
      </SheetContent>
    </Sheet>
  ),
};

export const Left: StoryObj = {
  name: "Left (navigation drawer, tablet)",
  render: () => (
    <Sheet>
      <SheetTrigger asChild>
        <Button variant="secondary">Open navigation</Button>
      </SheetTrigger>
      <SheetContent side="left" title="hs admin">
        <p className="text-sm text-text-muted">Navigation goes here.</p>
      </SheetContent>
    </Sheet>
  ),
};
