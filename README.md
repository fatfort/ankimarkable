# ankimarkable

A native **Anki review client for the reMarkable Paper Pro** — Anki's real Rust
backend (`rslib`), genuine AnkiWeb sync, full HTML/CSS card rendering, LaTeX math,
and an AnkiDroid-style **pen whiteboard** so you write your answer before revealing
it. Ships as a standalone [AppLoad](https://github.com/asivery/rm-appload) app: no
xochitl injection, no boot-bank risk.

<p align="center">
  <img src="docs/review.png" width="46%" alt="Review screen: HMM card with inline math, deck counts with active-queue underline, grade chrome">
  &nbsp;
  <img src="docs/whiteboard.png" width="46%" alt="Pen whiteboard: handwritten F-statistic worked answer over the card, black-and-white mode">
</p>

*Left: a statistics card with inline LaTeX rendered on-device. Right: working the
answer by hand on the whiteboard (in black-and-white mode) before revealing.*

## Features

- **Real Anki, not a viewer** — the actual `rslib` backend: a true `.anki2`
  collection, the FSRS scheduler with per-grade interval previews, and genuine
  **AnkiWeb sync** (incremental + automatic full-download recovery, media included).
- **Pen whiteboard over every card** — low-latency A2-waveform inking with the
  marker, pressure-sensitive width, working **eraser end**, stroke undo, and palm
  rejection. Ink persists through Show Answer so you can compare, and clears on grade.
- **LaTeX / MathJax math** — `\(...\)`, `\[...\]` and `[latex]` blocks render
  on-device via a bundled MicroTeX helper: fractions, big operators, `\perp`,
  `\stackrel`, `\boldsymbol`, blackboard/calligraphic letters. Inline math is
  baseline-aligned with the text; display math renders at natural size. TikZ
  diagrams appear via Anki desktop's own pre-rendered SVGs.
- **Full card fidelity** — cards render with their own HTML/CSS templates (pure-Rust
  blitz/stylo stack, embedded fonts), including images from your media collection.
- **Home screen** — collapsible deck tree with due counts, review streak, and
  tap-to-review any deck.
- **Tap the counts to toggle colour / black-and-white** rendering.
- **Review chrome** — new/learning/review counts with an underline marking the
  current card's queue; Undo (strokes first, then the last grade); an overflow menu
  for **bury card / suspend note**; colour-e-ink-aware refresh (fast mono inking +
  debounced colour settle, no flashing).
- **Safe by construction** — an external AppLoad process talking to the QTFB
  framebuffer. It cannot crash-loop xochitl or flip your boot bank.

## Install (device: reMarkable Paper Pro)

1. Install [xovi](https://github.com/asivery/xovi) + [rm-appload](https://github.com/asivery/rm-appload) (free, one-time).
2. Copy the app folder to `/home/root/xovi/exthome/appload/anki/`
   (`ankimarkable`, `icon.png`, `external.manifest.json`).
3. Copy `mathpng` to `/home/root/.ankimarkable/bin/mathpng` and the `microtex-res/`
   folder to `/home/root/.ankimarkable/microtex-res/` (math support).
4. Put your AnkiWeb credentials in `/home/root/.ankimarkable/ankiweb.txt`
   (line 1: email, line 2: password), launch from the AppLoad menu, tap **Sync**.

## Build (cross-compile from a Mac/Linux host)

```sh
export PROTOC="$(command -v protoc)"          # protobuf compiler for anki's build
cargo zigbuild --target aarch64-unknown-linux-musl --release
```

The math helper (`tools/mathpng/`) builds against
[MicroTeX](https://github.com/NanoMichael/MicroTeX) with Qt6 in an aarch64 SDK
container — see `tools/mathpng/mathpng-build.sh`.

## Environment knobs

| Variable | Default | Meaning |
|---|---|---|
| `ANKIMARKABLE_COL` | `/home/root/.ankimarkable/collection.anki2` | collection path |
| `ANKIMARKABLE_DECK` | `uni` | initial deck scope |
| `AM_MATHPNG` | `/home/root/.ankimarkable/bin/mathpng` | math helper binary |
| `AM_MATHPNG_RES` | `/home/root/.ankimarkable/microtex-res` | MicroTeX res fonts |

## License

**AGPL-3.0** (see `LICENSE`) — required by the [Anki](https://github.com/ankitects/anki)
`rslib` backend this links. Vendored `vendor/rex` is MIT/Apache-2.0;
`tools/mathpng` builds against MIT-licensed MicroTeX; bundled fonts are OFL/MIT.

A ready-made binary bundle is sold at
[bazaar.abaj.ai](https://bazaar.abaj.ai/remarkable) — buying it funds development;
building from this source yourself is and will always be possible.
