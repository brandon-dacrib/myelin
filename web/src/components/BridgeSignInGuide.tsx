import { ExternalLink } from "lucide-react";
import type { BridgeType } from "@/api/bridges";
import { CopyableId } from "@/components/CopyableId";
import { signInSteps } from "@/lib/bridge-catalogue";
import { withInlineCode } from "@/lib/inline-code";

/**
 * How a person signs in to a running bridge, in the bridge's own documented steps (the catalogue
 * carries them; `docs.mau.fi` is where they come from). Signing in stays where the bridges put
 * it -- a direct chat with the bot -- and this says exactly what to send there, rather than
 * pretending the interface could do it (PLAN.md section 8.4).
 */
export function BridgeSignInGuide({
  type,
  botId,
  heading = "h2",
}: {
  type: BridgeType | undefined;
  botId: string;
  /** The heading level to render, or `none` where the page already has one for this section. */
  heading?: "h2" | "h3" | "none";
}) {
  const steps = signInSteps(type, botId);
  const Heading = heading === "none" ? null : heading;

  return (
    <section
      aria-labelledby={Heading ? "bridge-sign-in-heading" : undefined}
      aria-label={Heading ? undefined : "Sign in"}
    >
      {Heading && (
        <Heading id="bridge-sign-in-heading" className="text-md font-medium text-text">
          Sign in
        </Heading>
      )}
      <p className="mt-1 text-sm text-text-muted">
        Each person signs in from their own Matrix client, in a direct chat with the bridge bot:{" "}
        <CopyableId value={botId} />
      </p>

      {steps.length > 0 ? (
        <ol className="mt-4 flex flex-col gap-3">
          {steps.map((step, i) => (
            <li key={i} className="flex gap-3 text-sm text-text">
              <span className="flex size-6 shrink-0 items-center justify-center rounded-full bg-accent-muted text-xs font-medium text-accent">
                {i + 1}
              </span>
              <span className="pt-0.5">{withInlineCode(step)}</span>
            </li>
          ))}
        </ol>
      ) : (
        <p className="mt-4 rounded-md border border-border bg-surface p-3 text-sm text-text-muted">
          This appservice was not added through the catalogue, so there is no sign-in guide for it.
          Bridges usually take a <code className="font-identifier">login</code> command in a direct
          chat with their bot, or have a provisioning page of their own.
        </p>
      )}

      {type?.sign_in?.notes && (
        <p className="mt-4 rounded-md border border-info-border bg-info-bg px-3 py-2 text-sm text-info">
          {type.sign_in.notes}
        </p>
      )}

      {type?.docs_url && (
        <a
          href={type.docs_url}
          target="_blank"
          rel="noreferrer"
          className="mt-4 inline-flex items-center gap-1 text-sm text-accent hover:underline"
        >
          <ExternalLink size={14} aria-hidden="true" />
          {type.name} documentation
        </a>
      )}
    </section>
  );
}
