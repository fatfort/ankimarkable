//! Math rendering — turns the two kinds of math in Anki cards into raster PNG
//! `<img>`s that blitz (no webview, no JS, no live SVG) can display:
//!
//! 1. `[latex]` / `[$]` / `[$$]` — Anki pre-renders these to `latex-<sha1>.svg`
//!    in the media folder (dvisvgm path-outline SVG). `anki::latex::extract_latex`
//!    munges them to `<img src="latex-<sha1>.svg">`; we rasterize each with resvg
//!    (path outlines → no font DB needed).
//! 2. MathJax `\(...\)` / `\[...\]` — rendered by the on-device **microtex**
//!    helper (`mathpng`, a Qt/offscreen clatexmath build with full AMS coverage:
//!    `\perp`, `\stackrel`, `\boldsymbol`, `\mathbf/\mathcal/\mathbb`, …). We batch
//!    every math span in a card into ONE `mathpng` subprocess call (init amortised,
//!    ~0.2s for a whole card), read back each PNG plus its (baseline, w, h), and
//!    substitute an `<img>` sized to match the card font: DISPLAY math at natural
//!    scale (so a tall `\sum`/`\prod`/fraction stays legible), INLINE math dropped
//!    onto the text baseline by its descent.
//!
//! Both paths end at a `data:image/png;base64,…` URI substituted into the HTML.
//! Results are cached (by SVG filename / by TeX-source hash) so re-reviews are free.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

// Point size microtex renders at. 2× the on-screen target so e-ink stays crisp;
// the image is then scaled down by DISPLAY_SCALE in CSS.
const RENDER_PT: f32 = 80.0;
// CSS px per rendered px. RENDER_PT × DISPLAY_SCALE sets the on-screen size — a
// touch larger than the surrounding text so math reads clearly; taller display
// math scales up in proportion (a fraction is ~2 lines, a `\sum` taller still).
const DISPLAY_SCALE: f32 = 0.50;
// SVG raster scale for the pre-rendered Anki `[latex]` path.
const RASTER_SCALE: f32 = 3.5;

#[derive(Clone)]
struct Rendered {
    uri: String,
    // Cropped (ink-tight) pixel dimensions.
    w: f32,
    h: f32,
}

pub struct MathRenderer {
    mathpng: PathBuf,
    res: String,
    cache_dir: PathBuf,
    media_dir: PathBuf,
    // tex-hash -> Some(Rendered) | None (unrenderable — leave raw source).
    math_cache: RefCell<HashMap<u64, Option<Rendered>>>,
    // latex-<sha1>.svg filename -> Some(data-uri) | None.
    svg_cache: RefCell<HashMap<String, Option<String>>>,
    re_math: regex::Regex,
    enabled: bool,
}

impl MathRenderer {
    pub fn new(media_dir: PathBuf) -> Self {
        let mathpng = PathBuf::from(env_or("AM_MATHPNG", "/home/root/.ankimarkable/bin/mathpng"));
        // Res-font resolution: env override, else the app's own bundled copy, else
        // the textBoxes install (dev convenience) — so a standalone install works
        // without any other product present.
        let res = std::env::var("AM_MATHPNG_RES").unwrap_or_else(|_| {
            let own = "/home/root/.ankimarkable/microtex-res";
            if std::path::Path::new(own).is_dir() {
                own.to_string()
            } else {
                "/home/root/xovi/exthome/textBoxes/microtex-res".to_string()
            }
        });
        let cache_dir = PathBuf::from(env_or("AM_MATH_CACHE", "/home/root/.ankimarkable/math-cache"));
        // Only enable the MathJax path if the helper is actually present (it's a
        // device binary; on a Mac spike it's absent → math spans stay raw).
        let enabled = std::fs::metadata(&mathpng).map(|m| m.is_file()).unwrap_or(false);
        if enabled {
            let _ = std::fs::create_dir_all(&cache_dir);
        }
        Self {
            mathpng,
            res,
            cache_dir,
            media_dir,
            math_cache: RefCell::new(HashMap::new()),
            svg_cache: RefCell::new(HashMap::new()),
            // \(...\) inline (group 1) OR \[...\] display (group 2).
            re_math: regex::Regex::new(r"(?s)\\\((.+?)\\\)|\\\[(.+?)\\\]").unwrap(),
            enabled,
        }
    }

    /// Replace both math forms in a card's HTML with `<img>` PNG data-URIs.
    pub fn preprocess(&self, html: &str) -> String {
        // 1) Anki [latex]/[$]/[$$] -> <img src="latex-<sha1>.svg"> -> resvg PNG.
        let (munged, extracts) = anki::latex::extract_latex(html, true);
        let mut out = munged.into_owned();
        for e in extracts {
            if let Some(uri) = self.svg_file_uri(&e.fname) {
                out = out.replace(&format!("src=\"{}\"", e.fname), &format!("src=\"{}\"", uri));
            }
        }
        // 2) MathJax \(...\) / \[...\] -> microtex.
        if !self.enabled {
            return out;
        }
        // Collect the unique, not-yet-cached math spans (keyed by tex + inline-ness,
        // since inline and display crop the PNG differently) and render them all in
        // ONE mathpng invocation (amortises the engine init across the card).
        let mut todo: Vec<(String, bool)> = Vec::new();
        {
            let cache = self.math_cache.borrow();
            let mut seen: HashSet<u64> = HashSet::new();
            for c in self.re_math.captures_iter(&out) {
                let inline = c.get(1).is_some();
                let tex = span_tex(&c);
                let key = tex_key(&tex, inline);
                if !cache.contains_key(&key) && seen.insert(key) {
                    todo.push((tex, inline));
                }
            }
        }
        self.render_batch(&todo);
        // Substitute every span from the now-populated cache.
        let cache = self.math_cache.borrow();
        self.re_math
            .replace_all(&out, |c: &regex::Captures| {
                let inline = c.get(1).is_some();
                let tex = span_tex(c);
                match cache.get(&tex_key(&tex, inline)).and_then(|o| o.as_ref()) {
                    Some(r) => img_tag(r, inline),
                    // Unrenderable (e.g. a \begin{tikzpicture}) — leave the raw
                    // source, escaped, so it's at least visible (no worse).
                    None => esc_text(c.get(0).unwrap().as_str()),
                }
            })
            .into_owned()
    }

    /// Render a batch of (tex, inline) spans via the microtex helper, populating
    /// the cache. Inline math is finalized to a UNIFORM baseline-to-bottom distance
    /// so every inline formula aligns the same way (parley bottom-pins the image to
    /// the text baseline and ignores vertical-align); display math keeps full ink.
    fn render_batch(&self, spans: &[(String, bool)]) {
        if spans.is_empty() {
            return;
        }
        // key -> inline?, so the stdout pass knows how to finalize each PNG.
        let inline_by_key: HashMap<u64, bool> =
            spans.iter().map(|(t, i)| (tex_key(t, *i), *i)).collect();
        let spawned = Command::new(&self.mathpng)
            .arg(&self.cache_dir)
            .env("QT_QPA_PLATFORM", "offscreen")
            .env("MATHPNG_RES", &self.res)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(_) => return self.mark_failed(spans),
        };
        // Feed one request per line: <hash>\t<pt>\t<tex>. mathpng echoes <hash>
        // back and names its output <hash>.png.
        if let Some(mut stdin) = child.stdin.take() {
            for (tex, inline) in spans {
                let key = tex_key(tex, *inline);
                let clean = tex.replace(['\t', '\n', '\r'], " ");
                // Inline math in TEXT style so fractions/operators stay compact and
                // fit the line (MathJax does the same for \(...\)); display math in
                // DISPLAY style (full-size) for \[...\].
                let style = if *inline { "\\textstyle" } else { "\\displaystyle" };
                let _ = writeln!(stdin, "{key:016x}\t{RENDER_PT}\t{style} {clean}");
            }
            // stdin dropped here -> EOF -> mathpng flushes + exits.
        }
        let output = match child.wait_with_output() {
            Ok(o) => o,
            Err(_) => return self.mark_failed(spans),
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut cache = self.math_cache.borrow_mut();
        for line in stdout.lines() {
            // <hash>\t<baseline|ERR>\t<w>\t<h>
            let mut f = line.split('\t');
            let (Some(hh), Some(f1)) = (f.next(), f.next()) else {
                continue;
            };
            let Ok(key) = u64::from_str_radix(hh, 16) else {
                continue;
            };
            if f1 == "ERR" {
                cache.insert(key, None);
                continue;
            }
            let baseline: f32 = f1.parse().unwrap_or(0.0);
            let inline = inline_by_key.get(&key).copied().unwrap_or(true);
            let png_path = self.cache_dir.join(format!("{hh}.png"));
            let done = std::fs::read(&png_path)
                .ok()
                .and_then(|p| finalize_math_png(&p, inline, baseline))
                .map(|(png, w, h)| Rendered {
                    uri: png_data_uri(&png),
                    w,
                    h,
                });
            cache.insert(key, done);
        }
        // Anything the helper never answered for -> failed (leave raw).
        for (tex, inline) in spans {
            cache.entry(tex_key(tex, *inline)).or_insert(None);
        }
    }

    fn mark_failed(&self, spans: &[(String, bool)]) {
        let mut cache = self.math_cache.borrow_mut();
        for (tex, inline) in spans {
            let key = tex_key(tex, *inline);
            cache.entry(key).or_insert(None);
        }
    }

    fn svg_file_uri(&self, fname: &str) -> Option<String> {
        if let Some(v) = self.svg_cache.borrow().get(fname) {
            return v.clone();
        }
        let uri = std::fs::read(self.media_dir.join(fname))
            .ok()
            .and_then(|svg| rasterize_svg(&svg))
            .map(|png| png_data_uri(&png));
        self.svg_cache.borrow_mut().insert(fname.to_string(), uri.clone());
        uri
    }
}

/// Extract a math span's TeX body (inline group 1 or display group 2) and apply
/// the minimal alias pass.
fn span_tex(c: &regex::Captures) -> String {
    let raw = c.get(1).or_else(|| c.get(2)).map(|m| m.as_str()).unwrap_or("");
    alias_macros(raw)
}

/// microtex (clatexmath) covers the AMS/unicode-math commands ReX lacked, so we
/// only normalise the HTML-flavoured MathJax-isms it doesn't define. Matches WHOLE
/// command names (`\[a-zA-Z]+`) so `\gt` can't corrupt `\gtrsim`.
fn alias_macros(tex: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"\\[a-zA-Z]+").unwrap());
    re.replace_all(tex, |c: &regex::Captures| match &c[0][1..] {
        "gt" => ">".to_string(),
        "lt" => "<".to_string(),
        other => format!("\\{other}"),
    })
    .into_owned()
}

/// Build the `<img>` tag for a rendered formula. The PNG is ink-tight and pure
/// black; blitz/parley pins an inline image's bottom to the baseline, so no
/// vertical-align is needed (and it would be ignored anyway).
fn img_tag(r: &Rendered, inline: bool) -> String {
    let w = r.w * DISPLAY_SCALE;
    let h = r.h * DISPLAY_SCALE;
    if inline {
        // Small horizontal margin so the formula doesn't butt against adjacent words.
        format!(
            "<img class=\"am-math\" style=\"width:{w:.1}px;height:{h:.1}px;margin:0 0.12em;\" src=\"{uri}\">",
            uri = r.uri
        )
    } else {
        // Block, centred, natural size, with generous vertical breathing room.
        // max-width keeps a very wide equation from overflowing (height:auto
        // preserves aspect if it has to shrink).
        format!(
            "<img class=\"am-math\" style=\"display:block;margin:26px auto;\
             width:{w:.1}px;max-width:100%;height:auto;\" src=\"{uri}\">",
            uri = r.uri
        )
    }
}

/// For INLINE math: how far (fraction of RENDER_PT) below the math BASELINE the
/// image bottom sits. parley bottom-pins the image to the text baseline, so the
/// math baseline (where the main glyphs H, β, = sit) lands this far ABOVE the text
/// baseline — small so the main glyphs sit low/natural, keeping typical subscripts
/// while trimming only the very bottom of deep descenders (β-tails). Uniform across
/// formulas regardless of height (tall ones don't float up).
const INLINE_DESCENT: f32 = 0.12;

/// Decode a mathpng PNG (transparent bg, dark grey ink), crop it, force the ink to
/// PURE BLACK (max e-ink contrast; mathpng renders it ~#222) keeping the anti-alias
/// alpha, and re-encode. Returns (png, w, h).
///
/// - `inline`: the image BOTTOM is fixed at `baseline + INLINE_DESCENT` (padding short
///   formulas, clipping only very deep descenders) so all inline math shares one
///   baseline offset. Display math (`inline=false`) crops to full ink so tall
///   `\sum`/fraction descenders survive.
/// - Horizontal crop keeps a generous margin + uses a low alpha threshold so slanted
///   italic glyph edges (e.g. an italic F) aren't clipped on the right.
fn finalize_math_png(png: &[u8], inline: bool, baseline: f32) -> Option<(Vec<u8>, f32, f32)> {
    let pm = tiny_skia::Pixmap::decode_png(png).ok()?;
    let (w, h) = (pm.width() as usize, pm.height() as usize);
    let src = pm.data(); // premultiplied RGBA; transparent bg has alpha 0.
    let (mut minx, mut miny, mut maxx, mut maxy) = (w, h, 0usize, 0usize);
    let mut any = false;
    for y in 0..h {
        for x in 0..w {
            // Low threshold so faint anti-aliased edge pixels count → no edge clip.
            if src[(y * w + x) * 4 + 3] > 3 {
                any = true;
                minx = minx.min(x);
                maxx = maxx.max(x);
                miny = miny.min(y);
                maxy = maxy.max(y);
            }
        }
    }
    if !any {
        return None;
    }
    let mx = 12usize; // horizontal margin — italic glyph right edges need headroom
    let mt = 3usize; // top margin
    let x0 = minx.saturating_sub(mx);
    let x1 = (maxx + mx).min(w - 1);
    let y0 = miny.saturating_sub(mt);
    let y1 = if inline {
        // Image bottom = math baseline + a small fixed descent, so simple formulas'
        // main glyphs sit just above the text baseline (low/natural). But NEVER clip:
        // a formula with real ink below that (a fraction's denominator, a deep
        // subscript) extends the box to its ink bottom instead of being cut — it then
        // rides a little higher, which the increased line-height absorbs.
        let d = (INLINE_DESCENT * RENDER_PT) as usize;
        (baseline.round().max(0.0) as usize)
            .saturating_add(d)
            .max(maxy + mt)
            .min(y0 + 4000)
    } else {
        (maxy + mt).min(h - 1)
    };
    let (cw, ch) = (x1 - x0 + 1, y1 - y0 + 1);
    let mut out = tiny_skia::Pixmap::new(cw as u32, ch as u32)?;
    let dst = out.data_mut();
    for y in 0..ch {
        let sy = y0 + y;
        for x in 0..cw {
            let sx = x0 + x;
            // Rows/cols past the source (inline bottom padding) are transparent.
            let a = if sy < h && sx < w {
                src[(sy * w + sx) * 4 + 3]
            } else {
                0
            };
            let di = (y * cw + x) * 4;
            dst[di] = 0;
            dst[di + 1] = 0;
            dst[di + 2] = 0;
            dst[di + 3] = a;
        }
    }
    Some((out.encode_png().ok()?, cw as f32, ch as f32))
}

fn tex_key(tex: &str, inline: bool) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    tex.hash(&mut h);
    inline.hash(&mut h);
    // Fold in the render size so a size change invalidates stale cache entries.
    (RENDER_PT as u32).hash(&mut h);
    h.finish()
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Rasterize a self-contained SVG (path outlines) to PNG bytes over opaque white.
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
    // Crop to the ink so the CSS size maps to the glyph, not the padding.
    autocrop(&pixmap).encode_png().ok()
}

/// Crop a white-background pixmap to its non-white ink bbox (+ a small margin).
fn autocrop(pm: &tiny_skia::Pixmap) -> tiny_skia::Pixmap {
    let (w, h) = (pm.width() as usize, pm.height() as usize);
    let data = pm.data();
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
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
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
