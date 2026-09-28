import { Dialog, DialogContent } from "@/components/ui/dialog/Dialog";
import { SendNoticeForm } from "./SendNoticeForm";

/**
 * "Send notice" from a user's page: the server-notice form with that user as the one
 * recipient. Unmounted when closed, so nothing typed outlives it.
 */
export function SendNoticeDialog({
  userId,
  open,
  onOpenChange,
}: {
  userId: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      {open && (
        <DialogContent
          size="form"
          title="Send a server notice"
          description="A message from the server itself, delivered to their server-notices room. They can read it in any Matrix client."
        >
          <SendNoticeForm fixedRecipient={userId} onDone={() => onOpenChange(false)} />
        </DialogContent>
      )}
    </Dialog>
  );
}
