/**
 * Configuration fixtures: the JSON Schema `GET /config/schema` answers, the
 * effective values behind `GET /config`, and the origin of every setting
 * something set.
 *
 * The schema mirrors `hs_config::Config` as `schemars` emits it — field
 * names, types, doc comments and `serde(default = ...)` values all taken
 * from `crates/hs-config/src/*.rs` and the generated `docs/config.md`, so
 * the forms this drives in `npm run dev:mock` are the forms a real server
 * produces. It is not the whole struct (the `mas_delegation` block is
 * elided), but every *kind* of setting is here, in the shape `schemars`
 * really emits it: booleans, enums (as a `oneOf` of documented `const`s),
 * integers, floats, durations and byte sizes (`x-duration`, `x-bytesize`,
 * also behind `Option<T>`), scalar arrays, an array of enum values, arrays of
 * objects (listeners, thumbnail sizes, OIDC providers), an optional nested
 * object (a listener's `tls`), an internally tagged enum (`media.storage`),
 * nested groups, secrets, a bootstrap-only section and a setting pinned by
 * the environment. `src/test/fixtures/hs-config-schema.json` is the real
 * schema itself, for the tests that need exactly that.
 */
import type { ConfigOrigin, JsonSchemaNode, JsonValue } from "@/api/config-schema";
import type { AuditEntry } from "@/api/config";
import realSchema from "@/test/fixtures/hs-config-schema.json";

const secretString: JsonSchemaNode = {
  type: "string",
  "x-secret": true,
  description:
    "An inline secret value. Prefer the matching *_file key to avoid putting secrets in the config file.",
};

const defs: Record<string, JsonSchemaNode> = {
  SecretString: secretString,
  Duration: {
    type: ["string", "integer"],
    "x-duration": true,
    description:
      "A duration: a string of <number><unit> groups (ms, s, m, h, d, w, y), or an integer number of milliseconds.",
  },
  ByteSize: {
    type: ["string", "integer"],
    "x-bytesize": true,
    description:
      "A byte size: a number with an optional unit (K, M, G, T with 1024 multipliers; KiB/MiB/GiB; KB/MB/GB with 1000 multipliers), or an integer byte count.",
  },
  LogLevel: {
    description: "Log level.",
    oneOf: [
      {
        description: "Everything, including per-request tracing detail.",
        type: "string",
        const: "trace",
      },
      { description: "Verbose diagnostic output.", type: "string", const: "debug" },
      { description: "Normal operational messages.", type: "string", const: "info" },
      {
        description: "Recoverable problems worth an operator's attention.",
        type: "string",
        const: "warn",
      },
      { description: "Failures.", type: "string", const: "error" },
    ],
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
    description: "One HTTP listener.",
    required: ["port", "resources"],
    additionalProperties: false,
    properties: {
      bind_addresses: {
        type: "array",
        items: { type: "string" },
        default: ["::"],
        description: "Addresses to bind. Corresponds to Synapse's bind_addresses.",
      },
      port: {
        type: "integer",
        format: "uint16",
        minimum: 0,
        maximum: 65535,
        description: "TCP port.",
      },
      tls: {
        anyOf: [{ $ref: "#/$defs/TlsConfig" }, { type: "null" }],
        default: null,
        description:
          "TLS material, or None to serve plaintext (typically behind a reverse proxy terminating TLS).",
      },
      resources: {
        type: "array",
        items: { $ref: "#/$defs/ListenerResource" },
        description:
          "Resource families this listener serves. Corresponds to Synapse's resources[].names.",
      },
      x_forwarded: {
        type: "boolean",
        default: false,
        description:
          "Trust X-Forwarded-For and X-Forwarded-Proto from this listener's peers. Corresponds to Synapse's x_forwarded.",
      },
    },
  },
  TlsConfig: {
    type: "object",
    description: "TLS material for a listener.",
    required: ["certificate_path", "private_key_path"],
    additionalProperties: false,
    properties: {
      certificate_path: { type: "string", description: "PEM certificate chain path." },
      private_key_path: { type: "string", description: "PEM private key path." },
    },
  },
  ListenerResource: {
    description:
      "A resource family a listener can serve. Synapse calls these resources with names like client, federation, media, metrics; we keep the same vocabulary since it is what operators already know.",
    oneOf: [
      {
        description: "/_matrix/client/* and legacy /_matrix/r0/*.",
        type: "string",
        const: "client",
      },
      {
        description: "/_matrix/federation/*, /_matrix/key/*.",
        type: "string",
        const: "federation",
      },
      { description: "/_matrix/media/*.", type: "string", const: "media" },
      { description: "Prometheus text exposition.", type: "string", const: "metrics" },
      {
        description: "The native /api/v1 admin API and the embedded management web UI.",
        type: "string",
        const: "admin",
      },
      {
        description: "/health liveness/readiness only, no auth, for load balancers.",
        type: "string",
        const: "health",
      },
    ],
  },
  ThumbnailSize: {
    type: "object",
    description:
      "One generated thumbnail size. Corresponds to one entry in Synapse's thumbnail_sizes.",
    required: ["width", "height", "method"],
    additionalProperties: false,
    properties: {
      width: {
        type: "integer",
        format: "uint32",
        minimum: 0,
        description: "Target width in pixels.",
      },
      height: {
        type: "integer",
        format: "uint32",
        minimum: 0,
        description: "Target height in pixels.",
      },
      method: { $ref: "#/$defs/ThumbnailMethod", description: "Resize method." },
    },
  },
  ThumbnailMethod: {
    description: "How a thumbnail is fit to its target size.",
    oneOf: [
      { description: "Crop to exactly fill the target box.", type: "string", const: "crop" },
      {
        description: "Scale to fit within the target box, preserving aspect ratio.",
        type: "string",
        const: "scale",
      },
    ],
  },
  MediaStorageBackend: {
    description: "Where media bytes live. Restart required to change.",
    oneOf: [
      {
        description: "Local filesystem. Corresponds to Synapse's media_store_path.",
        type: "object",
        required: ["backend", "path"],
        additionalProperties: false,
        properties: {
          path: { type: "string", description: "Root directory for stored media." },
          backend: { type: "string", const: "local" },
        },
      },
      {
        description: "S3-compatible object storage.",
        type: "object",
        required: ["backend", "bucket"],
        additionalProperties: false,
        properties: {
          bucket: { type: "string", description: "Bucket name." },
          region: {
            type: ["string", "null"],
            default: null,
            description: "Region, if the endpoint requires one.",
          },
          endpoint: {
            type: ["string", "null"],
            default: null,
            description: "Custom endpoint for S3-compatible services (MinIO, R2, ...).",
          },
          access_key_id: {
            type: ["string", "null"],
            default: null,
            description: "Access key ID.",
          },
          secret_access_key: {
            $ref: "#/$defs/SecretString",
            default: null,
            description: "Inline secret access key. Prefer secret_access_key_file.",
          },
          secret_access_key_file: {
            type: ["string", "null"],
            default: null,
            description: "Path to a file containing the secret access key.",
          },
          backend: { type: "string", const: "s3" },
        },
      },
      {
        description: "Google Cloud Storage.",
        type: "object",
        required: ["backend", "bucket"],
        additionalProperties: false,
        properties: {
          bucket: { type: "string", description: "Bucket name." },
          service_account_key_file: {
            type: ["string", "null"],
            default: null,
            description: "Path to a service account JSON key file.",
          },
          backend: { type: "string", const: "gcs" },
        },
      },
      {
        description: "Azure Blob Storage.",
        type: "object",
        required: ["backend", "container", "account"],
        additionalProperties: false,
        properties: {
          container: { type: "string", description: "Container name." },
          account: { type: "string", description: "Storage account name." },
          access_key: {
            $ref: "#/$defs/SecretString",
            default: null,
            description: "Inline access key. Prefer access_key_file.",
          },
          access_key_file: {
            type: ["string", "null"],
            default: null,
            description: "Path to a file containing the access key.",
          },
          backend: { type: "string", const: "azure" },
        },
      },
    ],
  },
  OidcProviderConfig: {
    type: "object",
    description:
      "One upstream OIDC identity provider. Corresponds to one entry in Synapse's oidc_providers.",
    required: ["idp_id", "issuer", "client_id"],
    additionalProperties: false,
    properties: {
      idp_id: {
        type: "string",
        description:
          "Stable identifier used in the login flow and stored on the user's external identity. Corresponds to Synapse's idp_id.",
      },
      idp_name: {
        type: ["string", "null"],
        default: null,
        description: "Display name shown on the login page. Corresponds to Synapse's idp_name.",
      },
      issuer: {
        type: "string",
        description: "The provider's issuer URL (used for discovery).",
      },
      client_id: { type: "string", description: "OAuth client ID registered with the provider." },
      client_secret: {
        $ref: "#/$defs/SecretString",
        default: null,
        description: "Inline client secret. Prefer client_secret_file.",
      },
      client_secret_file: {
        type: ["string", "null"],
        default: null,
        description: "Path to a file containing the client secret.",
      },
      scopes: {
        type: "array",
        items: { type: "string" },
        default: ["openid", "profile"],
        description: "OAuth scopes to request.",
      },
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
  SynapseSourceConfig: {
    type: "object",
    description:
      "A Synapse deployment to copy: its PostgreSQL database and, optionally, its media store. The database is only ever read; Synapse keeps working until cutover.",
    required: ["database"],
    properties: {
      database: { $ref: "#/$defs/SynapseDatabaseConfig" },
      media_store_path: {
        anyOf: [{ type: "string" }, { type: "null" }],
        description:
          "Synapse's media_store_path, as this server sees it (the same volume, mounted).",
      },
      batch_size: {
        type: "integer",
        default: 500,
        description: "Rows read from Synapse per batch.",
      },
    },
  },
  SynapseDatabaseConfig: {
    type: "object",
    description: "A connection to Synapse's PostgreSQL database.",
    required: ["host", "database", "user"],
    properties: {
      host: { type: "string", description: "Database host." },
      port: { type: "integer", default: 5432, description: "Database port." },
      database: { type: "string", description: "Database name." },
      user: { type: "string", description: "Connecting role. A read-only role is enough." },
      password: { $ref: "#/$defs/SecretString", description: "The role's password." },
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
          "How to reach this server's administrator: an email address or a Matrix ID, published in /.well-known/matrix/support.",
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
      storage: {
        $ref: "#/$defs/MediaStorageBackend",
        default: { backend: "local", path: "./media-store" },
        description:
          "Storage backend. Corresponds to Synapse's media_storage_providers (simplified to one active backend).",
      },
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
      max_image_pixels: {
        type: "integer",
        format: "uint64",
        minimum: 0,
        default: 33554432,
        description:
          "The largest image, in pixels (width times height), this server makes a thumbnail of; a bigger one is stored but gets no thumbnail. Corresponds to Synapse's max_image_pixels (32M).",
      },
      max_image_dimension: {
        type: "integer",
        format: "uint32",
        minimum: 0,
        default: 32768,
        description: "The widest or tallest image, in pixels, this server makes a thumbnail of.",
      },
      max_image_decode_memory: {
        $ref: "#/$defs/ByteSize",
        description: "The most memory one image may take while it is decoded for a thumbnail.",
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
      remote_media_retention: {
        anyOf: [{ $ref: "#/$defs/Duration" }, { type: "null" }],
        default: null,
        description:
          "How long to keep cached copies of remote media. None means keep forever. Corresponds to Synapse's media_retention.remote_media_lifetime.",
      },
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
      max_queued_pdus_per_destination: {
        type: "integer",
        format: "uint32",
        minimum: 0,
        default: 10000,
        description:
          "How many events the outbound queue holds for one destination before it is dropped and the destination, once it answers again, is caught up with the latest event of each room it is behind in instead (it fetches the rest itself). Bounds what a server that is down for days costs this one's database. Corresponds to Synapse's catch-up mode (destination_rooms), which Synapse enters on the first failure; Synapse has no setting for it. At least 1.",
      },
      max_queued_durable_edus_per_destination: {
        type: "integer",
        format: "uint32",
        minimum: 0,
        default: 10000,
        description:
          "How many to-device messages and device-list and cross-signing key updates this server keeps waiting for one other server before it drops the oldest. They are kept on disk until that server accepts them, so a server that is down for a while still gets the encryption keys and device changes it missed when it is back; this bounds what one that never comes back costs. A dropped update is logged; the other server re-learns a user's devices on their next change or when one of its users asks. Synapse keeps them without a bound and has no setting for it. At least 1; a change applies to the next update queued.",
      },
      forget_unused_destinations_after: {
        $ref: "#/$defs/Duration",
        default: "1w",
        description:
          "How long a server this one shares no room with is kept on the Federation page before it is forgotten. Every server this one ever sent to is remembered with its retry state; once no room brings the two together it is only state, and nothing will be sent to it until a room does again (decision 0042). A sweep runs every hour and forgets each such server once it has had nothing queued and nothing happen (no attempt, no success, no failure) for this long, and each one failing for this long whose queued events are only for rooms this server has since left (leaving a large room leaves one row per server that was in it, most of them never answering). A server this one still shares a room with is never swept. `0` turns the sweep off; the Federation page's Forget and Prune do the same by hand at any time. Synapse keeps every destination for ever and has no setting for it. A change applies to the next sweep.",
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
      access_token_lifetime: {
        $ref: "#/$defs/Duration",
        description: "How long an access token stays valid before it must be refreshed.",
      },
      refresh_token_lifetime: {
        anyOf: [{ $ref: "#/$defs/Duration" }, { type: "null" }],
        default: "1y",
        description: "Refresh token lifetime; None means refresh tokens do not expire.",
      },
      password: { $ref: "#/$defs/PasswordConfig" },
      oidc_providers: {
        type: "array",
        items: { $ref: "#/$defs/OidcProviderConfig" },
        default: [],
        description: "Upstream OIDC providers.",
      },
    },
  },

  appservices: {
    type: "object",
    description: "Appservice (bridge) registry bootstrap settings.",
    properties: {
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
  migration: {
    type: "object",
    description: "The Synapse deployment to migrate from (the admin API's Migration area).",
    properties: {
      synapse: {
        anyOf: [{ $ref: "#/$defs/SynapseSourceConfig" }, { type: "null" }],
        description: "The Synapse deployment to migrate from. Unset: there is nothing to migrate.",
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
  "/auth/password/policy/minimum_length": "database",
  "/appservices/registration_files": "file",
  "/telemetry/metrics/enabled": "database",
  "/telemetry/logging/level": "environment",
  "/cluster/single_node": "file",
};

/** The settings served redacted, whether or not anything has set them. */
const SECRET_POINTERS = [
  "/auth/registration_shared_secret",
  "/auth/password/pepper",
  "/telemetry/sentry/dsn",
  "/storage/password",
  "/migration/synapse/database/password",
];

/** When a change to a setting takes effect (`hs_config::reload::Applies`). */
type Applies = "bootstrap" | "hot" | "restart";

/**
 * Every classified setting and when a change to it applies, read from the
 * `x-applies` annotations of the real configuration schema (the fixture is
 * `hs_config::schema::json_schema()` verbatim, and a Rust test keeps it so) --
 * not a copy of the server's table that could drift from it. A pointer covers
 * everything beneath it.
 */
export const SETTING_APPLIES: ReadonlyMap<string, Applies> = (() => {
  const out = new Map<string, Applies>();
  const schema = realSchema as unknown as Record<string, unknown>;
  const defs = (schema.$defs ?? {}) as Record<string, Record<string, unknown>>;
  const walk = (node: unknown, pointer: string, seen: string[]): void => {
    if (typeof node !== "object" || node === null) return;
    const record = node as Record<string, unknown>;
    const applies = record["x-applies"];
    if (applies === "bootstrap" || applies === "hot" || applies === "restart") {
      out.set(pointer, applies);
      return;
    }
    const ref = typeof record.$ref === "string" ? record.$ref.replace("#/$defs/", "") : null;
    if (ref && !seen.includes(ref)) walk(defs[ref], pointer, [...seen, ref]);
    for (const keyword of ["anyOf", "oneOf", "allOf"]) {
      const branches = record[keyword];
      if (Array.isArray(branches)) branches.forEach((b) => walk(b, pointer, seen));
    }
    const properties = record.properties;
    if (typeof properties === "object" && properties !== null) {
      for (const [key, child] of Object.entries(properties)) {
        walk(child, `${pointer}/${key}`, seen);
      }
    }
  };
  walk(schema, "", []);
  return out;
})();

function settingsOfKind(kind: Applies): string[] {
  return [...SETTING_APPLIES].filter(([, applies]) => applies === kind).map(([p]) => p);
}

/** `hs_config::reload::HOT_SETTINGS`: what a running server re-reads when it changes. */
export const HOT_SETTINGS = settingsOfKind("hot");

/** When a change to the setting at `pointer` takes effect: its own entry's, or "restart". */
export function appliesTo(pointer: string): Applies {
  for (const [entry, applies] of SETTING_APPLIES) {
    if (pointer === entry || pointer.startsWith(`${entry}/`)) return applies;
  }
  return "restart";
}

/** Whether the setting at `pointer` takes effect without a restart. */
export function isHotSetting(pointer: string): boolean {
  return appliesTo(pointer) === "hot";
}

/** `hs_config::reload::RELOADABLE_SECTIONS`: every administered setting in them is hot. */
const RELOADABLE = new Set(
  [...new Set([...SETTING_APPLIES.keys()].map((p) => p.split("/")[1]))].filter((section) => {
    const administered = [...SETTING_APPLIES].filter(
      ([p, applies]) => p.startsWith(`/${section}/`) && applies !== "bootstrap",
    );
    return administered.length > 0 && administered.every(([, applies]) => applies === "hot");
  }),
);

/** `hs_config::store::BOOTSTRAP_SECTIONS`: bootstrap as a whole (decision 0010). */
const BOOTSTRAP = new Set(
  settingsOfKind("bootstrap")
    .filter((p) => p.split("/").length === 2)
    .map((p) => p.slice(1)),
);

/**
 * `hs_config::bootstrap::BOOTSTRAP_SETTINGS` inside administered sections: set at install, never
 * stored in the database. A pointer at or under one of these is a bootstrap setting.
 */
const BOOTSTRAP_SETTINGS = settingsOfKind("bootstrap").filter((p) => p.split("/").length > 2);

function isBootstrapPointer(pointer: string): boolean {
  const section = pointer.split("/")[1];
  return (
    BOOTSTRAP.has(section) ||
    BOOTSTRAP_SETTINGS.some((p) => pointer === p || pointer.startsWith(`${p}/`))
  );
}

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
  "migration",
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
const settingInfos = [
  ...new Set([
    ...Object.keys(configOrigins),
    ...SECRET_POINTERS,
    ...BOOTSTRAP_SETTINGS,
    // A hot setting inside a section that is not hot throughout is worth saying so about.
    ...HOT_SETTINGS.filter((pointer) => !RELOADABLE.has(pointer.split("/")[1])),
    // Like the server, a row for every classified setting, each saying when it applies.
    ...[...SETTING_APPLIES.keys()].filter((pointer) => pointer.split("/").length > 2),
  ]),
]
  .sort()
  .map((pointer) => {
    const section = pointer.split("/")[1];
    const origin = configOrigins[pointer] ?? "default";
    const bootstrap = isBootstrapPointer(pointer);
    return {
      pointer,
      section,
      origin,
      secret: SECRET_POINTERS.includes(pointer),
      reloadable: isHotSetting(pointer),
      applies: appliesTo(pointer),
      bootstrap,
      // The server's own answer to "would config.update take this?". False
      // for a bootstrap setting and for anything an HS__ variable pins.
      editable: origin !== "environment" && !bootstrap,
    };
  });

/**
 * Every setting's doc comment in the real schema, by JSON Pointer. The mock's own schema is
 * smaller and shaped for its fixtures, but its words should be the server's: the Rust doc
 * comments are where every explanation an operator reads is written (decision: the owner's
 * "well explained in the UI" rule, 2026-10-01), so the mock takes them from the fixture rather
 * than keeping copies that drift.
 */
const REAL_DESCRIPTIONS: ReadonlyMap<string, string> = (() => {
  const out = new Map<string, string>();
  const schema = realSchema as unknown as Record<string, unknown>;
  const realDefs = (schema.$defs ?? {}) as Record<string, Record<string, unknown>>;
  const walk = (node: unknown, pointer: string, seen: string[]): void => {
    if (typeof node !== "object" || node === null) return;
    const record = node as Record<string, unknown>;
    if (pointer && typeof record.description === "string" && !out.has(pointer)) {
      out.set(pointer, record.description.replace(/\s+/g, " ").trim());
    }
    const ref = typeof record.$ref === "string" ? record.$ref.replace("#/$defs/", "") : null;
    if (ref && !seen.includes(ref)) walk(realDefs[ref], pointer, [...seen, ref]);
    for (const keyword of ["anyOf", "oneOf", "allOf"]) {
      const branches = record[keyword];
      if (Array.isArray(branches)) branches.forEach((b) => walk(b, pointer, seen));
    }
    const children = record.properties;
    if (typeof children === "object" && children !== null) {
      for (const [key, child] of Object.entries(children)) walk(child, `${pointer}/${key}`, seen);
    }
  };
  for (const [key, child] of Object.entries((schema.properties ?? {}) as object)) {
    walk(child, `/${key}`, []);
  }
  return out;
})();

/** Puts the real schema's words on the mock's settings, wherever both have the setting. */
function adoptRealDescriptions(): void {
  // Only a property's own node takes the real words: a `$ref`'s target is the type's doc and a
  // variant's node is that variant's, both shared or distinct from the setting's.
  const walk = (
    node: JsonSchemaNode | undefined,
    pointer: string,
    seen: string[],
    own: boolean,
  ): void => {
    if (!node) return;
    const real = REAL_DESCRIPTIONS.get(pointer);
    if (own && real && node.description !== undefined) node.description = real;
    const ref = node.$ref?.replace("#/$defs/", "");
    if (ref && !seen.includes(ref)) walk(defs[ref], pointer, [...seen, ref], false);
    for (const branch of [...(node.anyOf ?? []), ...(node.oneOf ?? [])]) {
      walk(branch, pointer, seen, false);
    }
    for (const [key, child] of Object.entries(node.properties ?? {})) {
      walk(child, `${pointer}/${key}`, seen, true);
    }
  };
  for (const [key, child] of Object.entries(properties)) walk(child, `/${key}`, [], true);
}
adoptRealDescriptions();

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
    storage: { backend: "local", path: "/var/lib/myelin/media" },
    max_upload_size: "100M",
    thumbnail_sizes: DEFAULT_THUMBNAIL_SIZES,
    max_image_pixels: 33554432,
    max_image_dimension: 32768,
    max_image_decode_memory: "256M",
    url_preview_enabled: true,
    url_preview_ip_range_blocklist: DEFAULT_IP_BLOCKLIST,
    url_preview_timeout: "10s",
    url_preview_max_fetch_size: "10M",
    url_preview_cache_lifetime: "1h",
    allow_legacy_unauthenticated_media: true,
    remote_media_retention: "90d",
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
    max_queued_pdus_per_destination: 10000,
    max_queued_durable_edus_per_destination: 10000,
    forget_unused_destinations_after: "1w",
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
    oidc_providers: [
      {
        idp_id: "google",
        idp_name: "Google",
        issuer: "https://accounts.google.com/",
        client_id: "myelin-example.apps.googleusercontent.com",
        client_secret_file: "/run/secrets/oidc-google",
        scopes: ["openid", "profile", "email"],
      },
    ],
  },
  appservices: {
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
  migration: { synapse: null },
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

/** Recorded config writes, as the audit log shows them (the page reads `configHistory`). */
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

function isSecretPlaceholder(value: JsonValue): boolean {
  return isObject(value) && Object.keys(value).length === 1 && value.$secret === true;
}

function isSecretMarker(value: JsonValue): value is Record<string, JsonValue> {
  return (
    isSecretPlaceholder(value) ||
    (isObject(value) &&
      Object.keys(value).length === 2 &&
      value.$secret === true &&
      typeof value.$from === "string")
  );
}

function valueAt(root: JsonValue | undefined, pointer: string): JsonValue | undefined {
  let node: JsonValue | undefined = root;
  for (const token of pointer.split("/").slice(1)) {
    const key = token.replace(/~1/g, "/").replace(/~0/g, "~");
    if (Array.isArray(node)) node = node[Number(key)];
    else if (isObject(node)) node = node[key];
    else return undefined;
  }
  return node;
}

/**
 * `SecretPaths::restore_echoed_secrets` (docs/rfcs/0020): inside a list, which the interface sends
 * back whole, an untouched secret is put back from what is stored now — from the pointer its
 * `$from` names, or from the same pointer when it has none — instead of being dropped with its
 * entry. A `$from` naming nothing stored is an error on the placeholder's own pointer. The mock
 * stores its secrets as the placeholder itself, so "put back" keeps the placeholder.
 */
export function restoreEchoedSecrets(
  patch: JsonValue,
  current: JsonValue | undefined,
  section: string,
): { patch: JsonValue; errors: { pointer: string; detail: string }[] } {
  const errors: { pointer: string; detail: string }[] = [];
  const walk = (node: JsonValue, pointer: string, inArray: boolean): JsonValue | undefined => {
    if (isSecretMarker(node)) {
      const from = node.$from;
      if (typeof from === "string") {
        const stored = from.startsWith(`/${section}/`)
          ? valueAt(current, from.slice(section.length + 1))
          : undefined;
        if (stored === undefined || stored === null) {
          errors.push({
            pointer: `/${section}${pointer}`,
            detail: `$from names ${from}, where no secret of this section is stored now`,
          });
        }
        return stored ?? undefined;
      }
      if (!inArray) return node;
      // Nothing stored here: dropped, as `strip_echoed_secrets` would.
      const stored = valueAt(current, pointer);
      return stored === null ? undefined : stored;
    }
    if (Array.isArray(node)) {
      return node.map((item, index) => walk(item, `${pointer}/${index}`, true) ?? null);
    }
    if (isObject(node)) {
      const out: Record<string, JsonValue> = {};
      for (const [key, child] of Object.entries(node)) {
        const next = walk(child, `${pointer}/${key}`, inArray);
        if (next !== undefined) out[key] = next;
      }
      return out;
    }
    return node;
  };
  return { patch: walk(patch, "", false) ?? {}, errors };
}

/**
 * `SecretPaths::strip_echoed_secrets` in `crates/hs-admin/src/config_schema.rs`: a form
 * round-trips the secrets it was shown as `{"$secret": true}`, and the server drops each one from
 * the patch, which is what "the operator left this alone" means for a setting of its own. Inside
 * a list, `restoreEchoedSecrets` has already put back or dropped each one (the mock stores its
 * secrets as the placeholder itself, so lists are left alone here).
 */
export function stripEchoedSecrets(patch: JsonValue): JsonValue {
  if (Array.isArray(patch)) return patch;
  if (!isObject(patch)) return patch;
  const out: Record<string, JsonValue> = {};
  for (const [key, value] of Object.entries(patch)) {
    if (isSecretMarker(value)) continue;
    out[key] = stripEchoedSecrets(value);
  }
  return out;
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
    checkBytes(at("max_image_decode_memory"), "/max_image_decode_memory", out);
    checkDuration(at("url_preview_timeout"), "/url_preview_timeout", out);
    checkDuration(at("url_preview_cache_lifetime"), "/url_preview_cache_lifetime", out);
    checkDuration(at("remote_media_retention"), "/remote_media_retention", out);
    const thumbnails = at("thumbnail_sizes");
    if (Array.isArray(thumbnails)) {
      thumbnails.forEach((entry, index) => {
        for (const key of ["width", "height"]) {
          const size = isObject(entry) ? entry[key] : undefined;
          if (typeof size !== "number" || !Number.isInteger(size) || size < 1) {
            out.push({
              pointer: `/thumbnail_sizes/${index}/${key}`,
              detail: "must be a whole number of pixels, at least 1",
            });
          }
        }
      });
    }
    const storage = at("storage");
    if (isObject(storage)) {
      const required: Record<string, string[]> = {
        local: ["path"],
        s3: ["bucket"],
        gcs: ["bucket"],
        azure: ["container", "account"],
      };
      const backend = String(storage.backend);
      if (!(backend in required)) {
        out.push({ pointer: "/storage/backend", detail: `unknown backend ${backend}` });
      } else {
        for (const key of required[backend]) {
          if (typeof storage[key] !== "string" || storage[key] === "") {
            out.push({ pointer: `/storage/${key}`, detail: "required" });
          }
        }
      }
    }
  }

  if (section === "listeners") {
    const listeners = at("listeners");
    if (Array.isArray(listeners)) {
      const ports = new Set<number>();
      listeners.forEach((entry, index) => {
        const port = isObject(entry) ? entry.port : undefined;
        if (typeof port !== "number" || !Number.isInteger(port) || port < 1 || port > 65535) {
          out.push({
            pointer: `/listeners/${index}/port`,
            detail: "must be a TCP port, 1 to 65535",
          });
        } else if (ports.has(port)) {
          out.push({
            pointer: `/listeners/${index}/port`,
            detail: `another listener already binds port ${port}`,
          });
        } else {
          ports.add(port);
        }
        const resources = isObject(entry) ? entry.resources : undefined;
        if (!Array.isArray(resources) || resources.length === 0) {
          out.push({
            pointer: `/listeners/${index}/resources`,
            detail: "a listener that serves nothing is a mistake; choose at least one",
          });
        }
      });
    }
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
    const providers = at("oidc_providers");
    if (Array.isArray(providers)) {
      const ids = new Set<string>();
      providers.forEach((entry, index) => {
        const provider = isObject(entry) ? entry : {};
        for (const key of ["idp_id", "issuer", "client_id"]) {
          if (typeof provider[key] !== "string" || provider[key] === "") {
            out.push({ pointer: `/oidc_providers/${index}/${key}`, detail: "required" });
          }
        }
        if (typeof provider.issuer === "string" && provider.issuer !== "") {
          if (!/^https:\/\//.test(provider.issuer)) {
            out.push({
              pointer: `/oidc_providers/${index}/issuer`,
              detail: "must be an https URL",
            });
          }
        }
        if (typeof provider.idp_id === "string" && provider.idp_id !== "") {
          if (ids.has(provider.idp_id)) {
            out.push({
              pointer: `/oidc_providers/${index}/idp_id`,
              detail: `another provider is already called ${provider.idp_id}`,
            });
          }
          ids.add(provider.idp_id);
        }
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

// ---------------------------------------------------------------------------
// Per-setting history (`config.history.list`, `config.history.revert`)
// ---------------------------------------------------------------------------

/**
 * One recorded write, the way `hs_config::store::ChangeRecord` keeps it: the
 * patch, and what each setting it touched held before. `before` is `null` for
 * a record from before the server kept prior values (it can be shown, not
 * reverted); inside it, `undefined` means the setting was not set.
 */
export interface MockConfigRecord {
  revision: number;
  section: string;
  patch: Record<string, JsonValue>;
  actor: string;
  at: string;
  before: Record<string, JsonValue | undefined> | null;
  reverts: number | null;
}

const minutesAgo = (minutes: number) => new Date(Date.now() - minutes * 60_000).toISOString();

/** Oldest first. Revisions are per section here, as the mock's `ETag`s are. */
export const configHistory: MockConfigRecord[] = [
  {
    revision: 5,
    section: "rate_limits",
    patch: { enabled: true },
    actor: "@deploy:example.org",
    at: minutesAgo(40 * 60),
    before: null,
    reverts: null,
  },
  {
    revision: 6,
    section: "rate_limits",
    patch: { login: { per_second: 0.17 } },
    actor: "@admin:example.org",
    at: minutesAgo(32 * 60),
    before: { "/login/per_second": 0.1 },
    reverts: null,
  },
  {
    revision: 7,
    section: "rate_limits",
    patch: { message: { burst_count: 25 }, login: { burst_count: 5 } },
    actor: "@admin:example.org",
    at: minutesAgo(3 * 60),
    before: { "/message/burst_count": 20, "/login/burst_count": 3 },
    reverts: null,
  },
  {
    revision: 3,
    section: "auth",
    patch: { enable_registration: true },
    actor: "@admin:example.org",
    at: minutesAgo(33 * 60),
    before: { "/enable_registration": false },
    reverts: null,
  },
  {
    revision: 4,
    section: "auth",
    // The mock keeps the secrets themselves here, as the real store does; they are redacted on
    // the way out (`configChangeBody`), never stored as the placeholder.
    patch: { registration_shared_secret: "rotated-by-deploy" },
    actor: "@deploy:example.org",
    at: minutesAgo(31 * 60),
    before: { "/registration_shared_secret": "the-first-one" },
    reverts: null,
  },
  {
    revision: 3,
    section: "federation",
    patch: { client_timeout: "45s" },
    actor: "@admin:example.org",
    at: minutesAgo(26),
    before: { "/client_timeout": "30s" },
    reverts: null,
  },
];

const pristineHistory = structuredClone(configHistory);

/** Puts the history back as seeded (the tests' `afterEach`). */
export function resetConfigHistory(): void {
  configHistory.splice(0, configHistory.length, ...structuredClone(pristineHistory));
}

function pointerTokens(pointer: string): string[] {
  return pointer
    .split("/")
    .slice(1)
    .map((t) => t.replace(/~1/g, "/").replace(/~0/g, "~"));
}

/** A merge patch that sets `pointer` to `value` (`null`: removes it). */
function patchFor(pointer: string, value: JsonValue): Record<string, JsonValue> {
  let patch: JsonValue = value;
  for (const token of pointerTokens(pointer).reverse()) patch = { [token]: patch };
  return patch as Record<string, JsonValue>;
}

/** What each setting `patch` touches holds in `values` now. */
export function beforeValues(
  values: Record<string, JsonValue>,
  patch: JsonValue,
): Record<string, JsonValue | undefined> {
  return Object.fromEntries(patchPointers(patch).map((p) => [p, valueAt(values, p)]));
}

export function recordConfigHistory(
  section: string,
  revision: number,
  patch: Record<string, JsonValue>,
  before: Record<string, JsonValue | undefined>,
  reverts: number | null = null,
): void {
  configHistory.push({
    revision,
    section,
    patch,
    actor: "@admin:example.org",
    at: new Date().toISOString(),
    before,
    reverts,
  });
}

/** A record as `GET /config/{section}/history` answers it: per setting, secrets redacted. */
export function configChangeBody(record: MockConfigRecord) {
  const side = (value: JsonValue | undefined, secret: boolean) =>
    value === undefined || value === null
      ? { set: false }
      : { set: true, value: secret ? { $secret: true } : value };
  let patch: JsonValue = record.patch;
  for (const relative of patchPointers(record.patch)) {
    if (SECRET_POINTERS.includes(`/${record.section}${relative}`)) {
      patch = mergePatch(patch, patchFor(relative, { $secret: true }));
    }
  }
  return {
    revision: record.revision,
    section: record.section,
    patch,
    actor: record.actor,
    at: record.at,
    reverts: record.reverts,
    revertible: record.before !== null,
    settings: patchPointers(record.patch).map((relative) => {
      const pointer = `/${record.section}${relative}`;
      const after = valueAt(record.patch, relative);
      const before = record.before?.[relative];
      const secret =
        SECRET_POINTERS.includes(pointer) ||
        isSecretMarker(after ?? null) ||
        isSecretMarker(before ?? null);
      return {
        pointer,
        path: pointerTokens(pointer).join("."),
        secret,
        from: record.before === null ? null : side(before, secret),
        to: side(after, secret),
      };
    }),
  };
}

/** Later records of the same section that wrote a setting `record` touched, as `errors[]`. */
export function revertConflicts(record: MockConfigRecord): { pointer: string; detail: string }[] {
  const touched = Object.keys(record.before ?? {});
  const overlaps = (a: string, b: string) =>
    a === b || a.startsWith(`${b}/`) || b.startsWith(`${a}/`);
  return configHistory
    .filter((later) => later.section === record.section && later.revision > record.revision)
    .flatMap((later) =>
      patchPointers(later.patch)
        .filter((p) => touched.some((t) => overlaps(t, p)))
        .map((p) => ({
          pointer: `/${record.section}${p}`,
          detail: `changed again in revision ${later.revision} by ${later.actor} at ${later.at}`,
        })),
    );
}

/** The merge patch that puts every setting `record` touched back as it was. */
export function revertPatch(record: MockConfigRecord): Record<string, JsonValue> {
  let patch: JsonValue = {};
  for (const [pointer, value] of Object.entries(record.before ?? {})) {
    patch = combinePatches(patch, patchFor(pointer, value === undefined ? null : value));
  }
  return patch as Record<string, JsonValue>;
}

/** Combines two merge patches, keeping their `null`s (removals) rather than applying them. */
function combinePatches(target: JsonValue, patch: JsonValue): JsonValue {
  if (!isObject(patch) || !isObject(target)) return patch;
  const out: Record<string, JsonValue> = { ...target };
  for (const [key, value] of Object.entries(patch)) {
    out[key] = key in out ? combinePatches(out[key], value) : value;
  }
  return out;
}
