//! Review UI — screen layout, HTML chrome generation, and touch hit-testing.
//!
//! The screen is composited from three independent blitz renders blitted into the
//! QTFB framebuffer: a top status bar, the card body (Anki's own HTML/CSS), and a
//! bottom action bar. Keeping the card in its own document stops Anki's per-deck
//! `body{}`/`.card{}` CSS from bleeding into our chrome. Hit-rects are defined here
//! in Rust to match the geometry of the HTML we generate.

use crate::backend::{Counts, Grade, ReviewCard};
use crate::qtfb::{Qtfb, REFRESH_MODE_CONTENT};
use crate::render::Renderer;

pub const WIDTH: usize = 1620;
pub const HEIGHT: usize = 2160;
pub const TOP_H: usize = 130;
pub const BOTTOM_H: usize = 240;
pub const CARD_Y: usize = TOP_H;
pub const CARD_H: usize = HEIGHT - TOP_H - BOTTOM_H;

const SYNC_W: usize = 240;
const EXIT_W: usize = 200;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Question,
    Answer,
}

#[derive(Clone, Copy, Debug)]
pub enum Hit {
    ShowAnswer,
    Grade(Grade),
    Sync,
    Exit,
    None,
}

/// Map a touch in physical panel coords to a UI action for the current phase.
pub fn hit_test(x: i32, y: i32, phase: Phase) -> Hit {
    if x < 0 || y < 0 {
        return Hit::None;
    }
    let (x, y) = (x as usize, y as usize);

    // Top status bar: [counts ............ Sync | Exit]
    if y < TOP_H {
        if x >= WIDTH - EXIT_W {
            return Hit::Exit;
        }
        if x >= WIDTH - EXIT_W - SYNC_W {
            return Hit::Sync;
        }
        return Hit::None;
    }

    // Bottom action bar.
    if y >= HEIGHT - BOTTOM_H {
        return match phase {
            Phase::Question => Hit::ShowAnswer,
            Phase::Answer => {
                let q = WIDTH / 4;
                let idx = (x / q).min(3);
                Hit::Grade(match idx {
                    0 => Grade::Again,
                    1 => Grade::Hard,
                    2 => Grade::Good,
                    _ => Grade::Easy,
                })
            }
        };
    }

    // Tapping the card during the question reveals the answer (convenience).
    if phase == Phase::Question {
        Hit::ShowAnswer
    } else {
        Hit::None
    }
}

/// Composite a full review frame into a fresh WIDTH×HEIGHT RGBA buffer (white bg).
/// Shared by the on-device path (`draw_review`) and the headless screen test.
pub fn compose_review(
    renderer: &Renderer,
    card: &ReviewCard,
    phase: Phase,
    counts: Counts,
    status: &str,
) -> Vec<u8> {
    let mut fb = vec![255u8; WIDTH * HEIGHT * 4];

    let html = match phase {
        Phase::Question => &card.question_html,
        Phase::Answer => &card.answer_html,
    };
    let card_buf = renderer.render_rgba(html, WIDTH as u32, CARD_H as u32);
    blit(&mut fb, &card_buf, WIDTH, CARD_H, 0, CARD_Y);

    let top_buf = renderer.render_rgba(&top_bar_html(counts, status), WIDTH as u32, TOP_H as u32);
    blit(&mut fb, &top_buf, WIDTH, TOP_H, 0, 0);

    let bot_buf =
        renderer.render_rgba(&bottom_bar_html(card, phase), WIDTH as u32, BOTTOM_H as u32);
    blit(&mut fb, &bot_buf, WIDTH, BOTTOM_H, 0, HEIGHT - BOTTOM_H);

    hline_black(&mut fb, CARD_Y - 2, 2);
    hline_black(&mut fb, HEIGHT - BOTTOM_H, 2);
    fb
}

/// Composite + push a full review frame.
pub fn draw_review(
    qtfb: &mut Qtfb,
    renderer: &Renderer,
    card: &ReviewCard,
    phase: Phase,
    counts: Counts,
    status: &str,
) {
    let fb = compose_review(renderer, card, phase, counts, status);
    qtfb.blit_rgba(&fb, WIDTH, HEIGHT, 0, 0);
    let _ = qtfb.set_refresh_mode(REFRESH_MODE_CONTENT);
    let _ = qtfb.update_full();
}

/// Full-screen message (congrats / errors) into a fresh RGBA buffer.
pub fn compose_message(renderer: &Renderer, title: &str, body: &str) -> Vec<u8> {
    let html = format!(
        "<!DOCTYPE html><html><head><style>\
         body{{font-family:sans-serif;margin:0;padding:0;color:#111;background:#fff;\
         display:flex;flex-direction:column;align-items:center;justify-content:center;height:100%;}}\
         .t{{font-size:64px;font-weight:700;margin-bottom:24px;}}\
         .b{{font-size:36px;color:#444;}}</style></head>\
         <body><div class=\"t\">{}</div><div class=\"b\">{}</div></body></html>",
        esc(title),
        esc(body)
    );
    renderer.render_rgba(&html, WIDTH as u32, HEIGHT as u32)
}

/// Full-screen message (congrats / errors).
pub fn draw_message(qtfb: &mut Qtfb, renderer: &Renderer, title: &str, body: &str) {
    let fb = compose_message(renderer, title, body);
    qtfb.blit_rgba(&fb, WIDTH, HEIGHT, 0, 0);
    let _ = qtfb.set_refresh_mode(REFRESH_MODE_CONTENT);
    let _ = qtfb.update_full();
}

/// Alpha-composite an RGBA `src` (`sw`×`sh`) into RGBA `dst` (WIDTH-wide) at
/// (`x`,`y`) over the existing (white) destination. blitz emits transparent /
/// partially-transparent pixels for unstyled backgrounds and anti-aliased glyph
/// edges; compositing over white keeps backgrounds white and text edges clean
/// (a straight copy would turn transparency black and halo the text).
fn blit(dst: &mut [u8], src: &[u8], sw: usize, sh: usize, x: usize, y: usize) {
    for row in 0..sh {
        let py = y + row;
        if py >= HEIGHT {
            break;
        }
        for col in 0..sw {
            let px = x + col;
            if px >= WIDTH {
                break;
            }
            let si = (row * sw + col) * 4;
            let di = (py * WIDTH + px) * 4;
            let a = src[si + 3] as u32;
            if a == 0 {
                continue; // keep dst (white)
            }
            if a == 255 {
                dst[di] = src[si];
                dst[di + 1] = src[si + 1];
                dst[di + 2] = src[si + 2];
            } else {
                let ia = 255 - a;
                for k in 0..3 {
                    dst[di + k] = ((src[si + k] as u32 * a + dst[di + k] as u32 * ia) / 255) as u8;
                }
            }
            dst[di + 3] = 255;
        }
    }
}

fn hline_black(dst: &mut [u8], y: usize, thickness: usize) {
    for yy in y..(y + thickness).min(HEIGHT) {
        for x in 0..WIDTH {
            let di = (yy * WIDTH + x) * 4;
            dst[di] = 0;
            dst[di + 1] = 0;
            dst[di + 2] = 0;
            dst[di + 3] = 255;
        }
    }
}

fn top_bar_html(counts: Counts, status: &str) -> String {
    format!(
        "<!DOCTYPE html><html><head><style>\
         body{{font-family:sans-serif;margin:0;padding:0;height:100%;color:#111;background:#fff;\
         display:flex;align-items:center;}}\
         .counts{{flex:1;font-size:34px;padding-left:32px;}}\
         .n{{color:#1565c0;font-weight:700;}} .l{{color:#c62828;font-weight:700;}} \
         .r{{color:#2e7d32;font-weight:700;}} .status{{font-size:26px;color:#666;padding-right:24px;}}\
         .btn{{font-size:32px;font-weight:700;text-align:center;color:#fff;background:#555;\
         height:100%;display:flex;align-items:center;justify-content:center;}}\
         .sync{{width:{sync}px;}} .exit{{width:{exit}px;background:#333;}}</style></head>\
         <body><div class=\"counts\"><span class=\"n\">{new}</span> &nbsp; \
         <span class=\"l\">{learn}</span> &nbsp; <span class=\"r\">{rev}</span></div>\
         <div class=\"status\">{status}</div>\
         <div class=\"btn sync\">Sync</div><div class=\"btn exit\">Exit</div></body></html>",
        sync = SYNC_W,
        exit = EXIT_W,
        new = counts.new,
        learn = counts.learning,
        rev = counts.review,
        status = esc(status),
    )
}

fn bottom_bar_html(card: &ReviewCard, phase: Phase) -> String {
    let head = "<!DOCTYPE html><html><head><style>\
        body{font-family:sans-serif;margin:0;padding:0;height:100%;background:#fff;}\
        .row{display:flex;height:100%;}\
        .cell{flex:1;display:flex;flex-direction:column;align-items:center;justify-content:center;\
        border-left:2px solid #fff;}\
        .name{font-size:40px;font-weight:700;color:#fff;}\
        .iv{font-size:28px;color:#eee;margin-top:8px;}\
        .show{display:flex;align-items:center;justify-content:center;height:100%;\
        font-size:48px;font-weight:700;color:#fff;background:#1565c0;}</style></head><body>";

    match phase {
        Phase::Question => {
            format!("{head}<div class=\"show\">Show Answer</div></body></html>")
        }
        Phase::Answer => {
            let cells = [
                ("Again", "#c62828", &card.button_labels[0]),
                ("Hard", "#ef6c00", &card.button_labels[1]),
                ("Good", "#2e7d32", &card.button_labels[2]),
                ("Easy", "#1565c0", &card.button_labels[3]),
            ];
            let mut s = format!("{head}<div class=\"row\">");
            for (name, color, iv) in cells {
                s.push_str(&format!(
                    "<div class=\"cell\" style=\"background:{color};\">\
                     <div class=\"name\">{name}</div><div class=\"iv\">{}</div></div>",
                    esc(iv)
                ));
            }
            s.push_str("</div></body></html>");
            s
        }
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
