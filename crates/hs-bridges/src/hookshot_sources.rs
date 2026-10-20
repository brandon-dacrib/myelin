//! The webhook sources a Hookshot connection can be made from: what posts to the URL, and the
//! transformation function that turns its payload into a notice (`hookshot-sources/<id>/` beside
//! this crate's `src`, embedded at build time).
//!
//! One Hookshot has one public URL and many webhooks, each a generic webhook connection in a
//! room (`uk.half-shot.matrix-hookshot.generic.hook`) whose `transformationFunction` Hookshot
//! runs on every payload: `data` is the decoded body, and the script sets `result` to
//! `{version: "v2", plain, html?, msgtype?, mentions?}` or `{version: "v2", empty: true}` for
//! nothing to say (Hookshot 7.5.0, `generic/WebhookTransformer.ts`: QuickJS, a 500 ms budget,
//! no `require` or network; Markdown in `plain` is rendered when `html` is not given). The
//! server writes the source's transform into the connection's state event and marks the event
//! with the source's id (`io.myelin.source`), so the listing can say where a webhook's payloads
//! come from.
//!
//! Each source directory has `README.md` (what to set on the sender's side, with `{{url}}` and
//! `{{name}}` for the connection's URL and name), `transform.js` (none for `generic`: Hookshot's
//! own rendering), one or more `sample-*.json` payloads the tests run the transform on with
//! `node`, and for `gmail` the Apps Script `Code.gs` the sender runs.

use std::collections::BTreeMap;

/// One webhook source: what posts to the URL, and how its payloads become notices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookshotSource {
    /// The id the admin API takes (`datadog`), a DNS-label-shaped word.
    pub id: &'static str,
    /// For the interface's cards (`Datadog monitors`).
    pub title: &'static str,
    /// One sentence on what it is for.
    pub description: &'static str,
    /// The sender's setup, Markdown, with `{{url}}` and `{{name}}` placeholders
    /// ([`HookshotSource::setup_for`]).
    pub setup: &'static str,
    /// The transformation function Hookshot runs on each payload; `None` posts the payload as
    /// Hookshot renders it (`text`, `username`, `html`, or the JSON as data).
    pub transform: Option<&'static str>,
    /// Sample payloads by file name, for the tests and the documentation.
    pub samples: &'static [(&'static str, &'static str)],
    /// Files the sender runs, by name, with the same placeholders as `setup` (`Code.gs`).
    pub files: &'static [(&'static str, &'static str)],
}

macro_rules! source_file {
    ($dir:literal, $name:literal) => {
        include_str!(concat!("../hookshot-sources/", $dir, "/", $name))
    };
}

/// Every source, in the order the interface lists them: the plain one first, then by name.
pub const SOURCES: &[HookshotSource] = &[
    HookshotSource {
        id: "generic",
        title: "Plain JSON",
        description: "Anything that posts JSON: `text` is the message, `username` who it is from, anything else is shown as data.",
        setup: source_file!("generic", "README.md"),
        transform: None,
        samples: &[(
            "sample-text.json",
            source_file!("generic", "sample-text.json"),
        )],
        files: &[],
    },
    HookshotSource {
        id: "alertmanager",
        title: "Prometheus Alertmanager",
        description: "Alertmanager's webhook receiver: each notification's alerts, firing or resolved, with severity, summary and links.",
        setup: source_file!("alertmanager", "README.md"),
        transform: Some(source_file!("alertmanager", "transform.js")),
        samples: &[
            (
                "sample-firing.json",
                source_file!("alertmanager", "sample-firing.json"),
            ),
            (
                "sample-resolved.json",
                source_file!("alertmanager", "sample-resolved.json"),
            ),
            (
                "sample-empty.json",
                source_file!("alertmanager", "sample-empty.json"),
            ),
        ],
        files: &[],
    },
    HookshotSource {
        id: "datadog",
        title: "Datadog monitors",
        description: "A monitor's alerts, warnings and recoveries through Datadog's Webhooks integration, with priority, tags and a link.",
        setup: source_file!("datadog", "README.md"),
        transform: Some(source_file!("datadog", "transform.js")),
        samples: &[
            (
                "sample-alert.json",
                source_file!("datadog", "sample-alert.json"),
            ),
            (
                "sample-recovered.json",
                source_file!("datadog", "sample-recovered.json"),
            ),
            (
                "sample-test.json",
                source_file!("datadog", "sample-test.json"),
            ),
        ],
        files: &[],
    },
    HookshotSource {
        id: "github-comments",
        title: "GitHub comments",
        description: "Comments and reviews on a repository's pull requests and issues, through a plain repository webhook; no GitHub App needed.",
        setup: source_file!("github-comments", "README.md"),
        transform: Some(source_file!("github-comments", "transform.js")),
        samples: &[
            (
                "sample-issue-comment.json",
                source_file!("github-comments", "sample-issue-comment.json"),
            ),
            (
                "sample-review.json",
                source_file!("github-comments", "sample-review.json"),
            ),
            (
                "sample-ping.json",
                source_file!("github-comments", "sample-ping.json"),
            ),
            (
                "sample-push.json",
                source_file!("github-comments", "sample-push.json"),
            ),
        ],
        files: &[],
    },
    HookshotSource {
        id: "gmail",
        title: "Gmail",
        description: "New threads under one Gmail label, posted by a Google Apps Script on a timer: sender, subject, snippet and a link.",
        setup: source_file!("gmail", "README.md"),
        transform: Some(source_file!("gmail", "transform.js")),
        samples: &[
            (
                "sample-thread.json",
                source_file!("gmail", "sample-thread.json"),
            ),
            (
                "sample-test.json",
                source_file!("gmail", "sample-test.json"),
            ),
            (
                "sample-empty.json",
                source_file!("gmail", "sample-empty.json"),
            ),
        ],
        files: &[("Code.gs", source_file!("gmail", "Code.gs"))],
    },
    HookshotSource {
        id: "linear",
        title: "Linear",
        description: "Issues, comments and projects as they are created, changed and deleted in Linear, with who did it.",
        setup: source_file!("linear", "README.md"),
        transform: Some(source_file!("linear", "transform.js")),
        samples: &[
            (
                "sample-issue-update.json",
                source_file!("linear", "sample-issue-update.json"),
            ),
            (
                "sample-comment.json",
                source_file!("linear", "sample-comment.json"),
            ),
            (
                "sample-project-create.json",
                source_file!("linear", "sample-project-create.json"),
            ),
            (
                "sample-label.json",
                source_file!("linear", "sample-label.json"),
            ),
        ],
        files: &[],
    },
    HookshotSource {
        id: "notion",
        title: "Notion",
        description: "Notion's webhook subscriptions: what kind of page, database or comment changed and how, with a link; Notion sends ids, not content.",
        setup: source_file!("notion", "README.md"),
        transform: Some(source_file!("notion", "transform.js")),
        samples: &[
            (
                "sample-verification.json",
                source_file!("notion", "sample-verification.json"),
            ),
            (
                "sample-page-updated.json",
                source_file!("notion", "sample-page-updated.json"),
            ),
            (
                "sample-comment-created.json",
                source_file!("notion", "sample-comment-created.json"),
            ),
            (
                "sample-unknown.json",
                source_file!("notion", "sample-unknown.json"),
            ),
        ],
        files: &[],
    },
];

/// The source with id `id`.
#[must_use]
pub fn get(id: &str) -> Option<&'static HookshotSource> {
    SOURCES.iter().find(|s| s.id == id)
}

/// The content of a connection's state event for `source`: Hookshot's `name` and
/// `transformationFunction`, and `io.myelin.source` naming the source (Hookshot's
/// `validateState` reads the keys it knows and ignores the rest).
#[must_use]
pub fn state_content(source: &HookshotSource, name: &str) -> serde_json::Value {
    let mut content = serde_json::Map::new();
    content.insert("name".into(), serde_json::Value::String(name.to_owned()));
    if let Some(transform) = source.transform {
        content.insert(
            "transformationFunction".into(),
            serde_json::Value::String(transform.to_owned()),
        );
    }
    content.insert(
        SOURCE_MARKER.into(),
        serde_json::Value::String(source.id.to_owned()),
    );
    serde_json::Value::Object(content)
}

/// The key in a connection's state content that names the source it was made from.
pub const SOURCE_MARKER: &str = "io.myelin.source";

impl HookshotSource {
    /// `setup` with the connection's URL and name filled in.
    #[must_use]
    pub fn setup_for(&self, url: &str, name: &str) -> String {
        fill(self.setup, url, name)
    }

    /// `files` with the connection's URL and name filled in, by name.
    #[must_use]
    pub fn files_for(&self, url: &str, name: &str) -> BTreeMap<String, String> {
        self.files
            .iter()
            .map(|(file, text)| ((*file).to_owned(), fill(text, url, name)))
            .collect()
    }
}

fn fill(text: &str, url: &str, name: &str) -> String {
    text.replace("{{url}}", url).replace("{{name}}", name)
}

/// A hook id as Hookshot mints one (`randomUUID()` in `GenericHookConnection`): a version 4
/// UUID, the secret last part of the webhook's URL.
#[must_use]
pub fn new_hook_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::process::Command;

    /// What a transform made of a sample: `Empty`, or the `plain` text to check.
    enum Expect {
        Empty,
        Plain(&'static [&'static str]),
    }

    /// Every sample of every transforming source, with what its notice must say. A source or
    /// sample missing here fails the test: a sample is only worth keeping with an expectation.
    const EXPECTED: &[(&str, &str, Expect)] = &[
        (
            "alertmanager",
            "sample-firing.json",
            Expect::Plain(&[
                "**FIRING** 2 firing (alertname=HighLatency, namespace=myelin)",
                "https://alertmanager.example.org/#/alerts",
                "- **HighLatency** `warning`: p99 /sync latency is 3.2s on hs-0 — `namespace=myelin pod=hs-0`",
                "[runbook](https://example.org/runbooks/latency)",
                "p99 /sync latency is 2.9s on hs-1",
            ]),
        ),
        (
            "alertmanager",
            "sample-resolved.json",
            Expect::Plain(&[
                "**RESOLVED** 1 resolved",
                "**HighLatency** `warning` (resolved)",
            ]),
        ),
        ("alertmanager", "sample-empty.json", Expect::Empty),
        (
            "datadog",
            "sample-alert.json",
            Expect::Plain(&[
                "**Alert** P2: [[Triggered on {host:web-1}] CPU is high on web-1](https://app.datadoghq.com/event/event?id=7357)",
                "CPU usage has been above 90% for 10 minutes.",
                "host `web-1` · scope `host:web-1` · tags `env:prod` `service:web` `team:platform`",
            ]),
        ),
        (
            "datadog",
            "sample-recovered.json",
            Expect::Plain(&[
                "**Recovered**: [[Recovered on",
                "CPU usage is back under 90%.",
            ]),
        ),
        ("datadog", "sample-test.json", Expect::Empty),
        (
            "github-comments",
            "sample-issue-comment.json",
            Expect::Plain(&[
                "**octocat** commented on PR [#12 Receipts loop through the pump](https://github.com/example/myelin/pull/12#issuecomment-1):",
                "Looks right to me; one nit on the metric name.",
            ]),
        ),
        (
            "github-comments",
            "sample-review.json",
            Expect::Plain(&[
                "**octocat** approved PR [#12 Receipts loop through the pump]",
                "Ship it.",
            ]),
        ),
        (
            "github-comments",
            "sample-ping.json",
            Expect::Plain(&["GitHub webhook for example/myelin is connected."]),
        ),
        ("github-comments", "sample-push.json", Expect::Empty),
        (
            "gmail",
            "sample-thread.json",
            Expect::Plain(&[
                "**Ada Lovelace**: [Invoice 2026-10 for the cluster](https://mail.google.com/mail/u/0/#inbox/18f1b2c3d4e5f607)",
                "Hi, attached is this month's invoice",
                "`Billing`",
            ]),
        ),
        (
            "gmail",
            "sample-test.json",
            Expect::Plain(&["Gmail script connected (label `to-matrix`)."]),
        ),
        ("gmail", "sample-empty.json", Expect::Empty),
        (
            "linear",
            "sample-issue-update.json",
            Expect::Plain(&[
                "**Ada Lovelace** moved to **In Progress**: [ENG-42 Receipts loop through the pump](https://linear.app/example/issue/ENG-42/receipts-loop-through-the-pump)",
            ]),
        ),
        (
            "linear",
            "sample-comment.json",
            Expect::Plain(&[
                "**Grace Hopper** commented on [ENG-42 Receipts loop through the pump]",
                "Reproduced on the demo; the receipt comes back as the user's own.",
            ]),
        ),
        (
            "linear",
            "sample-project-create.json",
            Expect::Plain(&[
                "**Ada Lovelace** created project [Q4 launch](https://linear.app/example/project/q4-launch-abc123) (started)",
            ]),
        ),
        ("linear", "sample-label.json", Expect::Empty),
        (
            "notion",
            "sample-verification.json",
            Expect::Plain(&[
                "verification token",
                "`secret_EXAMPLE-not-a-real-token`",
            ]),
        ),
        (
            "notion",
            "sample-page-updated.json",
            Expect::Plain(&[
                "Notion: page content changed · `153104cd-477e-80d0-a893-d4fd3faa2d7a` · 2026-10-20 10:00:10 UTC · [open](https://www.notion.so/153104cd477e80d0a893d4fd3faa2d7a)",
                "1 block changed",
            ]),
        ),
        (
            "notion",
            "sample-comment-created.json",
            Expect::Plain(&[
                "Notion: comment created",
                "[open](https://www.notion.so/153104cd477e80d0a893d4fd3faa2d7a)",
            ]),
        ),
        ("notion", "sample-unknown.json", Expect::Empty),
    ];

    /// Runs `transform` on `sample` the way Hookshot does (`const data = ...; (() => { script
    /// })();` and the global `result`), with `node`; `None` when `node` is not on `PATH`.
    fn run_transform(transform: &str, sample: &str) -> Option<Result<Value, String>> {
        let dir = tempfile::tempdir().ok()?;
        let script = format!(
            "const data = {sample};\n(() => {{ {transform} }})();\nprocess.stdout.write(JSON.stringify(globalThis.result === undefined ? null : globalThis.result));\n"
        );
        let path = dir.path().join("run.js");
        std::fs::write(&path, script).ok()?;
        let out = match Command::new("node").arg(&path).output() {
            Ok(out) => out,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => return Some(Err(e.to_string())),
        };
        if !out.status.success() {
            return Some(Err(String::from_utf8_lossy(&out.stderr).into_owned()));
        }
        Some(
            serde_json::from_slice(&out.stdout)
                .map_err(|e| format!("{e}: {}", String::from_utf8_lossy(&out.stdout))),
        )
    }

    #[test]
    fn every_source_is_well_formed() {
        let mut ids: Vec<&str> = SOURCES.iter().map(|s| s.id).collect();
        ids.dedup();
        assert_eq!(ids.len(), SOURCES.len(), "duplicate ids");
        for source in SOURCES {
            assert!(
                source
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'-'),
                "{}",
                source.id
            );
            assert!(!source.title.is_empty() && !source.description.is_empty());
            assert!(
                source.setup.contains("{{url}}"),
                "{}: the README names the URL",
                source.id
            );
            assert!(!source.samples.is_empty(), "{}: no samples", source.id);
            for (name, text) in source.samples {
                assert!(
                    name.starts_with("sample-") && name.ends_with(".json"),
                    "{name}"
                );
                serde_json::from_str::<Value>(text)
                    .unwrap_or_else(|e| panic!("{}/{name}: {e}", source.id));
            }
            if source.id == "generic" {
                assert!(source.transform.is_none());
            } else {
                assert!(source.transform.is_some(), "{}: no transform", source.id);
            }
        }
        assert_eq!(get("datadog").map(|s| s.title), Some("Datadog monitors"));
        assert!(get("nope").is_none());
    }

    #[test]
    fn the_setup_and_files_are_filled_in() {
        let gmail = get("gmail").unwrap();
        let url = "https://hooks.example.org/webhook/abc";
        let setup = gmail.setup_for(url, "mail");
        assert!(setup.contains(url) && !setup.contains("{{url}}"));
        let files = gmail.files_for(url, "mail");
        assert!(files["Code.gs"].contains(&format!("const WEBHOOK_URL = \"{url}\";")));
        assert!(get("generic").unwrap().files_for(url, "x").is_empty());
        let content = state_content(get("datadog").unwrap(), "alerts");
        assert_eq!(content["name"], "alerts");
        assert_eq!(content[SOURCE_MARKER], "datadog");
        assert!(
            content["transformationFunction"]
                .as_str()
                .unwrap()
                .contains("result =")
        );
        assert!(
            state_content(get("generic").unwrap(), "plain")
                .get("transformationFunction")
                .is_none()
        );
    }

    #[test]
    fn a_hook_id_is_a_version_4_uuid() {
        let id = new_hook_id();
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12],
            "{id}"
        );
        assert!(parts[2].starts_with('4'), "{id}");
        assert!(
            matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'),
            "{id}"
        );
        assert_ne!(id, new_hook_id());
    }

    /// Every transform, on every sample, with `node`: the notice says what the expectation
    /// says, and the payloads with nothing to say set `empty`. Skips without `node`.
    #[test]
    fn every_transform_renders_its_samples_as_expected() {
        let mut checked = 0;
        for source in SOURCES {
            let Some(transform) = source.transform else {
                continue;
            };
            for (sample_name, sample) in source.samples {
                let expect = EXPECTED
                    .iter()
                    .find(|(id, name, _)| *id == source.id && name == sample_name)
                    .map(|(_, _, e)| e)
                    .unwrap_or_else(|| panic!("{}/{sample_name} has no expectation", source.id));
                let Some(result) = run_transform(transform, sample) else {
                    eprintln!("SKIP: node is not on PATH; the transforms were not run");
                    return;
                };
                let result = result.unwrap_or_else(|e| panic!("{}/{sample_name}: {e}", source.id));
                assert_eq!(
                    result["version"], "v2",
                    "{}/{sample_name}: {result}",
                    source.id
                );
                match expect {
                    Expect::Empty => {
                        assert_eq!(
                            result["empty"], true,
                            "{}/{sample_name}: {result}",
                            source.id
                        );
                    }
                    Expect::Plain(needles) => {
                        assert!(
                            result.get("empty").is_none(),
                            "{}/{sample_name}: {result}",
                            source.id
                        );
                        let plain = result["plain"].as_str().unwrap_or_else(|| {
                            panic!("{}/{sample_name}: no plain in {result}", source.id)
                        });
                        assert_eq!(result["msgtype"], "m.notice", "{}/{sample_name}", source.id);
                        for needle in *needles {
                            assert!(
                                plain.contains(needle),
                                "{}/{sample_name}: {plain:?} lacks {needle:?}",
                                source.id
                            );
                        }
                    }
                }
                checked += 1;
            }
        }
        // Every expectation named a real sample.
        for (id, name, _) in EXPECTED {
            let source = get(id).unwrap_or_else(|| panic!("{id} is not a source"));
            assert!(
                source.samples.iter().any(|(n, _)| n == name),
                "{id}/{name} is not a sample"
            );
        }
        assert_eq!(checked, EXPECTED.len());
    }

    /// The transforms compile the way Hookshot checks them (`function f(data) { script }`) and
    /// use nothing QuickJS lacks: no `require`, no `fetch`, no `Intl`. Skips without `node`.
    #[test]
    fn every_transform_compiles_as_hookshot_checks_it_and_stays_inside_quickjs() {
        for source in SOURCES {
            let Some(transform) = source.transform else {
                continue;
            };
            for word in ["require(", "fetch(", "Intl.", "process.", "import "] {
                assert!(!transform.contains(word), "{}: uses {word}", source.id);
            }
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("compile.js");
            std::fs::write(
                &path,
                format!(
                    "new Function('data', {});\n",
                    serde_json::to_string(transform).unwrap()
                ),
            )
            .unwrap();
            let out = match Command::new("node").arg(&path).output() {
                Ok(out) => out,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    eprintln!("SKIP: node is not on PATH");
                    return;
                }
                Err(e) => panic!("{e}"),
            };
            assert!(
                out.status.success(),
                "{}: {}",
                source.id,
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
