/**
 * Whether a person has signed in to their own bridge, on the offering page's list of people's
 * bridges: the bridge is asked about its owner (`GET /appservices/{id}/logins`, a mautrix
 * bridge's `whoami`; the server keeps each answer 30 seconds). One request per ready bridge.
 */
import { Link } from "@tanstack/react-router";
import { useAppserviceLogins, type BridgeInstance } from "@/api/bridges";
import { Badge } from "@/components/ui/badge/Badge";

export function InstanceSignIn({ instance }: { instance: BridgeInstance }) {
  const ready = instance.state === "ready" && Boolean(instance.appservice_id);
  const { data, isLoading, isError } = useAppserviceLogins(
    ready ? (instance.appservice_id ?? undefined) : undefined,
    undefined,
  );

  if (!ready) {
    return <span className="text-text-muted">Once it is ready</span>;
  }
  if (isLoading) return <span className="text-text-muted">Asking the bridge…</span>;
  if (isError || !data) {
    return <span className="text-text-muted">Could not ask</span>;
  }
  if (!data.supported) {
    return (
      <span className="flex max-w-[16rem] flex-col gap-0.5">
        <span className="text-text-muted">Not reported</span>
        <span className="text-xs text-text-faint">
          This bridge keeps who has signed in itself.{" "}
          {instance.appservice_id && (
            <Link
              to="/bridges/$bridgeId"
              params={{ bridgeId: instance.appservice_id }}
              hash="sign-in"
              className="text-accent underline hover:no-underline"
            >
              Why
            </Link>
          )}
        </span>
      </span>
    );
  }
  if (data.error) {
    return (
      <span className="flex max-w-[16rem] flex-col gap-0.5">
        <Badge status="warning">Bridge did not answer</Badge>
        <span className="text-xs text-text-faint">{data.error.detail}</span>
      </span>
    );
  }
  if (data.signed_in && data.logins.length > 0) {
    return (
      <ul className="flex flex-col gap-0.5">
        {data.logins.map((login) => (
          <li key={login.remote_id} className="flex flex-wrap items-center gap-1.5">
            <span>
              <span className="text-text-muted">As </span>
              <span className="font-identifier">{login.remote_name ?? login.remote_id}</span>
            </span>
            {login.state !== "connected" && (
              <Badge status="warning">{login.state.replaceAll("_", " ")}</Badge>
            )}
          </li>
        ))}
      </ul>
    );
  }
  return (
    <span className="flex flex-col gap-0.5">
      <span>Not signed in</span>
      <span className="text-xs text-text-faint">Their bridge waits for them to link it.</span>
    </span>
  );
}
