/**
 * The mock's media: local uploads and cached remote copies, in the admin API's real
 * `MediaItem` shape. Mutable (quarantine, protect and the deletions change it), so tests call
 * {@link resetMedia} between runs, as `src/test/setup.ts` does.
 */
import type { MediaItem } from "@/api/media";

function seed(): MediaItem[] {
  return [
    {
      server_name: "example.org",
      media_id: "vacationPhotoAbc123",
      origin: "local",
      uploader: "@alice:example.org",
      upload_name: "vacation.jpg",
      content_type: "image/jpeg",
      size_bytes: 2_400_000,
      created_at: "2026-08-01T10:00:00.000Z",
      last_accessed_at: "2026-09-17T09:00:00.000Z",
      quarantined: false,
      protected: false,
    },
    {
      server_name: "example.org",
      media_id: "notMalwareDef456",
      origin: "local",
      uploader: "@mallory:example.org",
      upload_name: "definitely-not-malware.exe",
      content_type: "application/octet-stream",
      size_bytes: 900_000,
      created_at: "2026-09-10T02:00:00.000Z",
      last_accessed_at: null,
      quarantined: true,
      protected: false,
    },
    {
      server_name: "example.org",
      media_id: "teamLogoGhi789",
      origin: "local",
      uploader: "@bob:example.org",
      upload_name: "team-logo.png",
      content_type: "image/png",
      size_bytes: 48_000,
      created_at: "2026-03-02T08:30:00.000Z",
      last_accessed_at: "2026-09-18T07:00:00.000Z",
      quarantined: false,
      protected: true,
    },
    {
      server_name: "example.org",
      media_id: "oldReportJkl012",
      origin: "local",
      uploader: "@carol:example.org",
      upload_name: "q1-report.pdf",
      content_type: "application/pdf",
      size_bytes: 5_600_000,
      created_at: "2026-01-15T14:00:00.000Z",
      last_accessed_at: "2026-02-01T09:00:00.000Z",
      quarantined: false,
      protected: false,
    },
    {
      server_name: "matrix.org",
      media_id: "avatarMno345",
      origin: "remote",
      uploader: null,
      upload_name: "avatar.png",
      content_type: "image/png",
      size_bytes: 40_000,
      created_at: "2026-07-15T12:00:00.000Z",
      last_accessed_at: "2026-08-02T00:00:00.000Z",
      quarantined: false,
      protected: false,
    },
    {
      server_name: "remote.example",
      media_id: "memePqr678",
      origin: "remote",
      uploader: null,
      upload_name: null,
      content_type: "image/gif",
      size_bytes: 1_200_000,
      created_at: "2026-09-01T18:00:00.000Z",
      last_accessed_at: null,
      quarantined: false,
      protected: false,
    },
  ];
}

export let mediaItems: MediaItem[] = seed();

export function resetMedia(): void {
  mediaItems = seed();
}

export function findMedia(serverName: string, mediaId: string): MediaItem | undefined {
  return mediaItems.find((m) => m.server_name === serverName && m.media_id === mediaId);
}

export function removeMedia(item: MediaItem): void {
  mediaItems = mediaItems.filter((m) => m !== item);
}

/** When `item` was last used: its last access, or its creation if never served. */
export function lastUsed(item: MediaItem): string {
  return item.last_accessed_at ?? item.created_at;
}

/** The server's list semantics: filters, case-insensitive search, one sort field. */
export function listMedia(params: URLSearchParams): MediaItem[] {
  const q = params.get("q")?.trim().toLowerCase();
  const origin = params.get("origin");
  const quarantined = params.get("quarantined");
  const isProtected = params.get("protected");
  const uploader = params.get("uploader");
  const sort = params.get("sort") ?? "-created_at";
  const descending = sort.startsWith("-");
  const field = descending ? sort.slice(1) : sort;
  const rows = mediaItems.filter(
    (m) =>
      (!origin || m.origin === origin) &&
      (quarantined === null || String(m.quarantined) === quarantined) &&
      (isProtected === null || String(m.protected) === isProtected) &&
      (!uploader || m.uploader === uploader) &&
      (!q ||
        [m.media_id, m.server_name, m.upload_name, m.uploader, m.content_type].some((f) =>
          f?.toLowerCase().includes(q),
        )),
  );
  const key = (m: MediaItem): string | number =>
    field === "size_bytes"
      ? m.size_bytes
      : field === "last_accessed_at"
        ? lastUsed(m)
        : field === "media_id"
          ? m.media_id
          : m.created_at;
  rows.sort((a, b) => {
    const [x, y] = [key(a), key(b)];
    const order = x < y ? -1 : x > y ? 1 : 0;
    return descending ? -order : order;
  });
  return rows;
}

/**
 * A stand-in thumbnail: a tile in a colour picked from the media id, with the file's initial,
 * so a mock page shows distinct previews without shipping images.
 */
export function thumbnailSvg(mediaId: string, width: number, height: number): string {
  const hue = [...mediaId].reduce((h, c) => (h * 31 + c.charCodeAt(0)) % 360, 7);
  const item = mediaItems.find((m) => m.media_id === mediaId);
  const letter = (item?.upload_name ?? mediaId).charAt(0).toUpperCase();
  return (
    `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}">` +
    `<rect width="100%" height="100%" fill="hsl(${hue} 55% 55%)"/>` +
    `<text x="50%" y="54%" dominant-baseline="middle" text-anchor="middle" font-family="sans-serif" ` +
    `font-size="${Math.round(Math.min(width, height) / 2)}" fill="white">${letter}</text></svg>`
  );
}
