/**
 * Where the `e2e-real/` specs write their screenshots.
 *
 * By default `test-results/real-screenshots/`: a run against a real server leaves the committed
 * record in `docs/design/screenshots/` alone, so running a spec to check something does not
 * rewrite tracked images. `HS_REAL_UPDATE_SCREENSHOTS=1` writes them to
 * `docs/design/screenshots/` instead, to update that record on purpose (the file names are the
 * same either way: `<spec>-<name>-real.png`).
 */
export const SHOTS =
  process.env.HS_REAL_UPDATE_SCREENSHOTS === "1"
    ? "../docs/design/screenshots"
    : "test-results/real-screenshots";
