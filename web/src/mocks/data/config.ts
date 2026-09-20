/**
 * Configuration fixtures: the JSON Schema `GET /config/schema` answers, the
 * effective values behind `GET /config`, and the origin of every setting
 * something set.
 *
 * The schema mirrors `hs_config::Config` as `schemars` emits it — field
 * names, types, doc comments and `serde(default = ...)` values all taken
 * from `crates/hs-config/src/*.rs` and the generated `docs/config.md`, so
 * the forms this drives in `npm run dev:mock` are the forms a real server
 * produces. It is not the whole struct (`auth.oidc_providers`'s inner shape
 * and the `mas_delegation` block are elided), but every *kind* of setting is
 * here: booleans, enums, integers, floats, durations, byte sizes, scalar
 * arrays, arrays of objects, nested groups, secrets, a bootstrap-only
 * section and a setting pinned by the environment.
 */
import type { ConfigOrigin, JsonSchemaNode, JsonValue } from "@/api/config-schema";
import type { AuditEntry } from "@/api/config";

const secretString: JsonSchemaNode = {
  type: "string",
  "x-secret": true,
  description:
    "An inline secret value. Prefer the matching *_file key to avoid putting secrets in the config file.",
};

const defs: Record<string, JsonSchemaNode> = {
  SecretString: secretString,
  Duration: {
    anyOf: [{ type: "string" }, { type: "integer", minimum: 0 }],
    description:
      "A duration: a string of <number><unit> groups (ms, s, m, h, d, w, y), or an integer number of milliseconds.",
  },
  ByteSize: {
    anyOf: [{ type: "string" }, { type: "integer", minimum: 0 }],
    description:
      "A byte size: a number with an optional unit (K, M, G, T with 1024 multipliers; KiB/MiB/GiB; KB/MB/GB with 1000 multipliers), or an integer byte count.",
  },
  LogLevel: {
    type: "string",
    enum: ["trace", "debug", "info", "warn", "error"],
    description: "Log level.",
  },
  RateLimitBucket: {
    type: "object",
    description:
      "A token-bucket rate limit: `burst_count` tokens refilling at `per_second` tokens/second.",
    required: ["per_second", "burst_count"],
    properties: {
      per_second: {
        type: "number",
        minimum: 0,
        description: "Tokens added per second. Must be greater than zero.",
      },
      burst_count: {
        type: "integer",
        minimum: 1,
        description: "Bucket capacity: how many requests may arrive at once before limiting bites.",
      },
    },
  },
  Listener: {
    type: "object",
    description: "One bound socket.",
    required: ["port"],
    properties: {
      bind_addresses: { type: "array", items: { type: "string" } },
      port: { type: "integer", minimum: 1, maximum: 65535 },
      tls: { anyOf: [{ type: "object" }, { type: "null" }] },
      resources: {
        type: "array",
        items: {
          type: "string",
          enum: ["client", "federation", "media", "health", "metrics", "admin"],
        },
      },
      x_forwarded: { type: "boolean" },
    },
  },
  ThumbnailSize: {
    type: "object",
    required: ["width", "height", "method"],
    properties: {
      width: { type: "integer", minimum: 1 },
      height: { type: "integer", minimum: 1 },
      method: { type: "string", enum: ["crop", "scale"] },
    },
  },
  MetricsConfig: {
    type: "object",
    description: "Prometheus metrics.",
    properties: {
      enabled: {
        type: "boolean",
        default: false,
        description: "Serve /metrics on a listener with the metrics resource.",
      },
      synapse_compat_names: {
        type: "boolean",
        default: true,
        description:
          "Additionally export Synapse-named metrics (synapse_*) alongside the native hs_* ones, for dashboards built against Synapse.",
      },
    },
  },
  TracingConfig: {
    type: "object",
    description: "OpenTelemetry distributed tracing.",
    properties: {
      enabled: { type: "boolean", default: false, description: "Emit spans at all." },
      otlp_endpoint: {
        anyOf: [{ type: "string" }, { type: "null" }],
        description: "OTLP collector endpoint. Required when enabled is true.",
      },
      sample_ratio: {
        type: "number",
        default: 0.05,
        minimum: 0,
        maximum: 1,
        description: "Fraction of traces sampled, 0.0 to 1.0.",
      },
    },
  },
  LoggingConfig: {
    type: "object",
    description: "Structured logging.",
    properties: {
      level: { $ref: "#/$defs/LogLevel", default: "info", description: "Minimum level emitted." },
      json: {
        type: "boolean",
        default: false,
        description:
          "Emit JSON lines instead of human-readable text. Corresponds to Synapse's log_config handler choice, simplified to a boolean since this server has one structured schema rather than arbitrary logging.config.dictConfig handlers.",
      },
    },
  },
  SentryConfig: {
    type: "object",
    description: "Sentry error reporting; absent disables it.",
    properties: {
      dsn: { $ref: "#/$defs/SecretString", description: "Inline DSN. Prefer dsn_file." },
      dsn_file: {
        anyOf: [{ type: "string" }, { type: "null" }],
        description: "Path to a file containing the DSN.",
      },
      environment: {
        type: "string",
        default: "production",
        description: "Environment tag attached to events.",
      },
    },
  },
  PasswordPolicy: {
    type: "object",
    description: "Complexity requirements. Corresponds to Synapse's password_config.policy.",
    properties: {
      minimum_length: { type: "integer", default: 8, minimum: 1 },
      require_digit: { type: "boolean", default: false },
      require_symbol: { type: "boolean", default: false },
      require_uppercase: { type: "boolean", default: false },
      require_lowercase: { type: "boolean", default: false },
    },
  },
  PasswordConfig: {
    type: "object",
    description: "Password login and policy. Corresponds to Synapse's password_config.",
    properties: {
      enabled: {
        type: "boolean",
        default: true,
        description:
          "Allow password login at all. Corresponds to Synapse's password_config.enabled.",
      },
      pepper: {
        $ref: "#/$defs/SecretString",
        description:
          "Inline pepper mixed into password hashes. Prefer pepper_file. Corresponds to Synapse's password_config.pepper.",
      },
      pepper_file: {
        anyOf: [{ type: "string" }, { type: "null" }],
        description: "Path to a file containing the pepper.",
      },
      policy: { $ref: "#/$defs/PasswordPolicy" },
    },
  },
  MeshConfig: {
    type: "object",
    description: "Internal mesh transport between replicas.",
    properties: {
      bind_address: { type: "string", default: "0.0.0.0:9099" },
      advertise_address: { anyOf: [{ type: "string" }, { type: "null" }] },
      tls_enabled: {
        type: "boolean",
        default: false,
        description: "Require mutual TLS between replicas.",
      },
    },
  },
  RemoteMediaRetention: {
    type: "object",
    description:
      "How long to keep cached copies of remote media. Absent means keep forever. Corresponds to Synapse's media_retention.remote_media_lifetime.",
    properties: {
      lifetime: { $ref: "#/$defs/Duration" },
    },
  },
  EmbeddedStorage: {
    type: "object",
    description: "Fjall, embedded in-process. Single node, the small-ARM-host mode, and tests.",
    required: ["backend"],
    properties: {
      backend: { type: "string", const: "embedded" },
      data_dir: {
        type: "string",
        default: "./data",
        description: "Directory holding the embedded database files.",
      },
    },
  },
  PostgresStorage: {
    type: "object",
    description: "PostgreSQL. The default clustered backend.",
    required: ["backend", "host", "database", "user"],
    properties: {
      backend: { type: "string", const: "postgres" },
      host: { type: "string", description: "Database host." },
      port: { type: "integer", default: 5432, description: "Database port." },
      database: { type: "string", description: "Database name." },
      user: { type: "string", description: "Connecting role." },
      password: { $ref: "#/$defs/SecretString" },
      pool_size: {
        type: "integer",
        default: 10,
        description:
          "Connection pool size. Corresponds to Synapse's database.args.cp_max. Accepted but not yet threaded through.",
      },
      tls: {
        type: "boolean",
        default: false,
        description: "Require TLS for the connection. Accepted but not yet honoured.",
      },
    },
  },
  SlateDbStorage: {
    type: "object",
    description: "SlateDB on object storage. Diskless clusters.",
    required: ["backend", "bucket_url"],
    properties: {
      backend: { type: "string", const: "slatedb" },
      bucket_url: {
        type: "string",
        description:
          "The object store URL (s3://bucket/prefix, gs://..., az://...), passed to the object_store crate.",
      },
      shard_count: {
        type: "integer",
        default: 256,
        description:
          "Number of virtual storage shards. Fixed at cluster creation; see docs/rfcs/0001-cluster-ownership.md.",
      },
      lease_duration: { $ref: "#/$defs/Duration" },
    },
  },
};

const DEFAULT_IP_BLOCKLIST: JsonValue = [
  "127.0.0.0/8",
  "10.0.0.0/8",
  "172.16.0.0/12",
  "192.168.0.0/16",
  "100.64.0.0/10",
  "169.254.0.0/16",
  "::1/128",
  "fe80::/10",
  "fc00::/7",
];

const DEFAULT_THUMBNAIL_SIZES: JsonValue = [
  { width: 32, height: 32, method: "crop" },
  { width: 96, height: 96, method: "crop" },
  { width: 320, height: 240, method: "scale" },
  { width: 640, height: 480, method: "scale" },
  { width: 800, height: 600, method: "scale" },
];

function bucket(description: string): JsonSchemaNode {
  return { $ref: "#/$defs/RateLimitBucket", description };
}

const properties: Record<string, JsonSchemaNode> = {
  server: {
    type: "object",
    description:
      "Server identity: name, public URL, signing keys. Restart required to change: server_name is embedded in every user ID, room ID and event this process has ever produced.",
    required: ["server_name"],
    properties: {
      server_name: {
        type: "string",
        description:
          "The domain in @user:server_name, room aliases and event origins. Changing it after any room exists is not supported by any Matrix homeserver, including this one.",
      },
      public_baseurl: {
        anyOf: [{ type: "string" }, { type: "null" }],
        description:
          "The externally reachable base URL for clients, if different from https://{server_name}.",
      },
      well_known_server: {
        anyOf: [{ type: "string" }, { type: "null" }],
        description:
          "The host[:port] a remote server should connect to for federation, advertised at GET /.well-known/matrix/server, when that differs from server_name. Absent means the route is not served at all.",
      },
      signing_key_path: {
        type: "string",
        default: "./signing-keys",
        description:
          "Directory holding this server's Ed25519 signing keys. A directory rather than a file because multiple active keys are normal during rotation.",
      },
      admin_contact: {
        anyOf: [{ type: "string" }, { type: "null" }],
        description:
          "Contact address advertised for abuse reports and shown to operators of other servers.",
      },
      report_stats: {
        type: "boolean",
        default: false,
        description: "Whether this server opts in to the anonymised statistics-reporting endpoint.",
      },
    },
  },

  listeners: {
    type: "object",
    description:
      "All configured listeners. Restart required to change: sockets are bound once at startup.",
    properties: {
      listeners: {
        type: "array",
        items: { $ref: "#/$defs/Listener" },
        description: "One entry per bound socket.",
        default: [
          {
            bind_addresses: ["::"],
            port: 8008,
            tls: null,
            resources: ["client", "federation", "media", "health"],
            x_forwarded: false,
          },
        ],
      },
    },
  },

  storage: {
    description:
      "Which storage backend this replica uses, and its backend-specific settings. Restart required to change.",
    oneOf: [
      { $ref: "#/$defs/EmbeddedStorage" },
      { $ref: "#/$defs/PostgresStorage" },
      { $ref: "#/$defs/SlateDbStorage" },
    ],
  },

  media: {
    type: "object",
    description: "Media repository settings.",
    properties: {
      max_upload_size: {
        $ref: "#/$defs/ByteSize",
        description: "Largest single upload accepted. Corresponds to Synapse's max_upload_size.",
      },
      thumbnail_sizes: {
        type: "array",
        items: { $ref: "#/$defs/ThumbnailSize" },
        default: DEFAULT_THUMBNAIL_SIZES,
        description:
          "Thumbnail sizes to pre-generate/serve on demand. Corresponds to Synapse's thumbnail_sizes.",
      },
      url_preview_enabled: {
        type: "boolean",
        default: false,
        description: "Enable GET /_matrix/media/*/preview_url.",
      },
      url_preview_ip_range_blocklist: {
        type: "array",
        items: { type: "string" },
        default: DEFAULT_IP_BLOCKLIST,
        description:
          "IP ranges URL previews must not fetch from (SSRF protection). Corresponds to Synapse's url_preview_ip_range_blacklist.",
      },
      url_preview_timeout: { $ref: "#/$defs/Duration", description: "Fetch timeout per preview." },
      url_preview_max_fetch_size: {
        $ref: "#/$defs/ByteSize",
        description: "Stop fetching a previewed page after this many bytes.",
      },
      url_preview_cache_lifetime: {
        $ref: "#/$defs/Duration",
        description: "How long a generated preview is reused.",
      },
      allow_legacy_unauthenticated_media: {
        type: "boolean",
        default: true,
        description:
          "Serve the pre-authentication-media (legacy, unauthenticated) endpoints alongside the authenticated ones.",
      },
      remote_media_retention: { $ref: "#/$defs/RemoteMediaRetention" },
    },
  },

  federation: {
    type: "object",
    description: "Federation reachability and transport policy.",
    properties: {
      enabled: {
        type: "boolean",
        default: true,
        description: "Master switch for outbound and inbound federation traffic.",
      },
      domain_allowlist: {
        anyOf: [{ type: "array", items: { type: "string" } }, { type: "null" }],
        description:
          "If set, federation traffic is restricted to exactly these server names. Corresponds to Synapse's federation_domain_whitelist.",
      },
      ip_range_blocklist: {
        type: "array",
        items: { type: "string" },
        default: DEFAULT_IP_BLOCKLIST,
        description:
          "IP ranges (CIDR) federation requests must not be sent to. Corresponds to Synapse's federation_ip_range_blacklist.",
      },
      ip_range_allowlist: {
        type: "array",
        items: { type: "string" },
        default: [],
        description:
          "IP ranges exempted from ip_range_blocklist (for federating with a deliberately private deployment).",
      },
      verify_certificates: {
        type: "boolean",
        default: true,
        description:
          "Verify TLS certificates on outbound federation requests. Leave this true in production: setting it false makes outbound federation TLS accept any certificate, which is trivially machine-in-the-middled. It exists for test deployments and conformance harnesses that terminate TLS with a certificate this server has no other way to trust yet. Prefer custom_ca_certificates instead, which trusts exactly the named CA rather than every certificate on the internet.",
      },
      custom_ca_certificates: {
        type: "array",
        items: { type: "string" },
        default: [],
        description:
          "Paths to additional PEM-encoded CA certificate files trusted for outbound federation TLS, on top of (never instead of) the public root CAs. This is the answer to 'how do I federate with a server whose certificate was issued by a private CA' without resorting to verify_certificates.",
      },
      trust_os_root_store: {
        type: "boolean",
        default: false,
        description:
          "Whether outbound federation TLS also trusts whatever CA store the operating system trusts. Defaults to false: the OS store can be broadened by anyone with root on the machine, for reasons having nothing to do with running a homeserver.",
      },
      client_timeout: {
        $ref: "#/$defs/Duration",
        description: "Per-request timeout for outbound federation HTTP.",
      },
      max_retry_backoff: {
        $ref: "#/$defs/Duration",
        description: "Ceiling on the exponential backoff applied to a failing destination.",
      },
      allow_public_rooms_over_federation: {
        type: "boolean",
        default: false,
        description: "Advertise this server's public room directory over federation.",
      },
      allow_device_name_lookup_over_federation: {
        type: "boolean",
        default: false,
        description:
          "Answer remote servers' /_matrix/federation/*/user/devices/* queries for device display names.",
      },
    },
  },

  rate_limits: {
    type: "object",
    description: "All configured rate-limit buckets.",
    properties: {
      enabled: {
        type: "boolean",
        default: true,
        description:
          "Master switch; when false, no limiter runs (tests and benchmarking only — never recommended in production).",
      },
      message: bucket("Sending a message into a room."),
      registration: bucket("Creating an account."),
      login: bucket("Password and token login attempts."),
      joins_local: bucket("Joining a room this server already participates in."),
      joins_remote: bucket("Joining a room over federation, which is far more expensive."),
      admin_redaction: bucket("Redactions issued by a room moderator or server admin."),
      federation: bucket("Inbound federation transactions from one remote server."),
      third_party_id_validation: bucket("Email and phone verification requests."),
    },
  },

  auth: {
    type: "object",
    description: "Authentication, session and registration settings.",
    properties: {
      enable_registration: {
        type: "boolean",
        default: false,
        description: "Allow POST /register. Corresponds to Synapse's enable_registration.",
      },
      registration_shared_secret: {
        $ref: "#/$defs/SecretString",
        description:
          "Shared secret accepted by the shared-secret registration endpoint, which the hs register CLI uses.",
      },
      registration_shared_secret_file: {
        anyOf: [{ type: "string" }, { type: "null" }],
        description: "Path to a file containing the shared-secret-registration secret.",
      },
      enable_legacy_login: {
        type: "boolean",
        default: true,
        description:
          "Serve the legacy /login and user-interactive-auth flows in addition to the native OAuth 2.0 issuer. Needed for older clients, bridges and m.login.application_service.",
      },
      session_secret: {
        $ref: "#/$defs/SecretString",
        description: "Key the session cookies and OAuth state are signed with.",
      },
      session_secret_file: {
        anyOf: [{ type: "string" }, { type: "null" }],
        description: "Path to a file containing the session-signing secret.",
      },
      access_token_lifetime: {
        $ref: "#/$defs/Duration",
        description: "How long an access token stays valid before it must be refreshed.",
      },
      refresh_token_lifetime: {
        $ref: "#/$defs/Duration",
        default: "1y",
        description: "Refresh token lifetime; absent means refresh tokens do not expire.",
      },
      password: { $ref: "#/$defs/PasswordConfig" },
      oidc_providers: {
        type: "array",
        items: { type: "object" },
        default: [],
        description: "Upstream OIDC providers.",
      },
    },
  },

  appservices: {
    type: "object",
    description: "Appservice (bridge) registry bootstrap settings.",
    properties: {
      enabled: {
        type: "boolean",
        default: true,
        description: "Master switch for appservice transaction delivery.",
      },
      registration_files: {
        type: "array",
        items: { type: "string" },
        default: [],
        description:
          "Static registration YAML files loaded at startup (and on reload). Appservices registered later through the admin API do not need an entry here.",
      },
      tracking_failure_threshold: {
        type: "integer",
        default: 50,
        minimum: 1,
        description:
          "Consecutive delivery failures to one appservice before it is marked unhealthy and moved to backlog-only delivery.",
      },
    },
  },

  telemetry: {
    type: "object",
    description: "Telemetry: metrics, tracing, logging and error reporting.",
    properties: {
      metrics: { $ref: "#/$defs/MetricsConfig" },
      tracing: { $ref: "#/$defs/TracingConfig" },
      logging: { $ref: "#/$defs/LoggingConfig" },
      sentry: { $ref: "#/$defs/SentryConfig" },
    },
  },

  cluster: {
    type: "object",
    description: "Cluster topology and ownership tuning.",
    properties: {
      single_node: {
        type: "boolean",
        default: true,
        description:
          "Run as a single replica owning everything, with the ownership manager inert and no mesh listener. Corresponds to hs serve --single-node.",
      },
      room_shards: {
        type: "integer",
        default: 256,
        minimum: 1,
        description: "Number of room ownership shards. Fixed at cluster creation.",
      },
      user_shards: {
        type: "integer",
        default: 256,
        minimum: 1,
        description: "Number of user-session ownership shards. Fixed at cluster creation.",
      },
      mesh: { $ref: "#/$defs/MeshConfig" },
      heartbeat_interval: {
        $ref: "#/$defs/Duration",
        description: "How often a replica renews its ownership leases.",
      },
      lease_ttl: {
        $ref: "#/$defs/Duration",
        description:
          "How long a lease survives without a heartbeat before another replica may take it.",
      },
    },
  },
};

/**
 * Where each setting's value comes from, keyed by RFC 6901 JSON Pointer the
 * way `hs_config::document::origins` produces them. Settings nothing sets are
 * absent: their origin is `default`.
 */
export const configOrigins: Record<string, ConfigOrigin> = {
  "/server/server_name": "environment",
  "/server/public_baseurl": "database",
  "/server/admin_contact": "database",
  "/server/signing_key_path": "file",
  "/listeners/listeners": "file",
  "/storage/backend": "file",
  "/storage/data_dir": "file",
  "/media/max_upload_size": "database",
  "/media/url_preview_enabled": "database",
  "/federation/custom_ca_certificates": "database",
  "/federation/client_timeout": "database",
  "/rate_limits/message/per_second": "database",
  "/rate_limits/message/burst_count": "database",
  "/rate_limits/login/burst_count": "database",
  "/auth/enable_registration": "database",
  "/auth/registration_shared_secret": "database",
  "/auth/session_secret": "file",
  "/auth/password/policy/minimum_length": "database",
  "/appservices/registration_files": "file",
  "/telemetry/metrics/enabled": "database",
  "/telemetry/logging/level": "environment",
  "/cluster/single_node": "file",
};

/** The settings served redacted, whether or not anything has set them. */
const SECRET_POINTERS = [
  "/auth/registration_shared_secret",
  "/auth/session_secret",
  "/auth/password/pepper",
  "/telemetry/sentry/dsn",
  "/storage/password",
];

const RELOADABLE = new Set(["rate_limits", "federation", "telemetry", "appservices"]);
const BOOTSTRAP = new Set(["storage"]);

const sectionInfos = [
  "server",
  "listeners",
  "storage",
  "media",
  "federation",
  "rate_limits",
  "auth",
  "appservices",
  "telemetry",
  "cluster",
].map((name) => ({
  name,
  reloadable: RELOADABLE.has(name),
  bootstrap: BOOTSTRAP.has(name),
  source: sectionSource(name),
}));

/**
 * One row per setting the server has something to say about.
 *
 * A real server lists every setting in the schema; this lists the ones whose
 * answer is not the boring one (origin `default`, not a secret, editable), so
 * the fixture stays readable. The interface treats a setting absent from this
 * list exactly as the boring answer, which is the same thing.
 */
const settingInfos = [...new Set([...Object.keys(configOrigins), ...SECRET_POINTERS])]
  .sort()
  .map((pointer) => {
    const section = pointer.split("/")[1];
    const origin = configOrigins[pointer] ?? "default";
    return {
      pointer,
      section,
      origin,
      secret: SECRET_POINTERS.includes(pointer),
      reloadable: RELOADABLE.has(section),
      // The server's own answer to "would config.update take this?". False
      // for a bootstrap section and for anything an HS__ variable pins.
      editable: origin !== "environment" && !BOOTSTRAP.has(section),
    };
  });

/** The document `GET /config/schema` answers (`components.schemas.ConfigSchema`). */
export const configSchemaDocument = {
  schema: {
    $schema: "https://json-schema.org/draft/2020-12/schema",
    title: "Config",
    description: "The complete native configuration.",
    type: "object",
    properties,
    $defs: defs,
  },
  sections: sectionInfos,
  settings: settingInfos,
  revision: 7,
};

/**
 * Effective values per section: what every layer merged together comes to.
 * Mutated in place by `PATCH /config/{section}`, the same way the real store
 * is, so the mock survives a save and a reload of the page.
 */
export const configValues: Record<string, Record<string, JsonValue>> = {
  server: {
    server_name: "example.org",
    public_baseurl: "https://matrix.example.org",
    well_known_server: null,
    signing_key_path: "/var/lib/myelin/signing-keys",
    admin_contact: "mailto:abuse@example.org",
    report_stats: false,
  },
  listeners: {
    listeners: [
      {
        bind_addresses: ["::"],
        port: 8008,
        tls: null,
        resources: ["client", "federation", "media", "health", "admin"],
        x_forwarded: true,
      },
    ],
  },
  storage: {
    backend: "embedded",
    data_dir: "/var/lib/myelin/data",
  },
  media: {
    max_upload_size: "100M",
    thumbnail_sizes: DEFAULT_THUMBNAIL_SIZES,
    url_preview_enabled: true,
    url_preview_ip_range_blocklist: DEFAULT_IP_BLOCKLIST,
    url_preview_timeout: "10s",
    url_preview_max_fetch_size: "10M",
    url_preview_cache_lifetime: "1h",
    allow_legacy_unauthenticated_media: true,
    remote_media_retention: { lifetime: "90d" },
  },
  federation: {
    enabled: true,
    domain_allowlist: null,
    ip_range_blocklist: DEFAULT_IP_BLOCKLIST,
    ip_range_allowlist: [],
    verify_certificates: true,
    custom_ca_certificates: ["/etc/myelin/ca/internal-root.pem"],
    trust_os_root_store: false,
    client_timeout: "45s",
    max_retry_backoff: "1d",
    allow_public_rooms_over_federation: false,
    allow_device_name_lookup_over_federation: false,
  },
  rate_limits: {
    enabled: true,
    message: { per_second: 0.5, burst_count: 25 },
    registration: { per_second: 0.17, burst_count: 3 },
    login: { per_second: 0.17, burst_count: 5 },
    joins_local: { per_second: 0.1, burst_count: 10 },
    joins_remote: { per_second: 0.01, burst_count: 10 },
    admin_redaction: { per_second: 1, burst_count: 50 },
    federation: { per_second: 10, burst_count: 100 },
    third_party_id_validation: { per_second: 0.003, burst_count: 5 },
  },
  auth: {
    enable_registration: true,
    registration_shared_secret: { $secret: true },
    registration_shared_secret_file: null,
    enable_legacy_login: true,
    session_secret: { $secret: true },
    session_secret_file: "/run/secrets/session_secret",
    access_token_lifetime: "1h",
    refresh_token_lifetime: "1y",
    password: {
      enabled: true,
      pepper: { $secret: true },
      pepper_file: null,
      policy: {
        minimum_length: 12,
        require_digit: false,
        require_symbol: false,
        require_uppercase: false,
        require_lowercase: false,
      },
    },
    oidc_providers: [],
  },
  appservices: {
    enabled: true,
    registration_files: ["/etc/myelin/appservices/discord.yaml"],
    tracking_failure_threshold: 50,
  },
  telemetry: {
    metrics: { enabled: true, synapse_compat_names: true },
    tracing: { enabled: false, otlp_endpoint: null, sample_ratio: 0.05 },
    logging: { level: "debug", json: false },
    sentry: { dsn_file: null, environment: "production" },
  },
  cluster: {
    single_node: false,
    room_shards: 256,
    user_shards: 256,
    mesh: { bind_address: "0.0.0.0:9099", advertise_address: null, tls_enabled: true },
    heartbeat_interval: "5s",
    lease_ttl: "30s",
  },
};

/** Revision per section — what `ETag` carries and `If-Match` is checked against. */
export const configRevisions: Record<string, number> = Object.fromEntries(
  Object.keys(configValues).map((name) => [name, 1]),
);
configRevisions.auth = 4;
configRevisions.rate_limits = 7;
configRevisions.federation = 3;
configRevisions.telemetry = 2;

/** When each reloadable section was last hot-applied. */
export const configLastReloaded: Record<string, string | null> = {
  federation: new Date(Date.now() - 26 * 60_000).toISOString(),
  rate_limits: new Date(Date.now() - 3 * 3_600_000).toISOString(),
  telemetry: new Date(Date.now() - 3 * 3_600_000).toISOString(),
  appservices: null,
};

/**
 * `GET /config`'s `source`: the highest-precedence layer contributing to the
 * section, which is the one thing an operator most wants at a glance ("is
 * anything in here pinned outside the database?").
 */
export function sectionSource(name: string): string {
  const rank: Record<string, number> = { default: 0, file: 1, database: 2, environment: 3 };
  let best = "default";
  for (const [pointer, origin] of Object.entries(configOrigins)) {
    if (!pointer.startsWith(`/${name}/`) && pointer !== `/${name}`) continue;
    if (rank[origin] > rank[best]) best = origin;
  }
  return best;
}

/** Recorded config writes, surfaced by the page as the section's change history. */
export const configAuditEntries: AuditEntry[] = [
  {
    id: "audit-config-1",
    recorded_at: new Date(Date.now() - 26 * 60_000).toISOString(),
    action: "config.update",
    actor: { kind: "user", id: "@admin:example.org", display_name: "Operator" },
    target: { type: "config_section", id: "federation" },
    outcome: { status: 200, problem: null },
  },
  {
    id: "audit-config-2",
    recorded_at: new Date(Date.now() - 3 * 3_600_000).toISOString(),
    action: "config.update",
    actor: { kind: "user", id: "@admin:example.org", display_name: "Operator" },
    target: { type: "config_section", id: "rate_limits" },
    outcome: { status: 200, problem: null },
  },
  {
    id: "audit-config-3",
    recorded_at: new Date(Date.now() - 31 * 3_600_000).toISOString(),
    action: "config.update",
    actor: { kind: "user", id: "@deploy:example.org", display_name: "Deploy bot" },
    target: { type: "config_section", id: "auth" },
    outcome: { status: 200, problem: null },
  },
  {
    id: "audit-config-4",
    recorded_at: new Date(Date.now() - 32 * 3_600_000).toISOString(),
    action: "config.update",
    actor: { kind: "user", id: "@admin:example.org", display_name: "Operator" },
    target: { type: "config_section", id: "rate_limits" },
    outcome: {
      status: 400,
      problem: {
        type: "urn:hs:problem:validation",
        title: "Validation failed",
        status: 400,
      },
    },
  },
];

// ---------------------------------------------------------------------------
// The parts of the server this mock has to actually be
// ---------------------------------------------------------------------------

type ValidationError = { pointer: string; detail: string };

function isObject(value: unknown): value is Record<string, JsonValue> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * RFC 7396 JSON Merge Patch, the same rule as
 * `hs_config::document::merge_patch`: objects merge key by key, anything
 * else replaces wholesale, and an explicit `null` *removes* the key rather
 * than setting it to null. The removal is what reset-to-default is.
 */
export function mergePatch(target: JsonValue, patch: JsonValue): JsonValue {
  if (!isObject(patch)) return patch;
  const base: Record<string, JsonValue> = isObject(target) ? { ...target } : {};
  for (const [key, value] of Object.entries(patch)) {
    if (value === null) delete base[key];
    else base[key] = mergePatch(base[key] ?? {}, value);
  }
  return base;
}

/** Every leaf path a patch touches, as a JSON Pointer relative to the patch root. */
export function patchPointers(patch: JsonValue, prefix = ""): string[] {
  if (!isObject(patch) || Object.keys(patch).length === 0) return prefix ? [prefix] : [];
  return Object.entries(patch).flatMap(([key, value]) =>
    patchPointers(value, `${prefix}/${key.replace(/~/g, "~0").replace(/\//g, "~1")}`),
  );
}

/** Settings in `section` that an `HS__` variable pins, as section-relative pointers. */
export function environmentPinned(section: string): string[] {
  return Object.entries(configOrigins)
    .filter(([pointer, origin]) => origin === "environment" && pointer.startsWith(`/${section}/`))
    .map(([pointer]) => pointer.slice(section.length + 1));
}

const DURATION_RE = /^(\d+(?:ms|s|m|h|d|w|y))+$/;
const BYTES_RE = /^\d+(?:\.\d+)?\s*(?:[KMGT]|[KMGT]iB|[KMGT]B|B)?$/i;
const CIDR_RE = /^[0-9a-fA-F:.]+\/\d{1,3}$/;
const SERVER_NAME_RE = /^[a-zA-Z0-9.-]+(?::\d+)?$/;

function checkDuration(value: JsonValue | undefined, pointer: string, out: ValidationError[]) {
  if (value === undefined || value === null) return;
  if (typeof value === "number") return;
  if (typeof value !== "string" || !DURATION_RE.test(value)) {
    out.push({
      pointer,
      detail:
        "expected a duration such as 500ms, 30s, 5m, 1h or 7d, or an integer number of milliseconds",
    });
  }
}

function checkBytes(value: JsonValue | undefined, pointer: string, out: ValidationError[]) {
  if (value === undefined || value === null) return;
  if (typeof value === "number") return;
  if (typeof value !== "string" || !BYTES_RE.test(value)) {
    out.push({
      pointer,
      detail: "expected a byte size such as 512K, 50M or 1GiB, or an integer byte count",
    });
  }
}

const RATE_LIMIT_BUCKETS = [
  "message",
  "registration",
  "login",
  "joins_local",
  "joins_remote",
  "admin_redaction",
  "federation",
  "third_party_id_validation",
];

/**
 * The validators `hs_config` runs, as far as this mock reproduces them. The
 * point is not completeness: it is that every failure mode the interface has
 * to render — a bad duration, a zero rate, a missing dependency between two
 * settings — can be produced by typing something into the form.
 */
export function validateSection(
  section: string,
  values: Record<string, JsonValue>,
): ValidationError[] {
  const out: ValidationError[] = [];
  const at = (path: string): JsonValue | undefined => {
    let current: JsonValue | undefined = values;
    for (const key of path.split(".")) {
      if (!isObject(current)) return undefined;
      current = current[key];
    }
    return current;
  };

  if (section === "server") {
    const name = at("server_name");
    if (typeof name !== "string" || name.length === 0) {
      out.push({ pointer: "/server_name", detail: "required" });
    } else if (!SERVER_NAME_RE.test(name)) {
      out.push({
        pointer: "/server_name",
        detail: "must be a hostname, optionally with a port — no scheme, no path",
      });
    }
    const baseurl = at("public_baseurl");
    if (typeof baseurl === "string" && baseurl.length > 0 && !/^https?:\/\//.test(baseurl)) {
      out.push({ pointer: "/public_baseurl", detail: "must be an absolute http(s) URL" });
    }
  }

  if (section === "rate_limits") {
    for (const name of RATE_LIMIT_BUCKETS) {
      const perSecond = at(`${name}.per_second`);
      if (typeof perSecond === "number" && perSecond <= 0) {
        out.push({
          pointer: `/${name}/per_second`,
          detail: "must be greater than zero; use rate_limits.enabled to turn limiting off",
        });
      }
      const burst = at(`${name}.burst_count`);
      if (typeof burst === "number" && burst < 1) {
        out.push({ pointer: `/${name}/burst_count`, detail: "must be at least 1" });
      }
    }
  }

  if (section === "federation") {
    checkDuration(at("client_timeout"), "/client_timeout", out);
    checkDuration(at("max_retry_backoff"), "/max_retry_backoff", out);
    for (const key of ["ip_range_blocklist", "ip_range_allowlist"]) {
      const list = at(key);
      if (Array.isArray(list)) {
        list.forEach((entry, index) => {
          if (typeof entry !== "string" || !CIDR_RE.test(entry)) {
            out.push({ pointer: `/${key}`, detail: `entry ${index + 1} is not a CIDR range` });
          }
        });
      }
    }
    if (
      at("verify_certificates") === false &&
      (at("custom_ca_certificates") as JsonValue[])?.length
    ) {
      out.push({
        pointer: "/verify_certificates",
        detail:
          "custom_ca_certificates is set, which is the narrower answer; turning verification off entirely would make it pointless",
      });
    }
  }

  if (section === "media") {
    checkBytes(at("max_upload_size"), "/max_upload_size", out);
    checkBytes(at("url_preview_max_fetch_size"), "/url_preview_max_fetch_size", out);
    checkDuration(at("url_preview_timeout"), "/url_preview_timeout", out);
    checkDuration(at("url_preview_cache_lifetime"), "/url_preview_cache_lifetime", out);
    checkDuration(at("remote_media_retention.lifetime"), "/remote_media_retention/lifetime", out);
  }

  if (section === "auth") {
    checkDuration(at("access_token_lifetime"), "/access_token_lifetime", out);
    checkDuration(at("refresh_token_lifetime"), "/refresh_token_lifetime", out);
    const minimum = at("password.policy.minimum_length");
    if (typeof minimum === "number" && (minimum < 1 || minimum > 1024)) {
      out.push({
        pointer: "/password/policy/minimum_length",
        detail: "must be between 1 and 1024",
      });
    }
    if (at("enable_registration") === true && at("registration_shared_secret") === undefined) {
      // Not an error, just the most common misconfiguration; the real server
      // allows open registration without a shared secret.
    }
  }

  if (section === "telemetry") {
    if (at("tracing.enabled") === true && !at("tracing.otlp_endpoint")) {
      out.push({ pointer: "/tracing/otlp_endpoint", detail: "required when tracing is enabled" });
    }
    const ratio = at("tracing.sample_ratio");
    if (typeof ratio === "number" && (ratio < 0 || ratio > 1)) {
      out.push({ pointer: "/tracing/sample_ratio", detail: "must be between 0.0 and 1.0" });
    }
  }

  if (section === "appservices") {
    const threshold = at("tracking_failure_threshold");
    if (typeof threshold === "number" && threshold < 1) {
      out.push({ pointer: "/tracking_failure_threshold", detail: "must be at least 1" });
    }
  }

  if (section === "cluster") {
    checkDuration(at("heartbeat_interval"), "/heartbeat_interval", out);
    checkDuration(at("lease_ttl"), "/lease_ttl", out);
    for (const key of ["room_shards", "user_shards"]) {
      const shards = at(key);
      if (typeof shards === "number" && shards < 1) {
        out.push({ pointer: `/${key}`, detail: "must be at least 1" });
      }
    }
  }

  return out;
}

/** `POST /config/validate`: a whole document in, absolute pointers out. */
export function validateDocument(document: Record<string, JsonValue>): ValidationError[] {
  return Object.entries(document).flatMap(([section, values]) =>
    isObject(values)
      ? validateSection(section, values).map((e) => ({
          pointer: `/${section}${e.pointer}`,
          detail: e.detail,
        }))
      : [],
  );
}

/** The `ETag` a section's current revision produces. */
export function configEtag(section: string): string {
  return `"${section}:${configRevisions[section] ?? 0}"`;
}

/** Records a write the way the store's `history/<revision>` key does. */
export function recordConfigChange(section: string, revision: number): void {
  configAuditEntries.unshift({
    id: `audit-config-${revision}-${section}`,
    recorded_at: new Date().toISOString(),
    action: "config.update",
    actor: { kind: "user", id: "@admin:example.org", display_name: "Operator" },
    target: { type: "config_section", id: section },
    outcome: { status: 200, problem: null },
  });
}
