import { Link } from "@tanstack/react-router";
import { targetRoute, type AuditTarget } from "@/lib/audit";
import { cn } from "@/lib/cn";

/**
 * An audit entry's target as a link to its page, where the interface has one
 * (information-architecture.md #5: "every arrow is a link in both directions"), and as a plain
 * identifier where it does not yet -- a device, an event, a registration token. Same look
 * either way, so a row is not shouting about which resources happen to have pages.
 */
export function ResourceLink({ target, className }: { target: AuditTarget; className?: string }) {
  const route = targetRoute(target);
  const classes = cn("font-identifier", className);
  if (!route) {
    return <span className={classes}>{target.id}</span>;
  }
  const linkClasses = cn(classes, "text-text hover:text-accent hover:underline");
  switch (route.to) {
    case "/users/$userId":
      return (
        <Link to={route.to} params={route.params} className={linkClasses}>
          {target.id}
        </Link>
      );
    case "/rooms/$roomId":
      return (
        <Link to={route.to} params={route.params} className={linkClasses}>
          {target.id}
        </Link>
      );
    case "/bridges/$bridgeId":
      return (
        <Link to={route.to} params={route.params} className={linkClasses}>
          {target.id}
        </Link>
      );
    case "/federation/$serverName":
      return (
        <Link to={route.to} params={route.params} className={linkClasses}>
          {target.id}
        </Link>
      );
    case "/configuration/$section":
      return (
        <Link to={route.to} params={route.params} className={linkClasses}>
          {target.id}
        </Link>
      );
  }
}
