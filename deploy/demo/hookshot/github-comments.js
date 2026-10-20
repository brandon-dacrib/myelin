// Moved on 2026-10-20: the demo's GitHub comments transformation function is the server's
// `github-comments` webhook source, crates/hs-bridges/hookshot-sources/github-comments/transform.js
// (its README there says what to set on GitHub). A webhook made through the admin API
// (`POST /api/v1/bridge-offerings/matrix-hookshot/connections` with `source: "github-comments"`)
// or the offering's page carries that file as its transformation function; the demo's two
// rooms were made by hand with the same text.
