//! Chooses which management interface this build embeds, and stages it where
//! `src/assets.rs` can `#[derive(RustEmbed)]` over it.
//!
//! The interface is a separate build (`cd web && npm ci && npm run build` writes `web/dist`), and
//! `web/dist` is gitignored, so a clean checkout has nothing to embed. Until this script existed
//! the answer to that was to embed a checked-in placeholder page *always* -- which kept
//! `cargo build` working everywhere and meant that every binary and every published image served
//! "the management interface has not been built into this binary yet" at `/admin/`.
//!
//! In order of preference:
//!
//! 1. **`HS_ADMIN_WEB_DIST`**, a directory. Release builds set it (`deploy/Dockerfile`, the CD
//!    binaries job), and if it does not hold a built interface the build *fails*: a release must
//!    not be able to ship the placeholder by forgetting a step.
//! 2. **`web/dist`**, if it has been built. So `npm run build && cargo run` shows a developer the
//!    interface they just built, with nothing to configure.
//! 3. **The placeholder**, so that a Rust-only checkout, and CI's Rust jobs, still compile.
//!
//! Whichever it is gets copied to `$OUT_DIR/web-dist`, leaving out what a server has no use for:
//! source maps (2.8 MB of the 3.6 MB build) and the mock service worker.
//! `HS_ADMIN_WEB_UI` tells the crate which one it got, so the binary can say so at startup.

use std::path::{Path, PathBuf};
use std::{env, fs, io};

fn main() {
    println!("cargo:rerun-if-env-changed=HS_ADMIN_WEB_DIST");
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let staged = PathBuf::from(env::var_os("OUT_DIR").expect("set by cargo")).join("web-dist");
    let placeholder = manifest_dir.join("web-dist-placeholder");
    let local_build = manifest_dir.join("../../web/dist");

    let (source, kind) = match env::var_os("HS_ADMIN_WEB_DIST").filter(|v| !v.is_empty()) {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            assert!(
                dir.join("index.html").is_file(),
                "HS_ADMIN_WEB_DIST is {}, which has no index.html. Build the management \
                 interface first (cd web && npm ci && npm run build), or unset the variable to \
                 embed the placeholder.",
                dir.display()
            );
            (dir, "built")
        }
        None if local_build.join("index.html").is_file() => (local_build, "built"),
        None => (placeholder, "placeholder"),
    };
    // Watched whichever was chosen, so that building the interface later is noticed: cargo
    // re-runs this when a watched path appears, changes or disappears.
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("../../web/dist").display()
    );
    println!("cargo:rerun-if-changed={}", source.display());
    println!("cargo:rustc-env=HS_ADMIN_WEB_UI={kind}");

    if staged.exists() {
        fs::remove_dir_all(&staged).expect("clearing the previously staged interface");
    }
    copy_tree(&source, &staged).expect("staging the management interface");
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        let target = to.join(&name);
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if !is_not_for_serving(&name.to_string_lossy()) {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn is_not_for_serving(file_name: &str) -> bool {
    file_name.ends_with(".map") || file_name == "mockServiceWorker.js"
}
