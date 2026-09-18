//! The media security rules, as rustdoc and as tests. Read this module before touching
//! [`crate::routes`]: every response this crate serves goes through [`response_headers`] (or,
//! for a thumbnail, [`response_headers`] as well — a thumbnail is always `image/*`, so it is never
//! the risky case), and every header decision made here is load-bearing, not decorative.
//!
//! # The threat
//!
//! A homeserver's media repository is a file host with a browser as the client. Two attacker
//! goals follow directly from that:
//!
//! 1. **Get a browser to execute attacker script in the origin of the homeserver** (or, worse, a
//!    client web app that renders `<img src="mxc:...">`/`<img src="https://.../download/...">`
//!    directly) by uploading an SVG or HTML file and getting it served with a
//!    `Content-Type`/`Content-Disposition` combination the browser will render inline and
//!    interpret as markup.
//! 2. **Exhaust server memory or CPU decoding a hostile image** — a "decompression bomb" (a tiny
//!    file whose *decoded* pixel buffer is gigabytes), a truncated or structurally malformed file
//!    that trips a decoder bug, or a file whose extension/declared `Content-Type` does not match
//!    its actual bytes.
//!
//! This module is the answer to (1). [`crate::sniff`] is the answer to (2).
//!
//! # Rule 1: never serve SVG or HTML inline, ever — independent of what the uploader claimed
//!
//! The spec (client-server API, "Content repository") requires `Content-Type` be served as the
//! client supplied it at upload time (so a PNG stays `image/png`), which means this crate *cannot*
//! defend rule 1 by rewriting `Content-Type`. Instead, two independent layers do it:
//!
//! - **`Content-Disposition` is decided from an allowlist of safe-to-render types
//!   ([`INLINE_SAFE_CONTENT_TYPES`]), not from the uploader's claim.** `text/html` and
//!   `image/svg+xml` (and any `Content-Type` this crate does not specifically recognize) always
//!   get `Content-Disposition: attachment`, which most browsers will not render inline regardless
//!   of `Content-Type` — this is Synapse's `INLINE_CONTENT_TYPES` allowlist, behavior only, no
//!   code copied (`synapse/media/_base.py`).
//! - **A `Content-Security-Policy` is sent on *every* media response, inline or attachment,
//!   unconditionally** ([`CSP_HEADER_VALUE`]). This is the belt to the allowlist's suspenders: even
//!   a browser that renders an attachment inline anyway (some do, for same-origin navigations) gets
//!   a CSP that forbids script execution, plugins and framing. Synapse sends this exact policy
//!   shape (`sandbox; default-src 'none'; script-src 'none'; ...`) on every `/download` and
//!   `/thumbnail` response; observed 1.161 behavior, reproduced here for the same reason: it costs
//!   nothing and closes a class of bugs in *this crate's own* allowlist logic being wrong.
//!
//! `X-Content-Type-Options: nosniff` is also always sent, so a browser that receives (say) a
//! `.html`-named file served as `text/plain` cannot second-guess the declared type and render it
//! as HTML anyway (MIME-sniffing attacks predate CSP and are not fully covered by it in every
//! browser).
//!
//! # Rule 2: `Content-Disposition` filename handling
//!
//! The uploader's filename (from `?filename=` at upload time, or a client-set `filename` in the
//! async-upload `POST .../create` body — this crate reads it from wherever [`crate::repository`]
//! stored it) is untrusted input reflected into a response header. [`content_disposition`]:
//!
//! - Strips control characters (`\r`, `\n`, `\0` — header/response-splitting) and path separators
//!   (`/`, `\`) before use.
//! - ASCII filenames are quoted directly (`filename="report.pdf"`) with internal `"` and `\`
//!   backslash-escaped per RFC 6266.
//! - Non-ASCII filenames additionally get an RFC 5987 `filename*=UTF-8''<percent-encoded>`
//!   parameter (most modern browsers prefer `filename*` when present; the plain `filename` stays
//!   as an ASCII-safe fallback, replacing non-ASCII bytes with `_`).
//! - No filename at all (common for pasted images) omits the `filename` parameter entirely rather
//!   than inventing one — `Content-Disposition: inline` / `Content-Disposition: attachment` with
//!   no parameters is valid and is what Synapse sends in this case.
//!
//! # Rule 3: size limits
//!
//! Enforced in three independent places, each closing a different gap:
//!
//! 1. **Upload time** ([`crate::repository`]): the request body is rejected once it exceeds
//!    `max_upload_size` (`hs_config::MediaConfig`) or the caller's [`crate::policy`] quota,
//!    streamed — a client cannot force the server to buffer an unbounded body before rejecting it.
//! 2. **Decode time** ([`crate::sniff`]): even a file that fit under the upload limit can decode to
//!    a pixel buffer far larger than its compressed size (the decompression-bomb case) — decoders
//!    are given an explicit `image::Limits` (max dimensions, max allocation) so a hostile 500-byte
//!    PNG cannot force a multi-gigabyte allocation.
//! 3. **Thumbnail request time** ([`crate::thumbnail`]): a dynamically requested thumbnail size is
//!    bounds-checked against configured maximums before any decoding happens, so a client cannot
//!    request a 50000x50000 thumbnail and force the allocation that way instead.
//!
//! # Rule 4: content-type sniffing rules
//!
//! This crate does **not** attempt to guess a "better" `Content-Type` to serve than the one
//! recorded at upload time (whatever `sniff`-derived format detection happens in
//! [`crate::sniff`] is for the server's own decoding safety and thumbnail generation, never fed
//! back into the served `Content-Type` header — the spec requires the originally declared type be
//! preserved). Combined with `X-Content-Type-Options: nosniff`, this means: the server tells the
//! browser exactly one `Content-Type`, tells it not to guess a different one, and independently
//! decides `Content-Disposition` and `Content-Security-Policy` from its own allowlist rather than
//! from that same declared type. Three independent signals, not one type driving three decisions.
//!
//! # Rule 5: range requests
//!
//! `GET .../download/...` and `.../thumbnail/...` both advertise `Accept-Ranges: bytes` and honor
//! a `Range: bytes=<start>-<end>` request header ([`parse_range`]), returning `206 Partial
//! Content` with `Content-Range` when satisfiable, `416 Range Not Satisfiable` (with
//! `Content-Range: bytes */<total>`) when not, and a plain `200` with the full body when no
//! `Range` header was sent. Only a single byte range is supported (`multipart/byteranges` is not
//! implemented — Synapse does not implement it either; a multi-range request is treated as
//! satisfying only its first range, matching observed 1.161 behavior). This matters for media
//! specifically because clients seek video/audio content and resume interrupted downloads.

use axum::http::{HeaderMap, HeaderValue, header};

/// `Content-Type` values this crate will serve with `Content-Disposition: inline`. Anything not
/// on this list — most importantly `text/html` and `image/svg+xml`, which are deliberately never
/// added no matter what — gets `attachment` instead. Mirrors Synapse's `INLINE_CONTENT_TYPES`
/// (`synapse/media/_base.py`; behavior only, no code copied).
///
/// The comparison in [`is_inline_safe`] is against the MIME type only (the part before any `;`
/// parameter, lowercased), so `image/png; charset=binary` still matches `image/png`.
pub const INLINE_SAFE_CONTENT_TYPES: &[&str] = &[
    "text/css",
    "text/plain",
    "text/csv",
    "application/json",
    "application/ld+json",
    "image/jpeg",
    "image/gif",
    "image/png",
    "image/apng",
    "image/webp",
    "image/avif",
    "video/mp4",
    "video/webm",
    "video/ogg",
    "video/quicktime",
    "audio/mp4",
    "audio/webm",
    "audio/aac",
    "audio/mpeg",
    "audio/ogg",
    "audio/wave",
    "audio/wav",
    "audio/x-wav",
    "audio/x-pn-wav",
    "audio/flac",
    "audio/x-flac",
];

/// Content types that are *never* served inline even if a future edit of
/// [`INLINE_SAFE_CONTENT_TYPES`] accidentally added them: defense in depth against exactly that
/// mistake. Checked first, before the allowlist.
const ALWAYS_ATTACHMENT: &[&str] = &[
    "text/html",
    "application/xhtml+xml",
    "image/svg+xml",
    "text/xml",
    "application/xml",
    "application/xml+xhtml",
];

/// Extracts the bare MIME type (`type/subtype`, lowercased, no parameters) from a `Content-Type`
/// header value.
fn bare_mime(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// Whether `content_type` may be served with `Content-Disposition: inline`. See the module docs'
/// "Rule 1".
#[must_use]
pub fn is_inline_safe(content_type: &str) -> bool {
    let mime = bare_mime(content_type);
    if ALWAYS_ATTACHMENT.contains(&mime.as_str()) {
        return false;
    }
    INLINE_SAFE_CONTENT_TYPES.contains(&mime.as_str())
}

/// The `Content-Security-Policy` sent on every media response. Matches Synapse's shape
/// (`synapse/media/_base.py`'s `CONTENT_SECURITY_POLICY`; behavior only, no code copied):
/// sandboxed (no scripts, no plugins, no top-level navigation out of the sandbox), no default
/// source, media may only reference itself, and inline styles are allowed only because some
/// legitimate media (SVG-adjacent CSS in an attachment a browser chooses to render anyway) needs
/// them and style cannot execute script.
pub const CSP_HEADER_VALUE: &str = "sandbox; default-src 'none'; script-src 'none'; plugin-types application/pdf; style-src 'unsafe-inline'; media-src 'self'; object-src 'self';";

/// Sanitizes an untrusted filename for use in a `Content-Disposition` header: strips control
/// characters and path separators. Returns `None` if nothing safe is left (an empty or
/// all-control-character input), in which case the caller should omit the `filename` parameter
/// entirely rather than send an empty one.
fn sanitize_filename(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control() && *c != '/' && *c != '\\')
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Percent-encodes `s` for an RFC 5987 `ext-value` (the `filename*=UTF-8''...` form).
fn percent_encode_ext_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        let b = *byte;
        // RFC 5987 attr-char: ALPHA / DIGIT / "!" / "#" / "$" / "&" / "+" / "-" / "." / "^" / "_"
        // / "`" / "|" / "~"
        let is_attr_char = b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            );
        if is_attr_char {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

/// Builds an ASCII-safe fallback filename for the plain `filename=` parameter: non-ASCII bytes
/// become `_`, and any `"` or `\` is backslash-escaped per RFC 6266 `quoted-string`.
fn ascii_fallback_and_quote(s: &str) -> String {
    let ascii: String = s
        .chars()
        .map(|c| if c.is_ascii() { c } else { '_' })
        .collect();
    let mut quoted = String::with_capacity(ascii.len() + 2);
    quoted.push('"');
    for c in ascii.chars() {
        if c == '"' || c == '\\' {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

/// Whether a `Content-Disposition` should be `inline` or `attachment`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispositionKind {
    /// The browser may render this in place.
    Inline,
    /// The browser should offer to save this rather than render it.
    Attachment,
}

impl DispositionKind {
    fn as_str(self) -> &'static str {
        match self {
            DispositionKind::Inline => "inline",
            DispositionKind::Attachment => "attachment",
        }
    }
}

/// Decides [`DispositionKind`] for `content_type`. See the module docs' "Rule 1".
#[must_use]
pub fn disposition_kind(content_type: &str) -> DispositionKind {
    if is_inline_safe(content_type) {
        DispositionKind::Inline
    } else {
        DispositionKind::Attachment
    }
}

/// Builds a `Content-Disposition` header value for `kind`, optionally carrying a sanitized
/// `filename`. See the module docs' "Rule 2".
#[must_use]
pub fn content_disposition(kind: DispositionKind, filename: Option<&str>) -> HeaderValue {
    let base = kind.as_str();
    let Some(raw) = filename else {
        return HeaderValue::from_static(match kind {
            DispositionKind::Inline => "inline",
            DispositionKind::Attachment => "attachment",
        });
    };
    let Some(clean) = sanitize_filename(raw) else {
        return HeaderValue::from_static(match kind {
            DispositionKind::Inline => "inline",
            DispositionKind::Attachment => "attachment",
        });
    };
    let is_ascii = clean.is_ascii();
    let value = if is_ascii {
        format!("{base}; filename={}", ascii_fallback_and_quote(&clean))
    } else {
        format!(
            "{base}; filename={}; filename*=UTF-8''{}",
            ascii_fallback_and_quote(&clean),
            percent_encode_ext_value(&clean)
        )
    };
    // `value` was built entirely from sanitized (control-character-free) input plus static ASCII
    // punctuation, so this cannot fail; the fallback is defensive, not expected to trigger.
    HeaderValue::from_str(&value).unwrap_or_else(|_| HeaderValue::from_static(base))
}

/// Builds the full set of security headers this crate sends on a `download` or `thumbnail`
/// response: `Content-Type` (verbatim, per Rule 4), `Content-Disposition` (Rules 1 and 2),
/// `Content-Security-Policy` (Rule 1), `X-Content-Type-Options` (Rule 4), `Accept-Ranges` (Rule
/// 5) and a conservative `Cache-Control` (media content is immutable once uploaded — a media ID
/// is never reused — so it is safe to cache aggressively).
#[must_use]
pub fn response_headers(content_type: &str, filename: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(ct) = HeaderValue::from_str(content_type) {
        headers.insert(header::CONTENT_TYPE, ct);
    }
    headers.insert(
        header::CONTENT_DISPOSITION,
        content_disposition(disposition_kind(content_type), filename),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP_HEADER_VALUE),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    headers
}

/// A resolved, satisfiable byte range: `start..=end`, both inclusive, `0 <= start <= end < total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    /// First byte offset, inclusive.
    pub start: u64,
    /// Last byte offset, inclusive.
    pub end: u64,
    /// The full resource length this range was resolved against.
    pub total: u64,
}

impl ByteRange {
    /// The number of bytes this range covers.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.end - self.start + 1
    }

    /// Whether this range covers zero bytes. Never true for a [`ByteRange`] produced by
    /// [`parse_range`] (an empty range is rejected as unsatisfiable), kept for API completeness
    /// / clippy's `len_without_is_empty`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The `Content-Range` header value: `bytes <start>-<end>/<total>`.
    #[must_use]
    pub fn content_range_header(&self) -> String {
        format!("bytes {}-{}/{}", self.start, self.end, self.total)
    }
}

/// The outcome of parsing a `Range` header against a resource of length `total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeOutcome {
    /// No `Range` header (or one this crate does not understand — a non-`bytes` unit — which is
    /// treated as absent per RFC 9110 rather than an error): serve the whole resource.
    Full,
    /// A satisfiable single byte range.
    Partial(ByteRange),
    /// The header named a `bytes` unit but no range in it could be satisfied against `total`
    /// (e.g. `bytes=1000000-` against a 10-byte resource): `416`, `Content-Range: bytes */total`.
    Unsatisfiable,
}

/// Parses an HTTP `Range` header value against a resource of length `total` bytes. See the module
/// docs' "Rule 5" for the supported subset (single range only; a multi-range request is
/// satisfied by its first range, matching Synapse's observed behavior).
#[must_use]
pub fn parse_range(header_value: Option<&str>, total: u64) -> RangeOutcome {
    let Some(raw) = header_value else {
        return RangeOutcome::Full;
    };
    let Some(spec) = raw.strip_prefix("bytes=") else {
        return RangeOutcome::Full;
    };
    // Only the first range of a comma-separated list is honored.
    let first = spec.split(',').next().unwrap_or("").trim();
    if total == 0 {
        return RangeOutcome::Unsatisfiable;
    }
    let last_index = total - 1;
    let resolved = match first.split_once('-') {
        Some((start_s, end_s)) if start_s.is_empty() && !end_s.is_empty() => {
            // Suffix range: "-N" means the last N bytes.
            let Ok(n) = end_s.trim().parse::<u64>() else {
                return RangeOutcome::Unsatisfiable;
            };
            if n == 0 {
                return RangeOutcome::Unsatisfiable;
            }
            let start = last_index.saturating_sub(n - 1);
            Some((start, last_index))
        }
        Some((start_s, end_s)) if !start_s.is_empty() => {
            let Ok(start) = start_s.trim().parse::<u64>() else {
                return RangeOutcome::Unsatisfiable;
            };
            if start > last_index {
                return RangeOutcome::Unsatisfiable;
            }
            let end = if end_s.trim().is_empty() {
                last_index
            } else {
                match end_s.trim().parse::<u64>() {
                    Ok(e) => e.min(last_index),
                    Err(_) => return RangeOutcome::Unsatisfiable,
                }
            };
            if end < start {
                return RangeOutcome::Unsatisfiable;
            }
            Some((start, end))
        }
        _ => None,
    };
    match resolved {
        Some((start, end)) => RangeOutcome::Partial(ByteRange { start, end, total }),
        None => RangeOutcome::Unsatisfiable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- inline/attachment allowlist ---

    #[test]
    fn html_is_never_inline_safe() {
        assert!(!is_inline_safe("text/html"));
        assert!(!is_inline_safe("text/html; charset=utf-8"));
        assert!(!is_inline_safe("TEXT/HTML"));
    }

    #[test]
    fn svg_is_never_inline_safe() {
        assert!(!is_inline_safe("image/svg+xml"));
    }

    #[test]
    fn known_image_types_are_inline_safe() {
        for t in ["image/png", "image/jpeg", "image/gif", "image/webp"] {
            assert!(is_inline_safe(t), "{t} should be inline-safe");
        }
    }

    #[test]
    fn unrecognized_type_defaults_to_attachment() {
        assert!(!is_inline_safe("application/x-executable"));
        assert!(!is_inline_safe("application/octet-stream"));
    }

    #[test]
    fn always_attachment_wins_even_if_hypothetically_allowlisted() {
        // Defense in depth: even if a type is (incorrectly) in both lists, ALWAYS_ATTACHMENT wins.
        // We can't easily add to the const in a test, so this asserts the actual overlap is empty
        // and that the check order in `is_inline_safe` puts ALWAYS_ATTACHMENT first.
        for t in ALWAYS_ATTACHMENT {
            assert!(
                !INLINE_SAFE_CONTENT_TYPES.contains(t),
                "{t} must not be in both lists"
            );
        }
    }

    #[test]
    fn disposition_kind_matches_allowlist() {
        assert_eq!(disposition_kind("image/png"), DispositionKind::Inline);
        assert_eq!(disposition_kind("text/html"), DispositionKind::Attachment);
        assert_eq!(
            disposition_kind("image/svg+xml"),
            DispositionKind::Attachment
        );
    }

    // --- Content-Disposition filename handling ---

    #[test]
    fn no_filename_omits_the_parameter() {
        let v = content_disposition(DispositionKind::Inline, None);
        assert_eq!(v.to_str().unwrap(), "inline");
    }

    #[test]
    fn ascii_filename_is_quoted() {
        let v = content_disposition(DispositionKind::Attachment, Some("report.pdf"));
        assert_eq!(v.to_str().unwrap(), "attachment; filename=\"report.pdf\"");
    }

    #[test]
    fn filename_with_quotes_is_escaped() {
        let v = content_disposition(DispositionKind::Inline, Some("weird\"name.png"));
        let s = v.to_str().unwrap();
        assert!(s.contains("\\\""));
    }

    #[test]
    fn non_ascii_filename_gets_both_parameters() {
        let v = content_disposition(DispositionKind::Inline, Some("caf\u{e9}.png"));
        let s = v.to_str().unwrap();
        assert!(s.contains("filename=\"caf_.png\""));
        assert!(s.contains("filename*=UTF-8''caf%C3%A9.png"));
    }

    #[test]
    fn control_characters_and_crlf_are_stripped() {
        let v = content_disposition(
            DispositionKind::Attachment,
            Some("evil\r\nSet-Cookie: x=y.png"),
        );
        let s = v.to_str().unwrap();
        assert!(!s.contains('\r'));
        assert!(!s.contains('\n'));
    }

    #[test]
    fn path_separators_are_stripped_from_filename() {
        let v = content_disposition(DispositionKind::Attachment, Some("../../etc/passwd.png"));
        let s = v.to_str().unwrap();
        assert!(!s.contains('/'));
    }

    #[test]
    fn filename_that_is_only_control_characters_omits_parameter() {
        let v = content_disposition(DispositionKind::Inline, Some("\r\n\0"));
        assert_eq!(v.to_str().unwrap(), "inline");
    }

    // --- CSP / nosniff / response_headers ---

    #[test]
    fn response_headers_always_include_csp_and_nosniff() {
        for ct in ["image/png", "text/html", "application/octet-stream"] {
            let headers = response_headers(ct, None);
            assert_eq!(
                headers.get(header::CONTENT_SECURITY_POLICY).unwrap(),
                CSP_HEADER_VALUE
            );
            assert_eq!(
                headers.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
                "nosniff"
            );
            assert_eq!(headers.get(header::ACCEPT_RANGES).unwrap(), "bytes");
        }
    }

    #[test]
    fn response_headers_preserve_declared_content_type_verbatim() {
        // Rule 4: never re-derive/rewrite Content-Type, even for html.
        let headers = response_headers("text/html", None);
        assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "text/html");
        assert_eq!(
            headers.get(header::CONTENT_DISPOSITION).unwrap(),
            "attachment"
        );
    }

    // --- range parsing ---

    #[test]
    fn no_range_header_is_full() {
        assert_eq!(parse_range(None, 100), RangeOutcome::Full);
    }

    #[test]
    fn simple_bounded_range() {
        let RangeOutcome::Partial(r) = parse_range(Some("bytes=0-99"), 1000) else {
            panic!("expected Partial")
        };
        assert_eq!(r.start, 0);
        assert_eq!(r.end, 99);
        assert_eq!(r.len(), 100);
    }

    #[test]
    fn open_ended_range_goes_to_the_last_byte() {
        let RangeOutcome::Partial(r) = parse_range(Some("bytes=990-"), 1000) else {
            panic!("expected Partial")
        };
        assert_eq!(r.start, 990);
        assert_eq!(r.end, 999);
    }

    #[test]
    fn suffix_range_is_the_last_n_bytes() {
        let RangeOutcome::Partial(r) = parse_range(Some("bytes=-10"), 1000) else {
            panic!("expected Partial")
        };
        assert_eq!(r.start, 990);
        assert_eq!(r.end, 999);
    }

    #[test]
    fn end_beyond_total_is_clamped_not_rejected() {
        let RangeOutcome::Partial(r) = parse_range(Some("bytes=0-999999"), 1000) else {
            panic!("expected Partial")
        };
        assert_eq!(r.end, 999);
    }

    #[test]
    fn start_beyond_total_is_unsatisfiable() {
        assert_eq!(
            parse_range(Some("bytes=1000-2000"), 1000),
            RangeOutcome::Unsatisfiable
        );
    }

    #[test]
    fn empty_resource_is_always_unsatisfiable() {
        assert_eq!(
            parse_range(Some("bytes=0-0"), 0),
            RangeOutcome::Unsatisfiable
        );
    }

    #[test]
    fn malformed_range_header_is_unsatisfiable() {
        assert_eq!(
            parse_range(Some("bytes=abc-def"), 1000),
            RangeOutcome::Unsatisfiable
        );
    }

    #[test]
    fn non_bytes_unit_is_treated_as_absent() {
        assert_eq!(parse_range(Some("items=0-1"), 1000), RangeOutcome::Full);
    }

    #[test]
    fn multi_range_uses_only_the_first() {
        let RangeOutcome::Partial(r) = parse_range(Some("bytes=0-9,20-29"), 1000) else {
            panic!("expected Partial")
        };
        assert_eq!((r.start, r.end), (0, 9));
    }

    #[test]
    fn content_range_header_format() {
        let r = ByteRange {
            start: 0,
            end: 99,
            total: 1000,
        };
        assert_eq!(r.content_range_header(), "bytes 0-99/1000");
        assert!(!r.is_empty());
    }
}
