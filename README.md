# blitz + gpui + subsecond experiment

**Verdict: works, but too slow for production use.** Archived for reference.

## What it does

Renders HTML/CSS inside a GPUI window by rasterizing a `blitz-dom` document
offscreen with `vello_cpu` and presenting the result as a `gpui::RenderImage`.

- `src/views/*.html` — serialized DOM dumps of the buwiz UI redesign prototype
  (https://github.com/hexuria/buwiz-ui-redesign), JS stripped, one per view.
- `src/main.rs` — the bridge: GPUI window + status bar, blitz `HtmlDocument`
  per view, GPUI mouse/wheel events → `blitz_traits::events::UiEvent` →
  `EventDriver`, click chains → `data-act` attribute dispatch (a Rust port of
  the prototype's `act()` delegation), theme via `data-theme` attribute.
- Subsecond/Dioxus devtools: `dx serve --hot-patch` connects and patches the
  hot functions (`render_body`, pointer handlers, `view_html`). Patches to
  `src/views/*.html` rebuild the DOM live; GPUI-side state (current view,
  nav history, theme) survives.

## Why it's slow

Every repaint = full restyle+layout + **full-scene CPU raster** + 17MB
RGBA→BGRA byte swap + full texture upload. In release mode a full paint of the
1280×824@2x shell measured **~26ms** — survivable for clicks, but scroll on a
dense form page is still janky because every wheel tick re-rasters 4.2Mpx.

The dependency `opt-level=3` overrides (`Cargo.toml [profile.dev.package]` —
the gpui-kit docs trick) help the debug build but can't fix the architecture.

## What would actually be fast

- `blitz-shell` (winit + vello wgpu) — GPU raster, damage regions, no bridge.
- Or keep GPUI but share a GPU texture (vello wgpu → IOSurface/MTL → GPUI).
- Or dirty-rect rastering + partial texture upload on this bridge.

## Interesting bugs found along the way

- `Viewport::new(w, h, scale)` takes **physical** pixels — CSS viewport is
  `size/scale`. Passing CSS px silently halves your layout size.
- Inline SVGs get serialized to usvg with **markup attributes only** — class-
  based `stroke`/`fill` never reaches the tree (icons render as black
  squares). Fix: `fill="none" stroke="currentColor"` attributes; blitz
  resolves `currentColor` against the element's computed `color`.
- `dx` ThinLink caches `libdeps-*.a` — a killed link leaves a stale archive
  missing sysroot rlibs (delete the archive to regenerate).
- `io-surface` needs its `link` feature for `-framework IOSurface` under dx's
  force-loaded bundle links.

## Layout

    src/main.rs        bridge + act dispatch + subsecond wiring
    src/views/         per-view DOM dumps (blitz input)
    vendor/serde_fmt   patch removing From<Error> for fmt::Error (was needed
                       on gpui-pre 0.3.4's log/kv_unstable_serde; unused on 0.3.9)
    Cargo.toml         [profile.dev.package] O3 deps + io-surface link fix
