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

export { Root as Dialog, Trigger as DialogTrigger, Close as DialogClose };

/**
 * Confirmation dialogs are 480px; form dialogs 640px (states-density-responsiveness.md #3).
 * Every dialog states what will happen before the confirm (information-architecture.md #4).
 */
export interface DialogContentProps {
  title: ReactNode;
  description?: ReactNode;
  children?: ReactNode;
  footer?: ReactNode;
  size?: "confirm" | "form";
  className?: string;
}

export function DialogContent({
  title,
  description,
  children,
  footer,
  size = "confirm",
  className,
}: DialogContentProps) {
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
          "fixed left-1/2 top-1/2 z-50 max-h-[85vh] w-[calc(100vw-2rem)] -translate-x-1/2 -translate-y-1/2 overflow-y-auto",
          "rounded-lg border border-border bg-surface-raised p-6 shadow-3",
          "data-[state=open]:animate-[dialog-content-in_240ms_ease-out] data-[state=closed]:animate-[dialog-content-out_120ms_ease-in]",
          size === "confirm" ? "max-w-[480px]" : "max-w-[640px]",
          className,
        )}
      >
        <Title className="text-lg text-text">{title}</Title>
        {description && (
          <Description className="mt-1 text-base text-text-muted">{description}</Description>
        )}
        {children && <div className="mt-4">{children}</div>}
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
