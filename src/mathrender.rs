//! Math rendering — turns the two kinds of math in Anki cards into raster PNG
//! `<img>`s that blitz (no webview, no JS, no SVG) can display:
//!
//! 1. `[latex]` / `[$]` / `[$$]` — Anki pre-renders these to `latex-<sha1>.svg`
//!    in the media folder (dvisvgm path-outline SVG). `anki::latex::extract_latex`
//!    munges them to `<img src="latex-<sha1>.svg">` and gives the exact filenames;
//!    we rasterize each SVG with `resvg`.
//! 2. MathJax `\(...\)` / `\[...\]` — NOT pre-rendered (normally live MathJax JS).
//!    We render each with ReX (s3bk fork) → pathfinder `Scene` → self-contained
//!    `<path>` SVG → `resvg`.
//!
//! Both paths end at a `data:image/png;base64,…` URI substituted into the HTML.
//! Results are cached (by filename / by TeX source) so re-reviews are free.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;

use font::OpenTypeFont;
use pathfinder_export::{Export, FileFormat};
use pathfinder_geometry::rect::RectF;
use pathfinder_geometry::vector::vec2f;
use pathfinder_renderer::scene::Scene;
use rex::font::FontContext;
use rex::layout::{engine, Layout, LayoutSettings, Style};
use rex::parser::parse;
use rex::render::{Renderer, SceneWrapper};

// XITS Math (MATH-table OTF, CFF outlines) — embedded so no font discovery is
// needed on the musl device.
const MATH_FONT: &[u8] = include_bytes!("../assets/fonts/rex-xits.otf");

// Render scale: math is laid out at 48pt; ×3.5 keeps it crisp on e-ink.
const RASTER_SCALE: f32 = 3.5;

pub struct MathRenderer {
    ctx: FontContext<'static>,
    media_dir: PathBuf,
    // key -> Some(data-uri) rendered, or None if it failed (cached either way).
    cache: RefCell<HashMap<String, Option<String>>>,
    re_math: regex::Regex,
    enabled: bool,
}

impl MathRenderer {
    pub fn new(media_dir: PathBuf) -> Self {
        // Leak the parsed font so the FontContext can borrow it for `'static` — the
        // font is needed for the whole process lifetime, so one 683 KB leak is fine.
        let otf: Option<&'static OpenTypeFont> = font::parse(MATH_FONT)
            .ok()
            .and_then(|p| p.downcast_box::<OpenTypeFont>().ok())
            .map(|b| &*Box::leak(b));
        let enabled = otf.as_ref().map_or(false, |f| f.math.is_some());
        // Fall back to an (unused) context if the font somehow failed to parse; we
        // gate all rendering on `enabled` so it is never actually used then.
        let ctx = FontContext::new(otf.unwrap_or_else(|| {
            // Parse again and leak; if THIS fails the include_bytes is broken.
            let p = font::parse(MATH_FONT).expect("rex-xits.otf parse");
            let b = p.downcast_box::<OpenTypeFont>().ok().expect("rex-xits.otf not OpenType");
            &*Box::leak(b)
        }));
        Self {
            ctx,
            media_dir,
            cache: RefCell::new(HashMap::new()),
            // \(...\) inline OR \[...\] display
            re_math: regex::Regex::new(r"(?s)\\\((.+?)\\\)|\\\[(.+?)\\\]").unwrap(),
            enabled,
        }
    }

    /// Replace both math forms in a card's HTML with `<img>` PNG data-URIs.
    pub fn preprocess(&self, html: &str) -> String {
        // 1) Anki [latex]/[$]/[$$] -> <img ... src="latex-<sha1>.svg"> + fnames.
        let (munged, extracts) = anki::latex::extract_latex(html, true);
        let mut out = munged.into_owned();
        for e in extracts {
            if let Some(uri) = self.svg_file_uri(&e.fname) {
                out = out.replace(
                    &format!("src=\"{}\"", e.fname),
                    &format!("src=\"{}\"", uri),
                );
            }
        }
        // 2) MathJax \(...\) / \[...\] -> ReX -> PNG data-URI.
        if !self.enabled {
            return out;
        }
        self.re_math
            .replace_all(&out, |c: &regex::Captures| {
                // group 1 = inline \(...\); group 2 = display \[...\].
                let (tex, inline) = match (c.get(1), c.get(2)) {
                    (Some(m), _) => (m.as_str(), true),
                    (_, Some(m)) => (m.as_str(), false),
                    _ => ("", true),
                };
                match self.math_uri(tex) {
                    // The PNG is rendered at high res for crispness, so its display
                    // size MUST be set in CSS or it shows at native (huge) pixels.
                    // Height in `em` tracks the surrounding text; width stays auto to
                    // preserve aspect. Inline sits on the baseline; display centres.
                    Some(uri) if inline => format!(
                        "<img class=\"am-math\" style=\"height:1.15em;\
                         vertical-align:-0.35em;\" src=\"{uri}\">"
                    ),
                    Some(uri) => format!(
                        "<img class=\"am-math\" style=\"display:block;\
                         margin:16px auto;height:2.0em;\" src=\"{uri}\">"
                    ),
                    // Unrenderable (e.g. \begin{aligned}) — leave the raw source,
                    // escaped, so it's at least visible (no worse than before).
                    None => esc_text(c.get(0).unwrap().as_str()),
                }
            })
            .into_owned()
    }

    fn svg_file_uri(&self, fname: &str) -> Option<String> {
        let key = format!("f:{fname}");
        if let Some(v) = self.cache.borrow().get(&key) {
            return v.clone();
        }
        let uri = std::fs::read(self.media_dir.join(fname))
            .ok()
            .and_then(|svg| rasterize_svg(&svg))
            .map(|png| png_data_uri(&png));
        self.cache.borrow_mut().insert(key, uri.clone());
        uri
    }

    fn math_uri(&self, tex: &str) -> Option<String> {
        let key = format!("m:{tex}");
        if let Some(v) = self.cache.borrow().get(&key) {
            return v.clone();
        }
        let uri = self.render_tex(tex).map(|png| png_data_uri(&png));
        self.cache.borrow_mut().insert(key, uri.clone());
        uri
    }

    fn render_tex(&self, tex: &str) -> Option<Vec<u8>> {
        let tex = alias_macros(tex);
        let parsed = parse(&tex).ok()?;
        let settings = LayoutSettings::new(&self.ctx, 48.0, Style::Display);
        let node = engine::layout(&parsed, settings).ok()?.as_node();
        let mut layout = Layout::new();
        layout.add_node(node);
        let renderer = Renderer::new();
        let (x0, y0, x1, y1) = renderer.size(&layout);
        // ReX under-reports ink extent for accents/large operators; pad all sides.
        let pad = 40.0_f64;
        let mut scene = Scene::new();
        scene.set_view_box(RectF::from_points(
            vec2f((x0 - pad) as f32, (y0 - pad) as f32),
            vec2f((x1 + pad) as f32, (y1 + pad) as f32),
        ));
        let mut backend = SceneWrapper::new(&mut scene);
        renderer.render(&layout, &mut backend);
        let mut svg = Vec::new();
        scene.export(&mut svg, FileFormat::SVG).ok()?;
        rasterize_svg(&svg)
    }
}

/// ReX's parser predates several AMS/MathJax aliases the cards use — map them to
/// forms ReX understands (or unicode) before parsing. Matches WHOLE command names
/// (`\[a-zA-Z]+`) so an alias like `\ge` can't corrupt `\geq` / `\gets`.
fn alias_macros(tex: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"\\[a-zA-Z]+").unwrap());
    re.replace_all(tex, |c: &regex::Captures| {
        match &c[0][1..] {
            "longrightarrow" | "rightarrow" => "\\to".to_string(),
            "longleftarrow" | "leftarrow" => "\\gets".to_string(),
            "implies" => "\\Rightarrow".to_string(),
            "ldots" | "cdots" | "dotsc" => "\\dots".to_string(),
            "gt" => ">".to_string(),
            "lt" => "<".to_string(),
            "ge" => "\\geq".to_string(),
            "le" => "\\leq".to_string(),
            "neq" => "\\ne".to_string(),
            other => format!("\\{other}"),
        }
    })
    .into_owned()
}

/// Rasterize a self-contained SVG (path outlines) to PNG bytes over an opaque white
/// background. No font database needed (glyphs are already outlines).
fn rasterize_svg(svg: &[u8]) -> Option<Vec<u8>> {
    let opt = usvg::Options::default();
    let tree = usvg::Tree::from_data(svg, &opt).ok()?;
    let sz = tree.size();
    let w = ((sz.width() * RASTER_SCALE).ceil() as u32).clamp(1, 4000);
    let h = ((sz.height() * RASTER_SCALE).ceil() as u32).clamp(1, 4000);
    let mut pixmap = tiny_skia::Pixmap::new(w, h)?;
    pixmap.fill(tiny_skia::Color::WHITE);
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(RASTER_SCALE, RASTER_SCALE),
        &mut pixmap.as_mut(),
    );
    // Crop to the actual ink so the CSS `em` height maps to the glyph, not the
    // generous padding we render with (else the math shows tiny).
    autocrop(&pixmap).encode_png().ok()
}

/// Crop a white-background pixmap to its non-white ink bbox (+ a small margin).
fn autocrop(pm: &tiny_skia::Pixmap) -> tiny_skia::Pixmap {
    let (w, h) = (pm.width() as usize, pm.height() as usize);
    let data = pm.data(); // premultiplied RGBA, white bg = 255,255,255
    let (mut minx, mut miny, mut maxx, mut maxy) = (w, h, 0usize, 0usize);
    let mut any = false;
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            if data[i] < 245 || data[i + 1] < 245 || data[i + 2] < 245 {
                any = true;
                minx = minx.min(x);
                maxx = maxx.max(x);
                miny = miny.min(y);
                maxy = maxy.max(y);
            }
        }
    }
    if !any {
        return pm.clone();
    }
    let m = 6usize;
    let x0 = minx.saturating_sub(m);
    let y0 = miny.saturating_sub(m);
    let x1 = (maxx + m).min(w - 1);
    let y1 = (maxy + m).min(h - 1);
    let (cw, ch) = (x1 - x0 + 1, y1 - y0 + 1);
    let mut out = match tiny_skia::Pixmap::new(cw as u32, ch as u32) {
        Some(p) => p,
        None => return pm.clone(),
    };
    out.fill(tiny_skia::Color::WHITE);
    let dst = out.data_mut();
    for y in 0..ch {
        let si = ((y0 + y) * w + x0) * 4;
        let di = y * cw * 4;
        dst[di..di + cw * 4].copy_from_slice(&data[si..si + cw * 4]);
    }
    out
}

fn png_data_uri(png: &[u8]) -> String {
    format!("data:image/png;base64,{}", base64_encode(png))
}

fn esc_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Standard-alphabet base64 (no external dep).
fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}
