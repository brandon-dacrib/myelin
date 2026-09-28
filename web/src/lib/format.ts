/**
 * How the dashboard words numbers the server may or may not have.
 */

/**
 * A count for a tile, or a dash for one the server did not send.
 *
 * Every field of the admin API's `StatisticsOverview` is optional, and the server leaves out
 * what it cannot count (`crates/hs-cli/src/overview.rs`). Absent has to stay visibly different
 * from zero: "Rooms 0" is a fact about the server, and "Rooms —" is a fact about the software.
 */
export function formatCount(value: number | null | undefined): string {
  return value == null ? "—" : value.toLocaleString();
}

/**
 * Uptime as an operator would say it: minutes for the first hour, then hours, then days and
 * hours. A server that has just been set up has been up for minutes, and "0h" reads like a
 * fault.
 */
export function formatUptime(ms: number): string {
  const minutes = Math.floor(ms / 60_000);
  if (minutes < 1) return "under a minute";
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  const days = Math.floor(hours / 24);
  return days > 0 ? `${days}d ${hours % 24}h` : `${hours}h`;
}

/**
 * "bridges", "bridges or federation", "bridges, federation or reports": the things an all-clear
 * could not vouch for, as they read after "It can't check ...".
 */
export function joinWithOr(names: readonly string[]): string {
  if (names.length <= 1) return names.join("");
  return `${names.slice(0, -1).join(", ")} or ${names[names.length - 1]}`;
}

const BYTE_UNITS = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"] as const;

/**
 * A byte count in binary units, one decimal from KiB up ("38.0 GiB", "512 B"), or a dash for a
 * count the server did not send.
 */
export function formatBytes(value: number | null | undefined): string {
  if (value == null) return "—";
  let unit = 0;
  let scaled = value;
  while (Math.abs(scaled) >= 1024 && unit < BYTE_UNITS.length - 1) {
    scaled /= 1024;
    unit += 1;
  }
  return unit === 0 ? `${value} B` : `${scaled.toFixed(1)} ${BYTE_UNITS[unit]}`;
}

/** A duration as an operator would say it: "4s", "2m 10s", "1h 5m", "3d 2h". */
export function formatDuration(ms: number): string {
  if (ms < 1000) return "under a second";
  const seconds = Math.floor(ms / 1000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return seconds % 60 ? `${minutes}m ${seconds % 60}s` : `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return minutes % 60 ? `${hours}h ${minutes % 60}m` : `${hours}h`;
  const days = Math.floor(hours / 24);
  return hours % 24 ? `${days}d ${hours % 24}h` : `${days}d`;
}
