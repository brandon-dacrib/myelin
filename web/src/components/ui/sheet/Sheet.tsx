import type { ReactNode } from "react";
import {
  Root,
  Trigger,
  Portal,
  Overlay,
  Content,
  Title,
  Description,
  Close,
} from "radix-ui/dialog";
import { X } from "lucide-react";
import { cn } from "@/lib/cn";

export { Root as Sheet, Trigger as SheetTrigger, Close as SheetClose };

export interface SheetContentProps {
  title: ReactNode;
  description?: ReactNode;
  children?: ReactNode;
  footer?: ReactNode;
  side?: "left" | "right";
  className?: string;
}

/** A drawer from the edge it lives on (navigation drawer, filters, detail peek). */
export function SheetContent({
  title,
  description,
  children,
  footer,
  side = "right",
  className,
}: SheetContentProps) {
  return (
    <Portal>
      <Overlay
        className={cn(
          "fixed inset-0 z-50 bg-black/40",
          "data-[state=open]:animate-[overlay-in_240ms_ease-out] data-[state=closed]:animate-[overlay-out_120ms_ease-in]",
        )}
      />
      <Content
        className={cn(
          "fixed inset-y-0 z-50 flex w-[calc(100vw-2rem)] max-w-sm flex-col overflow-y-auto",
          "border-border bg-surface-raised p-6 shadow-4",
          side === "right" ? "right-0 border-l" : "left-0 border-r",
          side === "right"
            ? "data-[state=open]:animate-[sheet-in-right_240ms_ease-out] data-[state=closed]:animate-[sheet-out-right_120ms_ease-in]"
            : "data-[state=open]:animate-[sheet-in-right_240ms_ease-out] data-[state=closed]:animate-[sheet-out-right_120ms_ease-in] [animation-direction:reverse]",
          className,
        )}
      >
        <Title className="text-lg text-text">{title}</Title>
        {description && (
          <Description className="mt-1 text-base text-text-muted">{description}</Description>
        )}
        <div className="mt-4 flex-1">{children}</div>
        {footer && <div className="mt-6 flex justify-end gap-2">{footer}</div>}
        <Close
          aria-label="Close"
          className={cn(
            "absolute right-4 top-4 rounded-sm p-1 text-text-muted hover:bg-surface-sunken hover:text-text",
            "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]",
          )}
        >
          <X size={16} aria-hidden="true" />
        </Close>
      </Content>
    </Portal>
  );
}
