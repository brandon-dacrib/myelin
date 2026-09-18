import { forwardRef } from "react";
import type { ReactNode } from "react";
import {
  Root,
  Trigger,
  Value,
  Icon,
  Portal,
  Content,
  Viewport,
  Item,
  ItemText,
  ItemIndicator,
  ScrollUpButton,
  ScrollDownButton,
} from "radix-ui/select";
import { Check, ChevronDown, ChevronUp } from "lucide-react";
import { cn } from "@/lib/cn";

export interface SelectOption {
  value: string;
  label: string;
  disabled?: boolean;
}

export interface SelectProps {
  value?: string;
  defaultValue?: string;
  onValueChange?: (value: string) => void;
  options: SelectOption[];
  placeholder?: string;
  "aria-label"?: string;
  "aria-describedby"?: string;
  "aria-invalid"?: boolean;
  required?: boolean;
  id?: string;
  disabled?: boolean;
  className?: string;
}

export const Select = forwardRef<HTMLButtonElement, SelectProps>(
  (
    { value, defaultValue, onValueChange, options, placeholder, id, disabled, className, ...aria },
    ref,
  ) => {
    return (
      <Root
        value={value}
        defaultValue={defaultValue}
        onValueChange={onValueChange}
        disabled={disabled}
      >
        <Trigger
          ref={ref}
          id={id}
          className={cn(
            "inline-flex h-9 w-full items-center justify-between gap-2 rounded-sm border border-border-strong",
            "bg-surface px-3 text-base text-text",
            "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]",
            "disabled:cursor-not-allowed disabled:opacity-50 data-[placeholder]:text-text-faint",
            className,
          )}
          {...aria}
        >
          <Value placeholder={placeholder} />
          <Icon>
            <ChevronDown size={16} aria-hidden="true" className="text-text-muted" />
          </Icon>
        </Trigger>
        <Portal>
          <Content
            position="popper"
            sideOffset={4}
            className="z-50 overflow-hidden rounded-md border border-border bg-surface-raised shadow-2"
          >
            <ScrollUpButton className="flex items-center justify-center py-1">
              <ChevronUp size={14} aria-hidden="true" />
            </ScrollUpButton>
            <Viewport className="p-1">
              {options.map((opt) => (
                <SelectItem key={opt.value} value={opt.value} disabled={opt.disabled}>
                  {opt.label}
                </SelectItem>
              ))}
            </Viewport>
            <ScrollDownButton className="flex items-center justify-center py-1">
              <ChevronDown size={14} aria-hidden="true" />
            </ScrollDownButton>
          </Content>
        </Portal>
      </Root>
    );
  },
);
Select.displayName = "Select";

function SelectItem({
  value,
  disabled,
  children,
}: {
  value: string;
  disabled?: boolean;
  children: ReactNode;
}) {
  return (
    <Item
      value={value}
      disabled={disabled}
      className={cn(
        "relative flex h-9 cursor-pointer select-none items-center rounded-sm px-3 pr-8 text-base text-text outline-none",
        "data-[highlighted]:bg-surface-sunken data-[disabled]:pointer-events-none data-[disabled]:opacity-50",
      )}
    >
      <ItemText>{children}</ItemText>
      <ItemIndicator className="absolute right-2 inline-flex items-center">
        <Check size={16} aria-hidden="true" className="text-accent" />
      </ItemIndicator>
    </Item>
  );
}
