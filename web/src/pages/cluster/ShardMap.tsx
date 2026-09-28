import type { CSSProperties } from "react";
import type { Replica, Shard } from "@/api/cluster";
import {
  countByOwner,
  describeOwnership,
  groupByKind,
  ownerColours,
  plural,
  SHARD_KIND_LABELS,
} from "@/lib/cluster";

/** A cell nobody owns: an outline only. */
const UNOWNED_STYLE: CSSProperties = {
  background: "transparent",
  boxShadow: "inset 0 0 0 1px var(--color-border-strong)",
};

/** A cell between owners (released during a handoff): warning stripes. */
const RELEASED_STYLE: CSSProperties = {
  background:
    "repeating-linear-gradient(135deg, var(--color-warning) 0 2px, var(--color-warning-bg) 2px 4px)",
};

function cellStyle(shard: Shard, colours: Map<string, string>): CSSProperties {
  if (shard.owner) return { background: colours.get(shard.owner) };
  return shard.state === "released" ? RELEASED_STYLE : UNOWNED_STYLE;
}

function cellTitle(shard: Shard): string {
  if (shard.owner) return `${shard.id}: owned by ${shard.owner} (epoch ${shard.epoch ?? "?"})`;
  return shard.state === "released" ? `${shard.id}: being handed off` : `${shard.id}: unowned`;
}

/**
 * Every shard as a small square coloured by its owner, grouped by kind, with a legend that
 * counts. The squares are for the eye (each has a title for a pointer); each kind's grid is one
 * image whose name is the ownership in words, and the table view lists every shard for anyone
 * who wants them one by one.
 */
export function ShardMap({ shards, replicas }: { shards: Shard[]; replicas: Replica[] }) {
  const colours = ownerColours(replicas, shards);
  const counts = countByOwner(shards);
  const released = shards.filter((s) => !s.owner && s.state === "released").length;
  const unowned = shards.filter((s) => !s.owner && s.state !== "released").length;
  const thisReplica = replicas.find((r) => r.this_replica)?.id;

  return (
    <div className="flex flex-col gap-5 rounded-md border border-border bg-surface p-4">
      <ul aria-label="Owners" className="flex flex-wrap gap-x-5 gap-y-2 text-sm">
        {[...colours].map(([owner, colour]) => (
          <li key={owner} className="flex items-center gap-2">
            <span
              aria-hidden="true"
              className="size-3 rounded-[2px]"
              style={{ background: colour }}
            />
            <span className="font-identifier text-text">{owner}</span>
            {owner === thisReplica && <span className="text-text-muted">(this replica)</span>}
            <span className="text-text-muted tabular-nums">
              {plural(counts.get(owner) ?? 0, "shard", "shards")}
            </span>
          </li>
        ))}
        {unowned > 0 && (
          <li className="flex items-center gap-2">
            <span aria-hidden="true" className="size-3 rounded-[2px]" style={UNOWNED_STYLE} />
            <span className="text-text">Unowned</span>
            <span className="text-text-muted tabular-nums">{unowned.toLocaleString()}</span>
          </li>
        )}
        {released > 0 && (
          <li className="flex items-center gap-2">
            <span aria-hidden="true" className="size-3 rounded-[2px]" style={RELEASED_STYLE} />
            <span className="text-text">Being handed off</span>
            <span className="text-text-muted tabular-nums">{released.toLocaleString()}</span>
          </li>
        )}
      </ul>

      {groupByKind(shards).map(({ kind, shards: ofKind }) => {
        const label = SHARD_KIND_LABELS[kind];
        const headingId = `shard-map-${kind}`;
        return (
          <section key={kind} aria-labelledby={headingId} className="flex flex-col gap-2">
            <h3 id={headingId} className="flex items-baseline gap-2 text-sm font-medium text-text">
              {label.plural}
              <span className="font-normal text-text-muted tabular-nums">
                {plural(ofKind.length, "shard", "shards")}
              </span>
            </h3>
            <div
              role="img"
              aria-label={describeOwnership(label.singular, ofKind)}
              className="flex flex-wrap gap-[3px]"
            >
              {ofKind.map((shard) => (
                <span
                  key={shard.id}
                  title={cellTitle(shard)}
                  data-owner={shard.owner ?? ""}
                  className="size-3 rounded-[2px] transition-colors duration-500"
                  style={cellStyle(shard, colours)}
                />
              ))}
            </div>
          </section>
        );
      })}
    </div>
  );
}
