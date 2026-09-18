//! CLI for `hs-spec-coverage`: loads the Matrix spec's OpenAPI trees, diffs them against a
//! `routes.json` manifest, and writes a Markdown report (and optionally a JSON summary for
//! machine consumers such as `tools/dashboard.py`).
//!
//! ```text
//! hs-spec-coverage [--spec-dir DIR] [--routes FILE] [--out FILE] [--json-out FILE]
//!                   [--missing-limit N] [--fail-on-missing]
//! ```
//!
//! - `--spec-dir` (default `refs/matrix-spec/data/api`): the directory holding
//!   `client-server/`, `server-server/`, `application-service/`, `identity/`, `push-gateway/`.
//! - `--routes` (optional): a `routes.json` manifest (RFC 0005). If omitted, coverage is computed
//!   against an empty manifest — every spec route reports missing, which is the honest answer on
//!   a day no crate has mounted the Matrix surfaces yet.
//! - `--out` (optional): where to write the Markdown report; defaults to stdout.
//! - `--json-out` (optional): also write a compact JSON summary (per-family totals) here.
//! - `--missing-limit` (default 200): cap on missing routes listed per family in the Markdown
//!   report before collapsing to a count.
//! - `--fail-on-missing`: exit 1 if any spec route is missing. Off by default, since day one this
//!   would fail on every run; opt in once a track's routes are meant to be complete.

use std::path::PathBuf;
use std::process::ExitCode;

use hs_spec_coverage::{CoverageReport, RouteManifest, load_all, render_markdown};

struct Args {
    spec_dir: PathBuf,
    routes: Option<PathBuf>,
    out: Option<PathBuf>,
    json_out: Option<PathBuf>,
    missing_limit: usize,
    fail_on_missing: bool,
}

fn parse_args() -> Args {
    let mut spec_dir = PathBuf::from("refs/matrix-spec/data/api");
    let mut routes = None;
    let mut out = None;
    let mut json_out = None;
    let mut missing_limit = 200usize;
    let mut fail_on_missing = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--spec-dir" => {
                spec_dir = PathBuf::from(args.next().expect("--spec-dir needs a value"))
            }
            "--routes" => {
                routes = Some(PathBuf::from(args.next().expect("--routes needs a value")))
            }
            "--out" => out = Some(PathBuf::from(args.next().expect("--out needs a value"))),
            "--json-out" => {
                json_out = Some(PathBuf::from(
                    args.next().expect("--json-out needs a value"),
                ))
            }
            "--missing-limit" => {
                missing_limit = args
                    .next()
                    .expect("--missing-limit needs a value")
                    .parse()
                    .expect("--missing-limit must be a number");
            }
            "--fail-on-missing" => fail_on_missing = true,
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => {
                eprintln!("unrecognized argument: {other}");
                print_help();
                std::process::exit(2);
            }
        }
    }

    Args {
        spec_dir,
        routes,
        out,
        json_out,
        missing_limit,
        fail_on_missing,
    }
}

fn print_help() {
    eprintln!(
        "hs-spec-coverage [--spec-dir DIR] [--routes FILE] [--out FILE] [--json-out FILE] \
         [--missing-limit N] [--fail-on-missing]"
    );
}

fn main() -> ExitCode {
    let args = parse_args();

    let spec_routes = match load_all(&args.spec_dir) {
        Ok(routes) => routes,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    let manifest = match &args.routes {
        Some(path) => match RouteManifest::load(path) {
            Ok(manifest) => manifest,
            Err(err) => {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
        },
        None => {
            eprintln!(
                "note: no --routes given; treating every spec route as unregistered (pass \
                 --routes routes.json once something emits one, see docs/rfcs/0005-routes-json-manifest.md)"
            );
            RouteManifest::empty()
        }
    };

    let report = CoverageReport::compute(&spec_routes, &manifest);
    let markdown = render_markdown(&report, args.missing_limit);

    match &args.out {
        Some(path) => {
            if let Err(err) = std::fs::write(path, &markdown) {
                eprintln!("error writing {}: {err}", path.display());
                return ExitCode::FAILURE;
            }
            eprintln!("wrote {}", path.display());
        }
        None => println!("{markdown}"),
    }

    if let Some(path) = &args.json_out {
        let summary = serde_json::json!({
            "overall_percent": report.overall_percent(),
            "total_spec_routes": report.total_spec_routes(),
            "total_registered": report.total_registered(),
            "apis": report.apis.iter().map(|a| serde_json::json!({
                "family": a.family.to_string(),
                "spec_total": a.spec_total,
                "registered": a.registered,
                "missing": a.missing.len(),
                "extra": a.extra.len(),
                "percent": a.percent(),
            })).collect::<Vec<_>>(),
        });
        let text = serde_json::to_string_pretty(&summary).expect("summary is always serializable");
        if let Err(err) = std::fs::write(path, text) {
            eprintln!("error writing {}: {err}", path.display());
            return ExitCode::FAILURE;
        }
        eprintln!("wrote {}", path.display());
    }

    eprintln!(
        "{} / {} spec routes registered ({:.1}%)",
        report.total_registered(),
        report.total_spec_routes(),
        report.overall_percent()
    );

    if args.fail_on_missing && report.total_registered() < report.total_spec_routes() {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
