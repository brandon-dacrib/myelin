//! Every outbound HTTP exchange in the server's code runs in `hs_telemetry::outside_spans`
//! (decision 0048, rule 6).
//!
//! `kube-client` turns on `hyper-util`'s `tracing` feature for the whole binary, and with it a
//! new connection's task is spawned `in_current_span()`: without the wrapper, the caller's span
//! (and the request span above it) lives as long as the pooled connection, and the trace is
//! never exported. Nothing in the type system stops a new call site from forgetting the
//! wrapper, so this test reads the workspace's sources: every `reqwest` send (`.send()` with no
//! arguments, or `reqwest::get(`) and every `hyper` client handshake (which spawns the
//! connection's driver the same way) outside test code must have `outside_spans` in its statement
//! or in the two lines above it (a comment saying where the wrapper is, when the future is
//! wrapped further on).

use std::path::{Path, PathBuf};

/// Crates that are test tools, not the server: they never run inside the server's spans.
const NOT_THE_SERVER: &[&str] = &[
    "hs-bridge-conformance",
    "hs-e2e",
    "hs-loadgen",
    "hs-spec-coverage",
    "hs-testkit",
];

/// Files whose sends are exempt, and why.
const EXEMPT_FILES: &[(&str, &str)] = &[
    // The `hs` CLI's client commands talk to a running server from a one-shot process that
    // opens no spans.
    ("hs-cli/src/register.rs", "a one-shot CLI command"),
    ("hs-cli/src/recover.rs", "a one-shot CLI command"),
    ("hs-cli/src/admin_tokens.rs", "a one-shot CLI command"),
];

/// The 1-based line numbers of sends in `source` that are not wrapped.
fn unwrapped_sends(source: &str) -> Vec<usize> {
    sends(source)
        .into_iter()
        .filter_map(|(line, wrapped)| (!wrapped).then_some(line))
        .collect()
}

/// Every send in `source` outside test code: its 1-based line number, and whether it is wrapped.
fn sends(source: &str) -> Vec<(usize, bool)> {
    let lines: Vec<&str> = source.lines().collect();
    // Test code ends the part of the file that is checked.
    let end = lines
        .iter()
        .enumerate()
        .position(|(i, line)| {
            line.trim() == "#[cfg(test)]"
                && lines
                    .get(i + 1)
                    .is_some_and(|next| next.trim_start().starts_with("mod "))
        })
        .unwrap_or(lines.len());
    let mut found = Vec::new();
    for i in 0..end {
        let line = lines[i].trim_start();
        if line.starts_with("//") {
            continue;
        }
        if !(line.contains(".send()")
            || line.contains("reqwest::get(")
            || line.contains("http1::handshake(")
            || line.contains("http2::handshake("))
        {
            continue;
        }
        // The statement: back to the line after the previous one that ended a statement or
        // opened a block.
        let mut start = i;
        while start > 0 {
            let previous = lines[start - 1].trim_end();
            if previous.ends_with(';') || previous.ends_with('{') || previous.ends_with('}') {
                break;
            }
            start -= 1;
        }
        let from = start.saturating_sub(2);
        found.push((
            i + 1,
            lines[from..=i].iter().any(|l| l.contains("outside_spans")),
        ));
    }
    found
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // A module's tests in a directory of their own.
            if path.file_name().is_some_and(|n| n == "tests") {
                continue;
            }
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs")
            && path.file_name().is_some_and(|n| n != "tests.rs")
        {
            out.push(path);
        }
    }
}

#[test]
fn every_outbound_http_send_in_the_server_runs_outside_spans() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .to_path_buf();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&crates)
        .expect("crates/ is readable")
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        if NOT_THE_SERVER.contains(&name.as_str()) {
            continue;
        }
        rust_files(&entry.path().join("src"), &mut files);
    }
    assert!(files.len() > 100, "found only {} files", files.len());

    let mut offenders = Vec::new();
    let mut checked = 0;
    for file in files {
        let relative = file
            .strip_prefix(&crates)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        if EXEMPT_FILES.iter().any(|(f, _)| *f == relative) {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(&file) else {
            continue;
        };
        for (line, wrapped) in sends(&source) {
            checked += 1;
            if !wrapped {
                offenders.push(format!("crates/{relative}:{line}"));
            }
        }
    }
    // The appservice, push, media, module, identity-server and federation sends at least.
    assert!(
        checked >= 15,
        "found only {checked} sends: is the check still finding them?"
    );
    assert!(
        offenders.is_empty(),
        "outbound HTTP sent without `hs_telemetry::outside_spans` (decision 0048, rule 6: a \
         connection it opens would hold the caller's spans open, and its trace would never be \
         exported):\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn the_check_finds_an_unwrapped_send_and_accepts_wrapped_ones() {
    let source = r#"
fn unwrapped() {
    let x = 1;
    let response = self
        .client
        .get(&url)
        .send()
        .await?;
}
fn wrapped() {
    let response = hs_telemetry::outside_spans(
        self.client
            .get(&url)
            .send(),
    )
    .await;
}
fn wrapped_further_on() {
    // Run below through `hs_telemetry::outside_spans`.
    let exchange = async {
        let response = request.send().await;
    };
}
fn not_http() {
    tx.send(value).await;
}
#[cfg(test)]
mod tests {
    fn in_a_test() {
        reqwest::get(url).await;
    }
}
"#;
    assert_eq!(unwrapped_sends(source), vec![7]);
}
