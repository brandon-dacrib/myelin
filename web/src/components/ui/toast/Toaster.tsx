import { Provider, Viewport, Root, Title, Description, Action, Close } from "radix-ui/toast";
import { X } from "lucide-react";
import { cn } from "@/lib/cn";
import { dismissToast, useToasts } from "./toast-store";

/**
 * Mounted once by the app shell. Live region behaviour (aria-live) comes
 * from Radix's ToastProvider; per accessibility.md #6 toasts announce
 * politely. A toast never carries the only copy of an error the operator
 * will need later (states-density-responsiveness.md #1): callers pass an
 * `action` that links to where the error lives, when there is one.
 */
export function Toaster() {
  const toasts = useToasts();
  return (
    <Provider swipeDirection="right" duration={6000}>
      {toasts.map((t) => (
        <Root
          key={t.id}
          onOpenChange={(open) => {
            if (!open) dismissToast(t.id);
          }}
          className={cn(
            "grid grid-cols-[auto_max-content] items-start gap-x-3 gap-y-1 rounded-md border p-4 shadow-2",
            "data-[state=open]:animate-[toast-in-right_240ms_ease-out] data-[state=closed]:animate-[toast-out_120ms_ease-in]",
            t.variant === "danger"
              ? "border-danger-border bg-danger-bg"
              : "border-border bg-surface-raised",
          )}
        >
          <Title className="text-sm font-medium text-text">{t.title}</Title>
          {t.description && (
            <Description className="col-span-2 text-sm text-text-muted">
              {t.description}
            </Description>
          )}
          {t.action && (
            <Action altText={t.action.label} asChild>
              <button
                onClick={t.action.onClick}
                className="col-start-1 justify-self-start text-sm font-medium text-accent hover:underline"
              >
                {t.action.label}
              </button>
            </Action>
          )}
          <Close
            aria-label="Dismiss"
            className="row-start-1 col-start-2 text-text-muted hover:text-text"
          >
            <X size={14} aria-hidden="true" />
          </Close>
        </Root>
      ))}
      <Viewport className="fixed bottom-4 right-4 z-[100] flex w-96 max-w-[calc(100vw-2rem)] flex-col gap-2 outline-none" />
    </Provider>
  );
}
