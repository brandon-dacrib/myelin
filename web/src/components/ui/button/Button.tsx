import { forwardRef } from "react";
import type { ButtonHTMLAttributes, ReactNode } from "react";
import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "@/lib/cn";

export const buttonVariants = cva(
  [
    "inline-flex items-center justify-center gap-2 rounded-sm font-medium",
    "transition-colors duration-fast ease-out",
    "disabled:pointer-events-none disabled:opacity-50",
    "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]",
  ],
  {
    variants: {
      variant: {
        primary: "bg-accent text-accent-text-on hover:bg-accent-hover",
        secondary: "bg-surface text-text border border-border-strong hover:bg-surface-sunken",
        ghost: "text-text hover:bg-surface-sunken",
        danger: "bg-danger text-danger-text-on hover:opacity-90",
      },
      size: {
        sm: "h-8 px-3 text-sm",
        md: "h-9 px-4 text-base",
        lg: "h-10 px-5 text-md",
        icon: "h-9 w-9",
      },
    },
    defaultVariants: { variant: "primary", size: "md" },
  },
);

export interface ButtonProps
  extends ButtonHTMLAttributes<HTMLButtonElement>, VariantProps<typeof buttonVariants> {
  /** For icon-only buttons: a visible label is required for accessibility.md #4 unless this is set. */
  "aria-label"?: string;
  leadingIcon?: ReactNode;
}

export const Button = forwardRef<HTMLButtonElement, ButtonProps>(
  ({ className, variant, size, leadingIcon, children, ...props }, ref) => {
    return (
      <button ref={ref} className={cn(buttonVariants({ variant, size }), className)} {...props}>
        {leadingIcon}
        {children}
      </button>
    );
  },
);
Button.displayName = "Button";
