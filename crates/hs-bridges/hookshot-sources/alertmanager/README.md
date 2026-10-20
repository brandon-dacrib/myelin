# Prometheus Alertmanager

Alertmanager's webhook receiver: each notification is a group of alerts, firing or resolved,
and the room gets one notice per notification: `**FIRING** 2 firing (alertname=HighLatency)`
with a line per alert (its name, severity, summary, where it is, and links to its source and
runbook), and `**RESOLVED** ...` when the group resolves.

## In Alertmanager

A receiver with a `webhook_configs` entry, and a route that sends to it:

```yaml
receivers:
  - name: matrix-{{name}}
    webhook_configs:
      - url: '{{url}}'
        send_resolved: true
route:
  receiver: matrix-{{name}}
  group_by: ['alertname', 'namespace']
  group_wait: 30s
  group_interval: 5m
  repeat_interval: 4h
```

(Or a child route with `matchers` for the alerts that belong in this room.) With the
kube-prometheus-stack chart, this is `alertmanager.config` in its values. `send_resolved: true`
is what gets the resolved notice; `repeat_interval` is how often a still-firing group is said
again. `max_alerts` (default 0, unlimited) caps one notification; what is cut off is counted
as `truncatedAlerts` and the notice says `... and N more`.

The URL's last part is the secret; Alertmanager's `http_config` can add a `bearer_token` or
basic auth, but Hookshot's generic webhook does not check them.

## What lands

One notice per notification: the status and counts, the grouping labels, every alert with its
severity, summary (`summary`, else `description`, else `message` from the annotations), the
`instance`/`job`/`namespace`/`pod`/`service` labels it has, and links to its source and runbook.
