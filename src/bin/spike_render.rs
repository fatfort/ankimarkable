// Spike B (Phase 1) — GO/NO-GO gate for full HTML/CSS card rendering with no webview.
//
// Drives the pure-Rust CPU pipeline (blitz-dom + stylo + taffy + parley + vello_cpu)
// directly so we can inject a FontContext with EMBEDDED fonts. A static-musl binary
// has no usable font discovery on the device, so the first render came out textless;
// embedding the Noto fonts + mapping the CSS generics fixes that deterministically.
//
// Usage on device:  ./spike_render /home/root/card.png
//   then scp the PNG back and eyeball fidelity; the program prints render time.

use std::time::Instant;

use anyrender::render_to_buffer;
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::DocumentConfig;
use blitz_html::HtmlDocument;
use blitz_paint::paint_scene;
use blitz_traits::shell::{ColorScheme, Viewport};
use parley::fontique::{Blob, GenericFamily};
use parley::FontContext;

// Embedded device fonts (pulled from /usr/share/fonts/ttf/noto). Embedding makes
// rendering identical regardless of what the device exposes to a static binary.
const F_SANS_REG: &[u8] = include_bytes!("../../assets/fonts/NotoSans-Regular.ttf");
const F_SANS_BOLD: &[u8] = include_bytes!("../../assets/fonts/NotoSans-Bold.ttf");
const F_SANS_IT: &[u8] = include_bytes!("../../assets/fonts/NotoSans-Italic.ttf");
const F_SANS_BIT: &[u8] = include_bytes!("../../assets/fonts/NotoSans-BoldItalic.ttf");
const F_MONO: &[u8] = include_bytes!("../../assets/fonts/NotoMono-Regular.ttf");

const CARD_HTML: &str = r#"<!DOCTYPE html>
<html><head><style>
  body { font-family: sans-serif; background: #ffffff; color: #111;
         margin: 0; padding: 48px; }
  .card { font-size: 40px; text-align: center; line-height: 1.4; }
  .question { font-weight: 700; }
  hr#answer { border: none; border-top: 2px solid #888; margin: 40px 0; }
  .answer { font-size: 34px; color: #222; }
  .tag { display: inline-block; font-size: 22px; color: #fff;
         background: #555; border-radius: 8px; padding: 4px 14px; margin-top: 30px; }
  code { font-family: monospace; background: #eee; padding: 2px 6px; border-radius: 4px; }
</style></head>
<body><div class="card">
  <div class="question">What does <code>Box&lt;T&gt;</code> allocate?</div>
  <hr id="answer">
  <div class="answer">A single value of type T on the <b>heap</b>,
    owned via a unique pointer. <i>(unique ownership)</i></div>
  <div class="tag">rust::ownership</div>
</div></body></html>"#;

/// Build a FontContext with our embedded fonts registered and the CSS generic
/// families (sans-serif / serif / monospace) mapped onto them, so any deck's
/// font-family resolves to a real glyph set.
fn build_font_ctx() -> FontContext {
    let mut fcx = FontContext::new();

    let mut sans_ids = Vec::new();
    for bytes in [F_SANS_REG, F_SANS_BOLD, F_SANS_IT, F_SANS_BIT] {
        for (fam, _) in fcx
            .collection
            .register_fonts(Blob::from(bytes.to_vec()), None)
        {
            sans_ids.push(fam);
        }
    }
    let mut mono_ids = Vec::new();
    for (fam, _) in fcx
        .collection
        .register_fonts(Blob::from(F_MONO.to_vec()), None)
    {
        mono_ids.push(fam);
    }

    fcx.collection
        .set_generic_families(GenericFamily::SansSerif, sans_ids.iter().copied());
    fcx.collection
        .set_generic_families(GenericFamily::Serif, sans_ids.iter().copied());
    fcx.collection
        .set_generic_families(GenericFamily::Monospace, mono_ids.iter().copied());

    fcx
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/home/root/card.png".to_string());
    let (w, h) = (1500u32, 1200u32);
    let scale = 1.0f32;

    let viewport = Viewport::new(w, h, scale, ColorScheme::Light);
    let doc_config = DocumentConfig {
        viewport: Some(viewport),
        font_ctx: Some(build_font_ctx()),
        ..Default::default()
    };

    let t0 = Instant::now();
    let mut document = HtmlDocument::from_html(CARD_HTML, doc_config);
    document.resolve(0.0);

    let buffer = render_to_buffer::<VelloCpuImageRenderer, _>(
        |scene| paint_scene(scene, document.as_ref(), scale as f64, w, h),
        w,
        h,
    );
    let dt = t0.elapsed();

    let mut png_bytes = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut png_bytes, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut wr = enc.write_header()?;
        wr.write_image_data(&buffer)?;
    }
    std::fs::write(&out, &png_bytes)?;

    println!("rendered card -> {out} ({} bytes)", png_bytes.len());
    println!("render time: {dt:?}");
    println!("SPIKE B OK");
    Ok(())
}
