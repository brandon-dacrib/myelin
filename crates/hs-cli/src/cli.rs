//! Argument parsing (`clap`) and dispatch for every `hs` subcommand. See
//! `docs/compat/cli-shims.md` for the specification each subcommand is built against.

use std::io::Write as _;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// The `hs` command-line interface.
#[derive(Debug, Parser)]
#[command(
    name = "hs",
    about = "The homeserver CLI: serve, config, admin and compat tools"
)]
pub struct Cli {
    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Every `hs` subcommand.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Loads configuration, opens storage, mounts the client API and serves until shut down.
    Serve(ServeArgs),
    /// Reads and changes this server's stored configuration, for an operator without a browser.
    Config(ConfigArgs),
    /// Writes the small bootstrap config file: the few settings read before the database opens.
    GenerateConfig(GenerateConfigArgs),
    /// Hashes a password the way the server would at registration time.
    HashPassword(HashPasswordArgs),
    /// Generates a new Ed25519 signing key in Synapse's `signing.key` text format.
    GenerateSigningKey(GenerateSigningKeyArgs),
    /// Registers a user against a running server via the shared-secret admin protocol.
    Register(RegisterArgs),
    /// Prints a one-time link that gets an administrator back into a running server nobody can
    /// sign in to. Run it where the server keeps its signing key: the request is signed with it.
    Recover(RecoverArgs),
    /// Writes the `routes.json` manifest (`docs/rfcs/0005-routes-json-manifest.md`) without
    /// booting a server — routes are static, independent of runtime config.
    RoutesManifest(RoutesManifestArgs),
    /// Joins a room hosted by another server via the real `make_join`/`send_join` federation
    /// handshake (`hs_federation::outbound_join::join_room`), as this server's own user.
    ///
    /// This is a diagnostic/administrative tool, not (yet) what `POST /join` calls: no code path
    /// in this workspace triggers an outbound federated join from the ordinary client API yet,
    /// because doing so durably needs an `hs-room` API this server does not have (see
    /// `docs/rfcs/0015-outbound-join-needs-a-room-bootstrap-api.md`). This command performs the
    /// real handshake anyway and reports what it verified: proof the wire protocol, TLS/CA trust
    /// and event signing all work against a real remote, even though the room cannot yet be
    /// represented locally afterward.
    #[command(name = "federation-join-room")]
    FederationJoinRoom(FederationJoinRoomArgs),
    /// Runs the bridge operator: deploys each `Bridge` resource in one namespace as a pod, a
    /// Service and a volume, and reports its state back (`docs/rfcs/0017`). The Helm chart runs
    /// it as its own Deployment; it is not part of `hs serve`.
    Operator(OperatorArgs),
    /// Prints the `hs` version.
    Version,
}

/// `hs federation-join-room` arguments.
#[derive(Debug, Args)]
pub struct FederationJoinRoomArgs {
    /// Native `hs-config` YAML file this server would otherwise run `hs serve` from: supplies
    /// this server's own `server_name` and signing key (used to sign the join event) plus its
    /// federation policy (TLS/CA trust, IP range policy, timeouts).
    #[arg(short = 'c', long = "config")]
    pub config: PathBuf,

    /// The resident server to ask -- one already participating in the room, e.g.
    /// `matrix.example.org` or, for two local instances with no DNS, `localhost:8448`.
    #[arg(long = "destination")]
    pub destination: String,

    /// The room to join, e.g. `!abc123:matrix.example.org`.
    #[arg(long = "room")]
    pub room: String,

    /// The full Matrix user ID performing the join, e.g. `@alice:example.org`. Must belong to
    /// this server's own `server_name` (the resident server checks this; this command does not
    /// pre-check it).
    #[arg(long = "user")]
    pub user: String,
}

/// `hs serve` arguments.
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Native `hs-config` YAML file to load. Optional: with `--data-dir` and `--server-name`,
    /// a first run needs no file at all, and every run after that reads its settings from the
    /// database (see `crate::bootstrap`).
    #[arg(short = 'c', long = "config")]
    pub config: Option<PathBuf>,

    /// The one directory this server keeps everything in: its database in `<dir>/db`, its signing
    /// keys in `<dir>/keys`, its media in `<dir>/media`. Also readable from `HS_DATA_DIR`.
    /// Anything a config file, an `HS__` variable or the database says about those paths wins
    /// over this.
    #[arg(long = "data-dir")]
    pub data_dir: Option<PathBuf>,

    /// This server's name, for a first run with no config file. Recorded in the database the
    /// first time, so later runs do not need it; a value that disagrees with the one already
    /// recorded is reported and ignored, because it is baked into every event already signed.
    #[arg(long = "server-name")]
    pub server_name: Option<String>,

    /// A Synapse `homeserver.yaml` to translate and serve from.
    #[arg(long = "synapse-config", conflicts_with = "config")]
    pub synapse_config: Option<PathBuf>,

    /// Proceed even if the Synapse config sets unsupported or unrecognized keys.
    #[arg(long = "allow-unsupported-synapse-config")]
    pub allow_unsupported_synapse_config: bool,

    /// Format for the translation report: `markdown` (default) or `json`.
    #[arg(long = "translation-report", default_value = "markdown")]
    pub translation_report: String,

    /// Write the translation report to this file instead of stderr.
    #[arg(long = "translation-report-out")]
    pub translation_report_out: Option<PathBuf>,

    /// Deprecated: set `server.unstable_features` in the admin interface's Configuration page
    /// instead. A YAML file whose `unstable_features` win over the configured ones in
    /// `GET /_matrix/client/versions` (see `crate::versions`'s module doc for the file shape).
    #[arg(long = "capabilities-config")]
    pub capabilities_config: Option<PathBuf>,

    /// Write the `routes.json` manifest here at startup, before binding any listener. Omit to
    /// skip writing it (use the `hs routes-manifest` subcommand instead, which needs no config
    /// and does not bind a socket).
    #[arg(long = "routes-manifest")]
    pub routes_manifest: Option<PathBuf>,

    /// Deprecated: set `media.scanning` in the admin interface's Configuration page instead. A
    /// standalone `media.scanning` YAML file (the shape of
    /// `deploy/media-scanning/media-scanning.yaml`) that replaces the configured section
    /// wholesale, with a warning at startup.
    #[arg(long = "media-scanning-config")]
    pub media_scanning_config: Option<PathBuf>,
}

/// `hs operator` arguments.
#[derive(Debug, Args)]
pub struct OperatorArgs {
    /// The namespace whose `Bridge` resources to run. Defaults to `POD_NAMESPACE`, else the
    /// namespace of the pod's service account
    /// (`/var/run/secrets/kubernetes.io/serviceaccount/namespace`).
    #[arg(long = "namespace")]
    pub namespace: Option<String>,

    /// The StorageClass a bridge's volume asks for when its `Bridge` names none; the cluster's
    /// default when unset.
    #[arg(long = "default-storage-class")]
    pub default_storage_class: Option<String>,

    /// Also run the `Homeserver` controller: each `Homeserver` resource in the namespace becomes
    /// the chart's StatefulSet, Services and ConfigMap, and every replica that goes away is
    /// drained through the admin API first. Needs the `Homeserver` CRD and the RBAC in
    /// `deploy/operator/`.
    #[arg(long = "homeservers")]
    pub homeservers: bool,

    /// Serve the operator's Prometheus metrics (`hs_operator_*`) on `/metrics` at this address,
    /// for example `0.0.0.0:9090`.
    #[arg(long = "metrics-address")]
    pub metrics_address: Option<std::net::SocketAddr>,
}

/// `hs routes-manifest` arguments.
#[derive(Debug, Args)]
pub struct RoutesManifestArgs {
    /// Write the manifest here instead of stdout.
    #[arg(short = 'o', long = "output")]
    pub output: Option<PathBuf>,
}

/// `hs generate-config` arguments.
#[derive(Debug, Args)]
pub struct GenerateConfigArgs {
    /// The server name to bake into the generated config.
    #[arg(long = "server-name")]
    pub server_name: String,

    /// The directory the generated file points its three filesystem paths at. Defaults to
    /// `./data`, which is what makes the generated file work as written rather than needing
    /// three hand-edits before a container can write anything.
    #[arg(long = "data-dir", default_value = "./data")]
    pub data_dir: PathBuf,

    /// Write the config here instead of stdout.
    #[arg(short = 'o', long = "output")]
    pub output: Option<PathBuf>,
}

/// `hs config` arguments: which store to open, and what to do with it.
#[derive(Debug, Args)]
pub struct ConfigArgs {
    /// The bootstrap config file, if this deployment has one. Needed only to find the database
    /// -- everything else comes from the database itself.
    #[arg(short = 'c', long = "config", global = true)]
    pub config: Option<PathBuf>,

    /// The data directory, as `hs serve --data-dir` (or `HS_DATA_DIR`).
    #[arg(long = "data-dir", global = true)]
    pub data_dir: Option<PathBuf>,

    /// What to do.
    #[command(subcommand)]
    pub command: ConfigCommand,
}

/// Every `hs config` subcommand.
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Prints the effective configuration with each setting's origin, secrets redacted.
    Show(ConfigShowArgs),
    /// Prints one setting's effective value.
    Get(ConfigPointerArgs),
    /// Stores one setting in the database, after checking the result would be valid.
    Set(ConfigSetArgs),
    /// Clears one setting from the database, so it falls back to the file or the default.
    Unset(ConfigPointerArgs),
    /// Replays a configuration document into the database.
    Import(ConfigImportArgs),
    /// Prints the stored configuration as YAML, for backup or `hs config import`.
    Export(ConfigExportArgs),
    /// Prints recent configuration changes, newest first.
    History(ConfigHistoryArgs),
}

/// `hs config show` arguments.
#[derive(Debug, Args)]
pub struct ConfigShowArgs {
    /// `table` (default) or `json`.
    #[arg(long = "format", default_value = "table")]
    pub format: crate::config_cmd::ShowFormat,
}

/// Arguments for the `hs config` subcommands that name one setting.
#[derive(Debug, Args)]
pub struct ConfigPointerArgs {
    /// The setting, as a JSON Pointer: `/auth/enable_registration`.
    pub pointer: String,
}

/// `hs config set` arguments.
#[derive(Debug, Args)]
pub struct ConfigSetArgs {
    /// The setting, as a JSON Pointer: `/auth/enable_registration`.
    pub pointer: String,

    /// The value. Read as JSON when it parses as JSON (`true`, `8008`, `["a"]`) and as a plain
    /// string otherwise, so URLs and durations need no quoting.
    pub value: String,
}

/// `hs config import` arguments.
#[derive(Debug, Args)]
pub struct ConfigImportArgs {
    /// A YAML or JSON configuration document, as `hs config export` writes.
    pub file: PathBuf,
}

/// `hs config export` arguments.
#[derive(Debug, Args)]
pub struct ConfigExportArgs {
    /// Write the export here instead of stdout.
    #[arg(short = 'o', long = "output")]
    pub output: Option<PathBuf>,
}

/// `hs config history` arguments.
#[derive(Debug, Args)]
pub struct ConfigHistoryArgs {
    /// How many changes to show.
    #[arg(short = 'n', long = "limit", default_value_t = 20)]
    pub limit: usize,
}

/// `hs hash-password` arguments.
#[derive(Debug, Args)]
pub struct HashPasswordArgs {
    /// The password to hash. Prompted for (not echoed) if omitted; never accepted as a bare
    /// positional argument, which would leak it into shell history and `ps`.
    #[arg(short = 'p', long = "password")]
    pub password: Option<String>,

    /// A native or Synapse config file to read `auth.password.pepper` from.
    #[arg(short = 'c', long = "config")]
    pub config: Option<PathBuf>,
}

/// `hs generate-signing-key` arguments.
#[derive(Debug, Args)]
pub struct GenerateSigningKeyArgs {
    /// Write the key here instead of stdout.
    #[arg(short = 'o', long = "output")]
    pub output: Option<PathBuf>,
}

/// `hs recover` arguments. See `hs_auth::recovery` for what the link does and why holding the
/// signing key is what entitles somebody to one.
#[derive(Debug, Args)]
pub struct RecoverArgs {
    /// The running server's base URL. The default is right from inside its pod or container
    /// (`kubectl exec <pod> -- hs recover`, `docker exec <container> hs recover`).
    #[arg(long = "server", default_value = "http://127.0.0.1:8008")]
    pub server_url: String,

    /// The server's signing key: a key file, or the directory holding one. Defaults to
    /// `HS__SERVER__SIGNING_KEY_PATH` when set (a Helm chart in cluster mode sets it to the
    /// mounted Secret), else `<data-dir>/keys`.
    #[arg(long = "signing-key")]
    pub signing_key: Option<PathBuf>,

    /// The data directory, as `hs serve --data-dir` (or `HS_DATA_DIR`): its `keys/` holds the
    /// signing key.
    #[arg(long = "data-dir")]
    pub data_dir: Option<PathBuf>,
}

/// `hs register` arguments (`register_new_matrix_user`-compatible; see
/// `docs/compat/cli-shims.md`).
#[derive(Debug, Args)]
pub struct RegisterArgs {
    /// The target server's base URL, e.g. `https://matrix.example.org`.
    pub server_url: String,

    /// Username (localpart or full Matrix ID). Prompted for if omitted.
    #[arg(short = 'u', long = "user")]
    pub user: Option<String>,

    /// Password. Prompted for (not echoed) if omitted.
    #[arg(short = 'p', long = "password")]
    pub password: Option<String>,

    /// Register the account as a server admin.
    #[arg(short = 'a', long = "admin", conflicts_with = "no_admin")]
    pub admin: bool,

    /// Register the account as a non-admin (the default; exists to mirror Synapse's script,
    /// which accepts either flag explicitly).
    #[arg(long = "no-admin")]
    pub no_admin: bool,

    /// A native or Synapse config file to read `auth.registration_shared_secret` from.
    #[arg(short = 'c', long = "config", conflicts_with = "shared_secret")]
    pub config: Option<PathBuf>,

    /// The shared secret directly.
    #[arg(short = 'k', long = "shared-secret")]
    pub shared_secret: Option<String>,

    /// Optional user type (Synapse's `support`/`bot` categories).
    #[arg(long = "user-type")]
    pub user_type: Option<String>,

    /// Also print the access token and device ID on success.
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,
}

/// Prompts for a value on stdout/stdin (not echoed for passwords), matching
/// `register_new_matrix_user`'s interactive behavior when a flag is omitted.
fn prompt(label: &str) -> std::io::Result<String> {
    print!("{label}: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\n', '\r']).to_owned())
}

fn prompt_password(label: &str) -> std::io::Result<String> {
    rpassword::prompt_password(format!("{label}: "))
}

/// Runs the parsed CLI. Returns the process exit code.
pub async fn dispatch(cli: Cli) -> i32 {
    match cli.command {
        Command::Version => {
            println!("hs {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Command::Config(args) => run_config(&args),
        Command::GenerateConfig(args) => run_generate_config(&args),
        Command::HashPassword(args) => run_hash_password(&args),
        Command::GenerateSigningKey(args) => run_generate_signing_key(&args),
        Command::Register(args) => run_register(&args).await,
        Command::Recover(args) => run_recover(&args).await,
        Command::RoutesManifest(args) => run_routes_manifest(&args),
        Command::Serve(args) => run_serve(&args).await,
        Command::FederationJoinRoom(args) => crate::federation::run_join_room(&args).await,
        Command::Operator(args) => run_operator(&args).await,
    }
}

/// Where a pod finds its own namespace when `POD_NAMESPACE` is not set.
const SERVICE_ACCOUNT_NAMESPACE: &str = "/var/run/secrets/kubernetes.io/serviceaccount/namespace";

/// `--namespace`, else `POD_NAMESPACE`, else the service account's namespace file.
fn operator_namespace(args: &OperatorArgs) -> Option<String> {
    args.namespace
        .clone()
        .or_else(|| std::env::var("POD_NAMESPACE").ok())
        .or_else(|| std::fs::read_to_string(SERVICE_ACCOUNT_NAMESPACE).ok())
        .map(|ns| ns.trim().to_owned())
        .filter(|ns| !ns.is_empty())
}

async fn run_operator(args: &OperatorArgs) -> i32 {
    // No configuration file here: the level comes from `RUST_LOG` or the default, and the format
    // is JSON, as `hs serve`'s default.
    let _telemetry_guard = match hs_telemetry::init(&hs_telemetry::Options {
        service_name: "hs-operator".to_owned(),
        ..hs_telemetry::Options::default()
    }) {
        Ok(guard) => Some(guard),
        Err(e) => {
            eprintln!("hs operator: failed to initialize telemetry: {e}");
            None
        }
    };
    let Some(namespace) = operator_namespace(args) else {
        eprintln!(
            "hs operator: no namespace: pass --namespace, set POD_NAMESPACE, or run in a pod \
             ({SERVICE_ACCOUNT_NAMESPACE})"
        );
        return 2;
    };
    let client = match hs_operator::connect().await {
        Ok(client) => client,
        Err(e) => {
            eprintln!("hs operator: cannot reach the Kubernetes API: {e}");
            return 1;
        }
    };
    let options = hs_operator::RunOptions {
        bridges: hs_operator::controller::Options {
            default_storage_class: args.default_storage_class.clone().filter(|c| !c.is_empty()),
        },
        homeservers: args.homeservers,
        metrics_address: args.metrics_address,
    };
    match hs_operator::run(client, namespace, options).await {
        Ok(()) => 0,
        Err(e) => {
            tracing::error!(error = %e, "hs operator stopped with an error");
            1
        }
    }
}

fn run_routes_manifest(args: &RoutesManifestArgs) -> i32 {
    let manifest = crate::serve::route_manifest();
    let json = match manifest.to_json_pretty() {
        Ok(j) => j,
        Err(e) => {
            eprintln!("hs routes-manifest: {e}");
            return 1;
        }
    };
    match &args.output {
        Some(path) => {
            if let Err(e) = std::fs::write(path, &json) {
                eprintln!("hs routes-manifest: failed to write {path:?}: {e}");
                return 1;
            }
        }
        None => println!("{json}"),
    }
    0
}

/// Opens the configuration store the way `hs serve` would, then runs one `hs config` subcommand
/// against it. Every subcommand goes through the same boot so that what `hs config show` prints
/// is what `hs serve` would run on, rather than a second opinion assembled a different way.
fn run_config(args: &ConfigArgs) -> i32 {
    let source = match &args.config {
        Some(path) => crate::bootstrap::ConfigSource::Native(path.clone()),
        None => crate::bootstrap::ConfigSource::None,
    };
    let mut booted = match crate::bootstrap::boot(&crate::bootstrap::BootOptions {
        source,
        data_dir: args.data_dir.clone(),
        server_name: None,
    }) {
        Ok(booted) => booted,
        Err(e) => {
            eprintln!("hs config: {e}");
            return 1;
        }
    };

    let result = match &args.command {
        ConfigCommand::Show(a) => crate::config_cmd::show(&booted, a.format),
        ConfigCommand::Get(a) => crate::config_cmd::get(&booted, &a.pointer),
        ConfigCommand::Set(a) => crate::config_cmd::set(&mut booted, &a.pointer, &a.value),
        ConfigCommand::Unset(a) => crate::config_cmd::unset(&mut booted, &a.pointer),
        ConfigCommand::Import(a) => crate::config_cmd::import(&mut booted, &a.file),
        ConfigCommand::Export(_) => crate::config_cmd::export(&booted),
        ConfigCommand::History(a) => crate::config_cmd::history(&booted, a.limit),
    };
    let text = match result {
        Ok(text) => text,
        Err(e) => {
            eprintln!("hs config: {e}");
            return 1;
        }
    };
    match &args.command {
        ConfigCommand::Export(ConfigExportArgs {
            output: Some(path), ..
        }) => {
            if let Err(e) = std::fs::write(path, &text) {
                eprintln!("hs config: failed to write {path:?}: {e}");
                return 1;
            }
        }
        // `get` is the one subcommand whose output is meant to be captured by a shell, so it gets
        // a trailing newline and nothing else; the rest already format their own.
        ConfigCommand::Get(_) => println!("{}", text.trim_end()),
        _ => print!("{text}"),
    }
    0
}

fn run_generate_config(args: &GenerateConfigArgs) -> i32 {
    let yaml = crate::generate_config::render_yaml(&args.server_name, &args.data_dir);
    match &args.output {
        Some(path) => {
            if let Err(e) = std::fs::write(path, yaml) {
                eprintln!("hs generate-config: failed to write {path:?}: {e}");
                return 1;
            }
        }
        None => print!("{yaml}"),
    }
    0
}

fn run_hash_password(args: &HashPasswordArgs) -> i32 {
    let password = match &args.password {
        Some(p) => p.clone(),
        None => match prompt_password("Password") {
            Ok(p) => p,
            Err(e) => {
                eprintln!("hs hash-password: failed to read password: {e}");
                return 1;
            }
        },
    };
    if let Some(config_path) = &args.config
        && let Err(e) = crate::config_bridge::read_pepper_from_config(config_path)
    {
        eprintln!("hs hash-password: {e}");
        return 1;
    }
    match crate::hash_password::hash_password(&password) {
        Ok(hash) => {
            println!("{hash}");
            0
        }
        Err(e) => {
            eprintln!("hs hash-password: {e}");
            1
        }
    }
}

fn run_generate_signing_key(args: &GenerateSigningKeyArgs) -> i32 {
    let line = crate::signing_key::generate_signing_key_line();
    match &args.output {
        Some(path) => {
            if let Err(e) = std::fs::write(path, &line) {
                eprintln!("hs generate-signing-key: failed to write {path:?}: {e}");
                return 1;
            }
        }
        None => print!("{line}"),
    }
    0
}

/// `hs recover`: the link on stdout and nothing else there, so `$(hs recover)` is the link;
/// what it is and how long it lasts on stderr.
async fn run_recover(args: &RecoverArgs) -> i32 {
    let key_path = match crate::recover::signing_key_path(
        args.signing_key.as_deref(),
        args.data_dir.as_deref(),
        |name| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        },
    ) {
        Some(path) => path,
        None => {
            eprintln!(
                "hs recover: nowhere to look for the server's signing key. Run this where the \
                 server keeps it (its pod or container), or say where with --signing-key <file \
                 or directory> or --data-dir <the directory hs serve was given>"
            );
            return 1;
        }
    };
    let Some(key) = crate::identity::load_signing_key(&key_path) else {
        eprintln!(
            "hs recover: no ed25519 signing key at {}. The server writes one to <data-dir>/keys \
             on its first start; --signing-key names another file or directory",
            key_path.display()
        );
        return 1;
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    let request = crate::recover::signed_request(&key, now_ms);
    let client = reqwest::Client::new();
    match crate::recover::request_link(&client, &args.server_url, &request).await {
        Ok(link) => {
            println!("{}", link.link);
            match link.kind {
                hs_admin::model::RecoveryLinkKind::Recovery => {
                    let minutes = link
                        .expires_at_ms
                        .map_or(0, |at| at.saturating_sub(now_ms) / 60_000);
                    eprintln!(
                        "Open this link to reset an administrator's password and sign in as \
                         them. It works once and expires in {minutes} minutes; running hs \
                         recover again replaces it."
                    );
                }
                hs_admin::model::RecoveryLinkKind::Setup => {
                    eprintln!(
                        "This server has no active administrator, so this is its setup link: \
                         open it to create one. It works once."
                    );
                }
            }
            0
        }
        Err(e) => {
            eprintln!("hs recover: {e}");
            1
        }
    }
}

async fn run_register(args: &RegisterArgs) -> i32 {
    let username = match &args.user {
        Some(u) => u.clone(),
        None => match prompt("Username") {
            Ok(u) => u,
            Err(e) => {
                eprintln!("hs register: failed to read username: {e}");
                return 1;
            }
        },
    };
    let password = match &args.password {
        Some(p) => p.clone(),
        None => match prompt_password("Password") {
            Ok(p) => p,
            Err(e) => {
                eprintln!("hs register: failed to read password: {e}");
                return 1;
            }
        },
    };
    let shared_secret = if let Some(secret) = &args.shared_secret {
        secret.clone()
    } else if let Some(config_path) = &args.config {
        match crate::config_bridge::read_shared_secret_from_config(config_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("hs register: {e}");
                return 1;
            }
        }
    } else {
        eprintln!("hs register: one of -k/--shared-secret or -c/--config is required");
        return 1;
    };

    let client = reqwest::Client::new();
    let req = crate::register::RegisterRequest {
        server_url: args.server_url.trim_end_matches('/'),
        shared_secret: &shared_secret,
        username: &username,
        password: &password,
        admin: args.admin,
        user_type: args.user_type.as_deref(),
    };
    match crate::register::register(&client, &req).await {
        Ok(user) => {
            println!("{}", user.user_id);
            if args.verbose {
                println!("access_token: {}", user.access_token);
                if let Some(device_id) = &user.device_id {
                    println!("device_id: {device_id}");
                }
            }
            0
        }
        Err(e) => {
            eprintln!("hs register: {e}");
            1
        }
    }
}

/// Boots through the configuration layers (`crate::bootstrap`) and serves on what comes out.
///
/// The order is the interesting part: the bootstrap file and `HS__` environment are read first
/// because they say where the database is, the database is opened and — on a first run — seeded
/// from that file, and only then is the configuration resolved for real, with the database as the
/// layer that outranks the file. That is what makes a setting changed in the web interface
/// survive a restart while a `homeserver.yaml` nobody remembers is mounted still says otherwise.
async fn run_serve(args: &ServeArgs) -> i32 {
    let source = if let Some(synapse_path) = &args.synapse_config {
        match crate::synapse_serve::load_synapse_config(
            synapse_path,
            args.allow_unsupported_synapse_config,
            std::env::vars(),
        ) {
            Ok(result) => {
                print_translation_report(&result.report, args);
                crate::bootstrap::ConfigSource::Translated {
                    path: synapse_path.clone(),
                    config: Box::new(result.config),
                }
            }
            Err(e) => {
                eprintln!("{e}");
                if matches!(e, crate::synapse_serve::SynapseServeError::Unsupported(_)) {
                    eprintln!(
                        "See docs/compat/synapse-config-table.md for the full classification."
                    );
                }
                return 1;
            }
        }
    } else if let Some(config_path) = &args.config {
        crate::bootstrap::ConfigSource::Native(config_path.clone())
    } else {
        crate::bootstrap::ConfigSource::None
    };

    let booted = match crate::bootstrap::boot(&crate::bootstrap::BootOptions {
        source,
        data_dir: args.data_dir.clone(),
        server_name: args.server_name.clone(),
    }) {
        Ok(booted) => booted,
        Err(e) => {
            eprintln!("hs serve: {e}");
            return 1;
        }
    };
    let config = match booted.resolve() {
        Ok(resolved) => resolved.config,
        Err(e) => {
            eprintln!("hs serve: {e}");
            return 1;
        }
    };

    let telemetry_options =
        crate::config_bridge::telemetry_options_from(&config, "hs", env!("CARGO_PKG_VERSION"));
    let telemetry_guard = match hs_telemetry::init(&telemetry_options) {
        Ok(guard) => Some(guard),
        Err(e) => {
            eprintln!("hs serve: failed to initialize telemetry: {e}");
            None
        }
    };

    // Logged only now: telemetry is configured from the very configuration these lines describe,
    // so anything said before this point would go nowhere.
    for note in &booted.notes {
        tracing::warn!("{note}");
    }
    if let Some(seeded_from) = &booted.seeded {
        tracing::info!(
            source = %seeded_from,
            "first run: copied this configuration into the database. From here on the database \
             is what this server reads and what the web interface writes; the file is only \
             consulted for settings the database does not hold."
        );
    }
    tracing::info!(
        revision = booted.meta.revision,
        "configuration resolved from file, database and environment"
    );

    // The store is a handle on the same open backend `spawn_serve_with_storage` is about to serve
    // from, and it is what the admin API's configuration surface writes through: this is the line
    // that turns the management interface from something that displays the configuration into
    // something that changes it.
    // And what makes a saved change take effect without a restart, where one can: the source
    // applies every change it writes or reads back to this, and the server wires into it the
    // parts of itself that re-read a setting (`crate::live_config`).
    let live_config = std::sync::Arc::new(crate::live_config::LiveConfig::new(config.clone()));
    // The log level, swapped into the running filter -- unless `RUST_LOG` set it, which outranks
    // the configuration; a change then waits for a start without it, and is reported that way.
    if let Some(guard) = &telemetry_guard {
        let log_level = guard.log_level();
        if log_level.pinned_by_env() {
            tracing::info!(
                "RUST_LOG sets the log filter, so a change to telemetry.logging.level takes effect only at a start without it"
            );
        } else {
            live_config.on_change("telemetry", move |config| {
                let level = crate::config_bridge::log_level_from(config);
                log_level.set_level(level).map_err(|e| e.to_string())?;
                tracing::info!(?level, "the log level is now in force");
                Ok(())
            });
        }
    }
    let config_source = std::sync::Arc::new(
        crate::config_source::StoreConfigSource::new(
            booted.layers,
            booted.store,
            booted.meta,
            config.clone(),
        )
        .with_live(live_config.clone()),
    );
    let options = crate::serve::ServeOptions {
        capabilities_config: args.capabilities_config.clone(),
        routes_manifest_path: args.routes_manifest.clone(),
        media_scanning_config: args.media_scanning_config.clone(),
        config_source: Some(config_source.clone()),
        federation_scheme: None,
        migration_configs: Some(std::sync::Arc::new(crate::migration::StoreSourceConfigs(
            config_source,
        ))),
        media_bulk_pause: std::time::Duration::ZERO,
        live_config: Some(live_config),
    };
    let handle = match crate::serve::spawn_serve_with_storage(booted.storage, config, options).await
    {
        Ok(h) => h,
        Err(e) => {
            eprintln!("hs serve: {e}");
            return 1;
        }
    };
    for addr in &handle.addrs {
        tracing::info!(%addr, "listening");
    }
    if hs_admin::assets::EMBEDDED_UI == hs_admin::assets::EmbeddedUi::Placeholder {
        // Said once, here, rather than left for an operator to discover by opening /admin/ and
        // finding a page about build steps. Release builds cannot reach this: they set
        // HS_ADMIN_WEB_DIST, and `hs-admin`'s build script fails without a built interface.
        tracing::warn!(
            "this binary was built without the management interface: /admin/ serves a placeholder. The admin API at /api/v1 is unaffected. Build `web/` and rebuild to include it"
        );
    }
    if let Some(link) = &handle.setup_link {
        // `warn`, not `info`: it is the one line of a first boot the operator must act on, and
        // it should survive a deployment that has turned the log level down. It stops appearing
        // the moment an administrator exists.
        tracing::warn!(
            setup_link = %link,
            "this server has no administrator yet: open the setup link to create one. It works once, for whoever opens it first"
        );
    }

    wait_for_shutdown_signal().await;
    tracing::info!("shutdown signal received, draining connections");
    handle.shutdown().await;
    0
}

fn print_translation_report(report: &hs_compat::TranslationReport, args: &ServeArgs) {
    let text = if args.translation_report == "json" {
        serde_json::to_string_pretty(&report_as_json(report))
            .unwrap_or_else(|_| "<failed to render JSON report>".to_owned())
    } else {
        report.to_markdown()
    };
    match &args.translation_report_out {
        Some(path) => {
            if let Err(e) = std::fs::write(path, &text) {
                eprintln!("hs serve: failed to write translation report to {path:?}: {e}");
            }
        }
        None => eprintln!("{text}"),
    }
}

fn report_as_json(report: &hs_compat::TranslationReport) -> serde_json::Value {
    serde_json::Value::Array(
        report
            .outcomes
            .iter()
            .map(|o| {
                serde_json::json!({
                    "key": o.key,
                    "status": o.classification.to_string(),
                    "native": o.native,
                    "note": o.note,
                })
            })
            .collect(),
    )
}

/// Waits for SIGTERM (Unix) or Ctrl+C, whichever comes first.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = sigterm.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_serve_with_native_config() {
        let cli = Cli::parse_from(["hs", "serve", "-c", "config.yaml"]);
        match cli.command {
            Command::Serve(args) => assert_eq!(args.config, Some(PathBuf::from("config.yaml"))),
            _ => panic!("expected Serve"),
        }
    }

    #[test]
    fn parses_serve_with_synapse_config_and_allow_unsupported() {
        let cli = Cli::parse_from([
            "hs",
            "serve",
            "--synapse-config",
            "homeserver.yaml",
            "--allow-unsupported-synapse-config",
        ]);
        match cli.command {
            Command::Serve(args) => {
                assert_eq!(args.synapse_config, Some(PathBuf::from("homeserver.yaml")));
                assert!(args.allow_unsupported_synapse_config);
            }
            _ => panic!("expected Serve"),
        }
    }

    #[test]
    fn config_and_synapse_config_are_mutually_exclusive() {
        let result =
            Cli::try_parse_from(["hs", "serve", "-c", "a.yaml", "--synapse-config", "b.yaml"]);
        assert!(result.is_err());
    }

    #[test]
    fn parses_register_flags() {
        let cli = Cli::parse_from([
            "hs",
            "register",
            "-u",
            "alice",
            "-p",
            "hunter2",
            "-a",
            "-k",
            "sharedsecret",
            "https://matrix.example.org",
        ]);
        match cli.command {
            Command::Register(args) => {
                assert_eq!(args.user.as_deref(), Some("alice"));
                assert!(args.admin);
                assert_eq!(args.server_url, "https://matrix.example.org");
            }
            _ => panic!("expected Register"),
        }
    }

    /// The first-run invocation, which has to keep parsing exactly as documented.
    #[test]
    fn serve_parses_a_first_run_with_no_config_file() {
        let cli = Cli::parse_from([
            "hs",
            "serve",
            "--data-dir",
            "/var/lib/myelin",
            "--server-name",
            "example.org",
        ]);
        match cli.command {
            Command::Serve(args) => {
                assert!(args.config.is_none());
                assert_eq!(args.data_dir, Some(PathBuf::from("/var/lib/myelin")));
                assert_eq!(args.server_name.as_deref(), Some("example.org"));
            }
            _ => panic!("expected Serve"),
        }
    }

    #[test]
    fn config_subcommands_parse() {
        let cli = Cli::parse_from([
            "hs",
            "config",
            "--data-dir",
            "/var/lib/myelin",
            "set",
            "/auth/enable_registration",
            "true",
        ]);
        match cli.command {
            Command::Config(args) => {
                assert_eq!(args.data_dir, Some(PathBuf::from("/var/lib/myelin")));
                match args.command {
                    ConfigCommand::Set(set) => {
                        assert_eq!(set.pointer, "/auth/enable_registration");
                        assert_eq!(set.value, "true");
                    }
                    other => panic!("expected Set, got {other:?}"),
                }
            }
            _ => panic!("expected Config"),
        }

        // The bootstrap flags are global, so they read the same after the subcommand too.
        let cli = Cli::parse_from(["hs", "config", "show", "--format", "json", "-c", "a.yaml"]);
        match cli.command {
            Command::Config(args) => {
                assert_eq!(args.config, Some(PathBuf::from("a.yaml")));
                match args.command {
                    ConfigCommand::Show(show) => {
                        assert_eq!(show.format, crate::config_cmd::ShowFormat::Json);
                    }
                    other => panic!("expected Show, got {other:?}"),
                }
            }
            _ => panic!("expected Config"),
        }
    }

    #[test]
    fn generate_config_takes_a_data_dir_and_defaults_it() {
        let cli = Cli::parse_from(["hs", "generate-config", "--server-name", "example.org"]);
        match cli.command {
            Command::GenerateConfig(args) => assert_eq!(args.data_dir, PathBuf::from("./data")),
            _ => panic!("expected GenerateConfig"),
        }
    }

    #[test]
    fn version_subcommand_parses() {
        let cli = Cli::parse_from(["hs", "version"]);
        assert!(matches!(cli.command, Command::Version));
    }

    #[test]
    fn routes_manifest_subcommand_parses_with_optional_output() {
        let cli = Cli::parse_from(["hs", "routes-manifest", "-o", "routes.json"]);
        match cli.command {
            Command::RoutesManifest(args) => {
                assert_eq!(args.output, Some(PathBuf::from("routes.json")));
            }
            _ => panic!("expected RoutesManifest"),
        }
    }

    #[test]
    fn serve_parses_capabilities_config_and_routes_manifest_flags() {
        let cli = Cli::parse_from([
            "hs",
            "serve",
            "-c",
            "config.yaml",
            "--capabilities-config",
            "caps.yaml",
            "--routes-manifest",
            "routes.json",
        ]);
        match cli.command {
            Command::Serve(args) => {
                assert_eq!(args.capabilities_config, Some(PathBuf::from("caps.yaml")));
                assert_eq!(args.routes_manifest, Some(PathBuf::from("routes.json")));
            }
            _ => panic!("expected Serve"),
        }
    }
}
