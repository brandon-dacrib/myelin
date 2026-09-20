import { forwardRef } from "react";
import { Root, Thumb } from "radix-ui/switch";
import { cn } from "@/lib/cn";

export interface SwitchProps {
  checked?: boolean;
  defaultChecked?: boolean;
  onCheckedChange?: (checked: boolean) => void;
  disabled?: boolean;
  id?: string;
  name?: string;
  "aria-label"?: string;
  "aria-labelledby"?: string;
  "aria-describedby"?: string;
  className?: string;
}

/**
 * A boolean setting. `role="switch"` rather than a checkbox because it takes
 * effect on its own rather than as part of a submitted set (accessibility.md
 * #4); the track carries the on/off state as shape and position as well as
 * colour, so it still reads at a glance in a high-contrast or monochrome
 * rendering (#5: colour is never the only channel).
 */
export const Switch = forwardRef<HTMLButtonElement, SwitchProps>(
  ({ checked, defaultChecked, onCheckedChange, disabled, id, name, className, ...aria }, ref) => (
    <Root
      ref={ref}
      id={id}
      name={name}
      checked={checked}
      defaultChecked={defaultChecked}
      onCheckedChange={onCheckedChange}
      disabled={disabled}
      className={cn(
        "relative inline-flex h-6 w-10 shrink-0 cursor-pointer items-center rounded-full border transition-colors duration-fast ease-out",
        "border-border-strong bg-surface-sunken data-[state=checked]:border-accent data-[state=checked]:bg-accent",
        "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]",
        "disabled:cursor-not-allowed disabled:opacity-50",
        className,
      )}
      {...aria}
    >
      <Thumb
        className={cn(
          "pointer-events-none block h-4 w-4 translate-x-1 rounded-full bg-text-muted shadow-1",
          "transition-transform duration-fast ease-out",
          "data-[state=checked]:translate-x-5 data-[state=checked]:bg-accent-text-on",
        )}
      />
    </Root>
  ),
);
Switch.displayName = "Switch";
