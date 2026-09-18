#!/usr/bin/env node
// Checks the status token pairs in src/styles/tokens.css meet WCAG AA
// contrast (4.5:1 for the small text these tokens are mostly used for) in
// both themes. This does not replace axe (which checks the rendered page,
// class by class); it is a fast, offline way to catch a bad hex value
// before it reaches a component. Run after editing any status/* token.
//
// Found real failures this way once already: see the git history / status
// file entry for the axe color-contrast fix on badges and the primary button.

const pairs = [
  ["success (light)", "#166534", "#f0fdf4"],
  ["success (dark)", "#4ade80", "#052e16"],
  ["warning (light)", "#92400e", "#fffbeb"],
  ["warning (dark)", "#fbbf24", "#451a03"],
  ["danger (light)", "#b91c1c", "#fef2f2"],
  ["danger (dark)", "#f87171", "#450a0a"],
  ["info (light)", "#1d4ed8", "#eff6ff"],
  ["info (dark)", "#60a5fa", "#172554"],
  ["muted-status (light)", "#475569", "#f1f5f9"],
  ["muted-status (dark)", "#94a3b8", "#1e293b"],
  ["text-faint (light) on surface-sunken", "#586474", "#f1f5f9"],
  ["text-faint (light) on surface", "#586474", "#ffffff"],
  ["text-faint (dark) on surface", "#8492a6", "#0f172a"],
  ["text-faint (dark) on canvas", "#8492a6", "#020617"],
  ["danger button text-on-fill (light)", "#ffffff", "#b91c1c"],
  ["danger button text-on-fill (dark)", "#450a0a", "#f87171"],
  ["accent button text-on-fill (light)", "#ffffff", "#4f46e5"],
  ["accent button text-on-fill (dark)", "#0f172a", "#818cf8"],
];

function hexToRgb(hex) {
  const clean = hex.replace("#", "");
  const full =
    clean.length === 3
      ? clean
          .split("")
          .map((c) => c + c)
          .join("")
      : clean;
  const num = parseInt(full, 16);
  return [(num >> 16) & 255, (num >> 8) & 255, num & 255];
}

function luminance([r, g, b]) {
  const a = [r, g, b].map((v) => {
    const c = v / 255;
    return c <= 0.03928 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4);
  });
  return 0.2126 * a[0] + 0.7152 * a[1] + 0.0722 * a[2];
}

function contrast(hex1, hex2) {
  const l1 = luminance(hexToRgb(hex1));
  const l2 = luminance(hexToRgb(hex2));
  const [lighter, darker] = l1 > l2 ? [l1, l2] : [l2, l1];
  return (lighter + 0.05) / (darker + 0.05);
}

const THRESHOLD = 4.5;
let failed = false;
for (const [label, fg, bg] of pairs) {
  const ratio = contrast(fg, bg);
  const ok = ratio >= THRESHOLD;
  if (!ok) failed = true;
  console.log(`${ok ? "OK  " : "FAIL"} ${label.padEnd(38)} ${ratio.toFixed(2)}:1`);
}

if (failed) {
  console.error(`\nOne or more pairs are below ${THRESHOLD}:1.`);
  process.exit(1);
}
console.log(`\nAll pairs meet ${THRESHOLD}:1.`);
