//! Embeds the OpenAPI 3.1 document (RFC 0004) at compile time, so the running binary can serve
//! it at `GET /api/v1/openapi.yaml` and `.json` (RFC 0004 section 3.8) without a filesystem read.

/// The raw YAML, exactly as published at `crates/hs-admin/openapi/openapi.yaml`.
pub const DOCUMENT: &str = include_str!("../openapi/openapi.yaml");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_parses_as_yaml_and_has_paths() {
        let value: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(DOCUMENT).expect("embedded OpenAPI document must parse");
        assert!(
            value
                .get("paths")
                .and_then(|p| p.as_mapping())
                .map(|m| !m.is_empty())
                .unwrap_or(false)
        );
    }
}
