import type { Meta, StoryObj } from "@storybook/react-vite";
import { Sparkline } from "./Sparkline";

/**
 * The small trend line the Overview's activity tiles and the Cluster page's heartbeat cells
 * draw: one series, no axis, in the text colour it is given (decorative, `aria-hidden`; the
 * number beside it carries the meaning).
 */
const meta: Meta<typeof Sparkline> = {
  title: "Data/Sparkline",
  component: Sparkline,
};
export default meta;
type Story = StoryObj<typeof Sparkline>;

const values = (list: number[]) => list.map((value) => ({ value }));

/** A week of daily active users, as on the Overview. */
export const ActivityTrend: Story = {
  render: () => (
    <div className="w-48 rounded-md border border-border bg-surface p-4">
      <p className="text-xs text-text-muted">Daily active, 7-day trend</p>
      <p className="mt-1 text-2xl text-text tabular-nums">214</p>
      <Sparkline
        points={values([180, 192, 175, 201, 220, 208, 214])}
        className="mt-2 h-7 w-full text-accent"
      />
    </div>
  ),
};

/** Heartbeats per poll for a replica that is up: about seven every 15 seconds. */
export const HeartbeatsArriving: Story = {
  render: () => (
    <div className="flex flex-col gap-0.5 text-sm">
      <span className="text-text">just now</span>
      <span className="text-xs text-text-muted">seq 10,482</span>
      <span className="text-xs text-text-muted">+7 since the last poll</span>
      <Sparkline points={values([7, 8, 7, 7, 8, 7, 7])} className="h-4 w-24 text-accent" />
    </div>
  ),
};

/** A replica whose heartbeats stopped reaching the store: the line drops to zero. */
export const HeartbeatsStopped: Story = {
  render: () => (
    <div className="flex flex-col gap-0.5 text-sm">
      <span className="text-text">1 minute ago</span>
      <span className="text-xs text-text-muted">seq 10,503</span>
      <span className="text-xs text-warning">No new heartbeat since 1 minute ago</span>
      <Sparkline points={values([7, 8, 7, 3, 0, 0, 0])} className="h-4 w-24 text-accent" />
    </div>
  ),
};
