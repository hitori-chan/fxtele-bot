# Vendored files

Built into the binary; no JavaScript build step. Checked against the npm registry's sha512
integrity before copying.

| File | Package | License | SHA-256 |
|---|---|---|---|
| `htmx.min.js` | htmx.org 2.0.11 (`dist/htmx.min.js`) | 0BSD | `d6fdc75f204e6bdefa99b69bf1e6d4ac69b8a364f77929f45c13476b4000f717` |

`app.js`, `app.css`, `iv.css` and `icons.svg` are ours; `favicon.png` and the other pictures are
the logo (see the README). The charts are SVG drawn by the server (`src/dashboard/chart.rs`), so
there's no chart library.
