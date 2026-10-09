//! `gen-crds`: renders every CRD in [`hs_operator::crds::all_crds`] to
//! `deploy/crds/<kind>.yaml`, one `CustomResourceDefinition` manifest per file (`kubectl apply
//! -f deploy/crds/` applies all five), and copies the `Bridge` CRD into the Helm chart's
//! `deploy/helm/hs/files/crds/` (the chart's `templates/crds.yaml` renders that one on every
//! install and upgrade; RFC 0017, decision 0036). Run with `cargo run -p
//! hs-operator --bin gen-crds` from the workspace root after changing any CRD's Rust struct, and
//! commit the regenerated YAML. `crds::tests::generated_files_are_up_to_date` fails when they
//! drift.

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("deploy/crds"));
    std::fs::create_dir_all(&out_dir)?;

    for (label, crd) in hs_operator::crds::all_crds() {
        let text = hs_operator::crds::render(label, &crd)?;
        let path = out_dir.join(format!("{label}.yaml"));
        std::fs::write(&path, &text)?;
        println!("wrote {}", path.display());
        if label == "bridge" && std::env::args().nth(1).is_none() {
            let chart = PathBuf::from(hs_operator::crds::CHART_BRIDGE_CRD);
            std::fs::write(&chart, &text)?;
            println!("wrote {}", chart.display());
        }
    }
    Ok(())
}
