import { useEffect, useState } from "react";
import { FileIcon, FileImage, ShieldAlert } from "lucide-react";
import { fetchThumbnail, hasPreview, thumbnailUrl, type MediaItem } from "@/api/media";
import { cn } from "@/lib/cn";

const SIZES = {
  row: { width: 96, height: 96, method: "crop", box: "h-10 w-10", icon: 18 },
  detail: { width: 320, height: 240, method: "scale", box: "h-48 w-full", icon: 32 },
} as const;

/**
 * A preview of one media item, fetched through the authenticated media API with the operator's
 * token, the way a client would see it. Quarantined media and anything that is not an image get
 * an icon instead: a quarantined item is withheld from everyone, and the thumbnail endpoint only
 * makes images.
 */
export function MediaThumbnail({ item, size }: { item: MediaItem; size: "row" | "detail" }) {
  const spec = SIZES[size];
  const wanted = hasPreview(item);
  const url = wanted
    ? thumbnailUrl(item, { width: spec.width, height: spec.height, method: spec.method })
    : null;
  const [loaded, setLoaded] = useState<{ url: string; objectUrl: string | null } | null>(null);

  useEffect(() => {
    if (!url) return;
    const controller = new AbortController();
    let objectUrl: string | null = null;
    fetchThumbnail(url, controller.signal)
      .then((blob) => {
        objectUrl = URL.createObjectURL(blob);
        setLoaded({ url, objectUrl });
      })
      .catch(() => {
        if (!controller.signal.aborted) setLoaded({ url, objectUrl: null });
      });
    return () => {
      controller.abort();
      if (objectUrl) URL.revokeObjectURL(objectUrl);
    };
  }, [url]);

  const src = loaded && loaded.url === url ? loaded.objectUrl : null;
  const label = item.upload_name ?? item.media_id;

  return (
    <div
      className={cn(
        "flex shrink-0 items-center justify-center overflow-hidden rounded-sm border border-border bg-surface-sunken text-text-muted",
        spec.box,
      )}
    >
      {src ? (
        <img
          src={src}
          alt={`Preview of ${label}`}
          className={size === "row" ? "h-full w-full object-cover" : "h-full w-full object-contain"}
        />
      ) : item.quarantined ? (
        <ShieldAlert size={spec.icon} aria-label="Quarantined, no preview" />
      ) : item.content_type?.startsWith("image/") ? (
        <FileImage size={spec.icon} aria-hidden="true" />
      ) : (
        <FileIcon size={spec.icon} aria-hidden="true" />
      )}
    </div>
  );
}
