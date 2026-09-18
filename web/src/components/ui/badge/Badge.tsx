import type { ReactNode } from "react";
import { CircleCheck, TriangleAlert, CircleX, Info, CirclePause } from "lucide-react";
import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "@/lib/cn";

export const badgeVariants = cva(
  "inline-flex items-center gap-1.5 rounded-xs border px-2 py-0.5 text-xs font-medium",
  {
    variants: {
      status: {
        success: "bg-success-bg text-success border-success-border",
        warning: "bg-warning-bg text-warning border-warning-border",
        danger: "bg-danger-bg text-danger border-danger-border",
        info: "bg-info-bg text-info border-info-border",
        muted: "bg-muted-status-bg text-muted-status border-muted-status-border",
        neutral: "bg-surface-sunken text-text-muted border-border",
      },
    },
    defaultVariants: { status: "neutral" },
  },
);

const icons: Record<
  NonNullable<VariantProps<typeof badgeVariants>["status"]>,
  typeof CircleCheck
> = {
  success: CircleCheck,
  warning: TriangleAlert,
  danger: CircleX,
  info: Info,
  muted: CirclePause,
  neutral: Info,
};

export interface BadgeProps extends VariantProps<typeof badgeVariants> {
  children: ReactNode;
  /** Status pills always carry an icon (accessibility.md #5: colour is never the only channel). */
  hideIcon?: boolean;
  className?: string;
}

/** A status pill: colour + icon + text, never colour alone. */
export function Badge({ status = "neutral", children, hideIcon, className }: BadgeProps) {
  const Icon = icons[status ?? "neutral"];
  return (
    <span className={cn(badgeVariants({ status }), className)}>
      {!hideIcon && <Icon size={12} aria-hidden="true" />}
      {children}
    </span>
  );
}
