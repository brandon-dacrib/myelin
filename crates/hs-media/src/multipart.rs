//! A `multipart/mixed` parser for MSC3916 federation media responses.
//!
//! `GET /_matrix/federation/v1/media/{download,thumbnail}/{mediaId}` (spec since v1.11, formerly
//! MSC3916) replies with `multipart/mixed`: a first part carrying a small JSON metadata object
//! (currently always `{}`), and a second part carrying the actual media bytes with their own
//! `Content-Type`. This module parses that shape. It has no caller yet — track 06's federation
//! client, which this module is written ahead of (`docs/rfcs/0007-federation-media.md`) — but it
//! is complete, fuzzed (`fuzz/fuzz_targets/multipart_parse.rs`) and tested on its own here, since
//! it is one of this crate's two classic attack surfaces (`docs/workstreams/09-media.md`'s
//! "Risks": a malicious or compromised remote homeserver is exactly the attacker this parser must
//! survive intact).
//!
//! # Threat model
//!
//! The input is a response body from a *remote* server, so every byte is untrusted: this parser
//! must never panic, never allocate proportionally more than the input size, and never loop
//! unboundedly, regardless of how the boundary marker, headers or part framing are malformed.
//! [`parse`] achieves this by construction: `pos` strictly increases by at least the boundary
//! delimiter's length on every loop iteration (bounding iterations by
//! `body.len() / delimiter.len()`), [`MAX_PARTS`] caps the number of parts regardless, and
//! [`MAX_HEADER_BYTES`] caps how much of a part is scanned as header text before its body begins.
//!
//! # Known limitation
//!
//! Boundary delimiter lines are located by plain substring search, not full RFC 2046 line-start
//! anchoring (confirming the delimiter is preceded by a line break). A real MSC3916 boundary is
//! server-generated with enough entropy that an accidental collision inside binary media content
//! is not a practical concern, but a part's content *could* still be truncated early if it
//! happens to contain the exact delimiter bytes. Full line-anchored parsing is a follow-up once
//! track 06's federation client is the actual caller and can be tested against real servers; see
//! `docs/rfcs/0007-federation-media.md`.

use crate::error::MediaError;

/// Hard ceiling on the number of parts a body may contain, regardless of what the boundary
/// implies — defends against a maliciously tiny boundary string producing an enormous part count.
pub const MAX_PARTS: usize = 64;

/// Hard ceiling on how many bytes of a part are scanned looking for the header/body separator
/// before giving up — defends against a part with no `\r\n\r\n` at all forcing an O(n) scan of an
/// arbitrarily large body on every part boundary.
pub const MAX_HEADER_BYTES: usize = 8192;

/// One MIME part: its headers (order preserved, duplicates preserved — callers decide what to do
/// with a repeated header) and a borrowed slice of its raw body bytes (never copied; this parser
/// allocates only for header text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part<'a> {
    /// `(name, value)` pairs, in the order they appeared.
    pub headers: Vec<(String, String)>,
    /// The part's raw body, borrowed from the input.
    pub body: &'a [u8],
}

impl<'a> Part<'a> {
    /// The value of the first header matching `name`, case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A parsed `multipart/mixed` body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultipartMixed<'a> {
    /// The parts, in order.
    pub parts: Vec<Part<'a>>,
}

/// Extracts the `boundary` parameter from a `Content-Type` header value, verifying the base MIME
/// type is `multipart/mixed`.
///
/// # Errors
/// Returns [`MediaError::MalformedMultipart`] if the type is not `multipart/mixed` or no
/// (non-empty) `boundary=` parameter is present.
pub fn boundary_from_content_type(content_type: &str) -> Result<String, MediaError> {
    let mut parts = content_type.split(';');
    let mime = parts.next().unwrap_or("").trim();
    if !mime.eq_ignore_ascii_case("multipart/mixed") {
        return Err(MediaError::MalformedMultipart(format!(
            "expected multipart/mixed, got {mime:?}"
        )));
    }
    for param in parts {
        let param = param.trim();
        if let Some(value) = param
            .strip_prefix("boundary=")
            .or_else(|| param.strip_prefix("BOUNDARY="))
        {
            let value = value.trim_matches('"').trim();
            if !value.is_empty() {
                return Ok(value.to_string());
            }
        }
    }
    Err(MediaError::MalformedMultipart(
        "missing or empty boundary parameter".into(),
    ))
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

fn skip_newline(body: &[u8], idx: usize) -> usize {
    if body.get(idx..idx + 2) == Some(b"\r\n") {
        idx + 2
    } else if body.get(idx) == Some(&b'\n') {
        idx + 1
    } else {
        idx
    }
}

fn split_headers_body(raw: &[u8]) -> (Vec<(String, String)>, &[u8]) {
    let scan_limit = raw.len().min(MAX_HEADER_BYTES);
    let scan = &raw[..scan_limit];
    let (header_end, body_start) = match find(scan, b"\r\n\r\n", 0) {
        Some(i) => (i, i + 4),
        None => match find(scan, b"\n\n", 0) {
            Some(i) => (i, i + 2),
            // No separator found within the scan window: treat the whole thing as bodyless
            // headers rather than guessing — safer than mis-splitting a binary body as text.
            None => (0, 0),
        },
    };
    let header_text = String::from_utf8_lossy(&raw[..header_end]);
    let mut headers = Vec::new();
    for line in header_text.split(['\r', '\n']) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    (headers, &raw[body_start..])
}

/// Parses `body` as `multipart/mixed` framed by `boundary` (as extracted by
/// [`boundary_from_content_type`]). See the module docs for the safety argument and known
/// limitation.
///
/// # Errors
/// Returns [`MediaError::MalformedMultipart`] if `boundary` is empty or implausibly long, if no
/// opening delimiter is found at all, if a part is never terminated by a closing delimiter, or if
/// more than [`MAX_PARTS`] parts are found.
pub fn parse<'a>(boundary: &str, body: &'a [u8]) -> Result<MultipartMixed<'a>, MediaError> {
    if boundary.is_empty() || boundary.len() > 200 {
        return Err(MediaError::MalformedMultipart(
            "boundary length out of range".into(),
        ));
    }
    let delim = format!("--{boundary}");
    let delim = delim.as_bytes();

    let Some(mut pos) = find(body, delim, 0) else {
        return Err(MediaError::MalformedMultipart(
            "no boundary delimiter found in body".into(),
        ));
    };

    let mut parts = Vec::new();
    loop {
        let after_delim = pos + delim.len();
        if body.get(after_delim..after_delim + 2) == Some(b"--") {
            // Closing delimiter (`--boundary--`): done.
            break;
        }
        let cursor = skip_newline(body, after_delim);
        let Some(next_pos) = find(body, delim, cursor) else {
            return Err(MediaError::MalformedMultipart(
                "final part has no closing boundary".into(),
            ));
        };
        let mut end = next_pos;
        if end >= 2 && &body[end - 2..end] == b"\r\n" {
            end -= 2;
        } else if end >= 1 && body[end - 1] == b'\n' {
            end -= 1;
        }
        // `cursor` (from `skip_newline`, at or after `after_delim`) is never greater than `end`
        // (which is at or before `next_pos`, itself found searching from `cursor` onward), so
        // this slice is always in-bounds and non-inverted.
        let raw = &body[cursor..end.max(cursor)];
        let (headers, part_body) = split_headers_body(raw);
        parts.push(Part {
            headers,
            body: part_body,
        });
        if parts.len() > MAX_PARTS {
            return Err(MediaError::MalformedMultipart("too many parts".into()));
        }
        pos = next_pos;
    }

    Ok(MultipartMixed { parts })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crlf_body() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"--BOUNDARY\r\n");
        b.extend_from_slice(b"Content-Type: application/json\r\n\r\n");
        b.extend_from_slice(b"{}");
        b.extend_from_slice(b"\r\n--BOUNDARY\r\n");
        b.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
        b.extend_from_slice(&[0x89, 0x50, 0x4E, 0x47, 0x00, 0x01, 0x02]);
        b.extend_from_slice(b"\r\n--BOUNDARY--\r\n");
        b
    }

    #[test]
    fn boundary_extraction_happy_path() {
        let b = boundary_from_content_type("multipart/mixed; boundary=\"BOUNDARY\"").unwrap();
        assert_eq!(b, "BOUNDARY");
        let b2 = boundary_from_content_type("multipart/mixed;boundary=BOUNDARY").unwrap();
        assert_eq!(b2, "BOUNDARY");
    }

    #[test]
    fn boundary_extraction_rejects_wrong_mime_type() {
        assert!(boundary_from_content_type("application/json").is_err());
        assert!(boundary_from_content_type("multipart/form-data; boundary=x").is_err());
    }

    #[test]
    fn boundary_extraction_rejects_missing_parameter() {
        assert!(boundary_from_content_type("multipart/mixed").is_err());
        assert!(boundary_from_content_type("multipart/mixed; boundary=").is_err());
    }

    #[test]
    fn parses_the_two_msc3916_parts() {
        let body = crlf_body();
        let parsed = parse("BOUNDARY", &body).unwrap();
        assert_eq!(parsed.parts.len(), 2);
        assert_eq!(
            parsed.parts[0].header("Content-Type"),
            Some("application/json")
        );
        assert_eq!(parsed.parts[0].body, b"{}");
        assert_eq!(parsed.parts[1].header("content-type"), Some("image/png"));
        assert_eq!(
            parsed.parts[1].body,
            &[0x89u8, 0x50, 0x4E, 0x47, 0x00, 0x01, 0x02][..]
        );
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let body = crlf_body();
        let parsed = parse("BOUNDARY", &body).unwrap();
        assert_eq!(
            parsed.parts[0].header("CONTENT-TYPE"),
            Some("application/json")
        );
    }

    #[test]
    fn lf_only_line_endings_also_parse() {
        let body = b"--B\nContent-Type: text/plain\n\nhello\n--B--\n".to_vec();
        let parsed = parse("B", &body).unwrap();
        assert_eq!(parsed.parts.len(), 1);
        assert_eq!(parsed.parts[0].body, b"hello");
    }

    #[test]
    fn empty_body_is_an_error_not_a_panic() {
        assert!(parse("BOUNDARY", b"").is_err());
    }

    #[test]
    fn body_with_no_boundary_at_all_is_an_error() {
        assert!(parse("BOUNDARY", b"just some random bytes, no delimiter here").is_err());
    }

    #[test]
    fn unterminated_part_is_an_error_not_a_panic() {
        let body = b"--B\r\nContent-Type: text/plain\r\n\r\nunterminated...".to_vec();
        assert!(parse("B", &body).is_err());
    }

    #[test]
    fn empty_boundary_is_rejected() {
        assert!(parse("", b"--\r\n\r\n--").is_err());
    }

    #[test]
    fn part_with_no_header_body_separator_does_not_panic() {
        let body = b"--B\r\nno separator here at all".to_vec();
        // Missing closing boundary -> Err, but must not panic while trying.
        assert!(parse("B", &body).is_err());
    }

    #[test]
    fn preamble_before_first_boundary_is_ignored() {
        let mut body = b"This is preamble text some servers send.\r\n".to_vec();
        body.extend_from_slice(&crlf_body());
        let parsed = parse("BOUNDARY", &body).unwrap();
        assert_eq!(parsed.parts.len(), 2);
    }

    #[test]
    fn too_many_parts_is_rejected() {
        let mut body = Vec::new();
        for _ in 0..(MAX_PARTS + 5) {
            body.extend_from_slice(b"--B\r\nContent-Type: text/plain\r\n\r\nx\r\n");
        }
        body.extend_from_slice(b"--B--\r\n");
        assert!(parse("B", &body).is_err());
    }

    // A lightweight fuzz-style sweep: arbitrary byte perturbations of a valid body must never
    // panic, whatever they decide to return.
    #[test]
    fn arbitrary_mutations_never_panic() {
        let base = crlf_body();
        for i in 0..base.len() {
            let mut mutated = base.clone();
            mutated[i] ^= 0xFF;
            let _ = parse("BOUNDARY", &mutated);
        }
        for cut in 0..base.len() {
            let _ = parse("BOUNDARY", &base[..cut]);
        }
    }
}
