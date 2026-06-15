// Headless integration test — renders the COMPOSITED review screen (top bar +
// card + grade buttons) to PNG using the real Backend + Renderer + ui compositor,
// without QTFB/touch. Lets us verify the full UI on-device by eyeballing the PNGs.
//
// Usage on device:  ./spike_screen   (reads /home/root/.ankimarkable/collection.anki2)

use ankimarkable::backend::Backend;
use ankimarkable::render::Renderer;
use ankimarkable::ui::{self, Phase};

/// Percent-encode a media filename for use in an `<img src>` (keep unreserved).
fn pct_encode(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn save_png(path: &str, rgba: &[u8], w: usize, h: usize) -> anyhow::Result<()> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut wr = enc.write_header()?;
        wr.write_image_data(rgba)?;
    }
    std::fs::write(path, &out)?;
    println!("wrote {path} ({} bytes)", out.len());
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let col = std::env::var("ANKIMARKABLE_COL")
        .unwrap_or_else(|_| "/home/root/.ankimarkable/collection.anki2".to_string());
    let mut be = Backend::open(&col)?;
    let counts = be.counts()?;
    println!(
        "counts new={} learning={} review={}",
        counts.new, counts.learning, counts.review
    );
    let media_dir = std::path::Path::new(&col).with_extension("media");
    let r = Renderer::new().with_media_dir(media_dir.clone());

    // Glyph-fallback probe: emoji + symbols that aren't in Noto Sans/Mono.
    {
        let html = "<!DOCTYPE html><html><body style=\"background:#fff;margin:0;padding:30px;\
            font-family:sans-serif;font-size:56px\">\
            thumbs &#128077; &#128078; star &#9733; check &#10003; cross &#10007; \
            arrow &#8594; bullet &#9642; sun &#9728;</body></html>";
        let buf = r.render_rgba(html, ui::WIDTH as u32, 400);
        save_png("/home/root/screen_glyph.png", &buf, ui::WIDTH, 400)?;
    }

    // Image-provider probe: render the first image we find in media (percent-encode).
    if let Ok(rd) = std::fs::read_dir(&media_dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let lower = name.to_lowercase();
            let is_img = [".jpg", ".jpeg", ".png", ".gif"]
                .iter()
                .any(|ext| lower.ends_with(ext));
            if is_img {
                let enc = pct_encode(&name);
                let html = format!(
                    "<!DOCTYPE html><html><body style=\"background:#fff;margin:0;padding:20px\">\
                     <div style=\"font-family:sans-serif;font-size:32px\">img: {name}</div>\
                     <img src=\"{enc}\" style=\"max-width:1400px\"></body></html>"
                );
                let buf = r.render_rgba(&html, ui::WIDTH as u32, 1000);
                save_png("/home/root/screen_img.png", &buf, ui::WIDTH, 1000)?;
                println!("img probe: {name}");
                break;
            }
        }
    }

    match be.next_card()? {
        Some(card) => {
            let q = ui::compose_review(&r, &card, Phase::Question, counts, "ready");
            save_png("/home/root/screen_q.png", &q, ui::WIDTH, ui::HEIGHT)?;
            let a = ui::compose_review(&r, &card, Phase::Answer, counts, "ready");
            save_png("/home/root/screen_a.png", &a, ui::WIDTH, ui::HEIGHT)?;
            println!("buttons: {:?}", card.button_labels);
        }
        None => {
            let m = ui::compose_message(&r, "All done", "No cards due");
            save_png("/home/root/screen_m.png", &m, ui::WIDTH, ui::HEIGHT)?;
        }
    }
    println!("SPIKE SCREEN OK");
    Ok(())
}
