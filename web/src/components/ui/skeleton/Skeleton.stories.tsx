import type { Meta, StoryObj } from "@storybook/react-vite";
import { Skeleton, SkeletonText, SkeletonTableRows } from "./Skeleton";

const meta: Meta = { title: "Primitives/Skeleton" };
export default meta;

export const Block: StoryObj = { render: () => <Skeleton className="h-24 w-64" /> };
export const Text: StoryObj = { render: () => <SkeletonText lines={3} className="w-80" /> };
export const TableRows: StoryObj = { render: () => <SkeletonTableRows rows={5} cols={5} /> };
