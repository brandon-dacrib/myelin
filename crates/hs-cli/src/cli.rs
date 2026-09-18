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
    /// Writes a minimal, valid native config file for a given server name.
    GenerateConfig(GenerateConfigArgs),
    /// Hashes a password the way the server would at registration time.
    HashPassword(HashPasswordArgs),
    /// Generates a new Ed25519 signing key in Synapse's `signing.key` text format.
    GenerateSigningKey(GenerateSigningKeyArgs),
    /// Registers a user against a running server via the shared-secret admin protocol.
    Register(RegisterArgs),
    /// Writes the `routes.json` manifest (`docs/rfcs/0005-routes-json-manifest.md`) without
    /// booting a server — routes are static, independent of runtime config.
    RoutesManifest(RoutesManifestArgs),
    /// Prints the `hs` version.
    Version,
}

/// `hs serve` arguments.
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Native `hs-config` YAML file to load.
    #[arg(short = 'c', long = "config")]
    pub config: Option<PathBuf>,

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

    /// An optional YAML file overriding the `unstable_features` advertised by
    /// `GET /_matrix/client/versions` (see `crate::versions`'s module doc for the file shape and
    /// why this cannot live in `-c`/`--config`'s native `hs-config` file).
    #[arg(long = "capabilities-config")]
    pub capabilities_config: Option<PathBuf>,

    /// Write the `routes.json` manifest here at startup, before binding any listener. Omit to
    /// skip writing it (use the `hs routes-manifest` subcommand instead, which needs no config
    /// and does not bind a socket).
    #[arg(long = "routes-manifest")]
    pub routes_manifest: Option<PathBuf>,

    /// An optional `media.scanning` YAML file (`hs_media::scanning::ScanningConfig::from_yaml`'s
    /// shape, e.g. `deploy/media-scanning/media-scanning.yaml`) attaching content scanning to the
    /// media repository. Omit for no scanning (`crate::media`'s module doc explains why this
    /// cannot live in `-c`/`--config`'s native config file yet).
    #[arg(long = "media-scanning-config")]
    pub media_scanning_config: Option<PathBuf>,
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

    /// Write the config here instead of stdout.
    #[arg(short = 'o', long = "output")]
    pub output: Option<PathBuf>,
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
        Command::GenerateConfig(args) => run_generate_config(&args),
        Command::HashPassword(args) => run_hash_password(&args),
        Command::GenerateSigningKey(args) => run_generate_signing_key(&args),
        Command::Register(args) => run_register(&args).await,
        Command::RoutesManifest(args) => run_routes_manifest(&args),
        Command::Serve(args) => run_serve(&args).await,
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

fn run_generate_config(args: &GenerateConfigArgs) -> i32 {
    let config = crate::generate_config::minimal_config(&args.server_name);
    let yaml = match crate::generate_config::render_yaml(&config, &args.server_name) {
        Ok(y) => y,
        Err(e) => {
            eprintln!("hs generate-config: {e}");
            return 1;
        }
    };
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

async fn run_serve(args: &ServeArgs) -> i32 {
    let config = if let Some(synapse_path) = &args.synapse_config {
        match crate::synapse_serve::load_synapse_config(
            synapse_path,
            args.allow_unsupported_synapse_config,
            std::env::vars(),
        ) {
            Ok(result) => {
                print_translation_report(&result.report, args);
                result.config
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
        match hs_config::Config::load(config_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("hs serve: {e}");
                return 1;
            }
        }
    } else {
        eprintln!("hs serve: one of -c/--config or --synapse-config is required");
        return 1;
    };

    let telemetry_options =
        crate::config_bridge::telemetry_options_from(&config, "hs", env!("CARGO_PKG_VERSION"));
    let _telemetry_guard = match hs_telemetry::init(&telemetry_options) {
        Ok(guard) => Some(guard),
        Err(e) => {
            eprintln!("hs serve: failed to initialize telemetry: {e}");
            None
        }
    };

    let options = crate::serve::ServeOptions {
        capabilities_config: args.capabilities_config.clone(),
        routes_manifest_path: args.routes_manifest.clone(),
        media_scanning_config: args.media_scanning_config.clone(),
    };
    let handle = match crate::serve::spawn_serve(config, options).await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("hs serve: {e}");
            return 1;
        }
    };
    for addr in &handle.addrs {
        tracing::info!(%addr, "listening");
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
