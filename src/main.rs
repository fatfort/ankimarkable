//! ankimarkable — Anki review client for the reMarkable Paper Pro (AppLoad/QTFB).
//!
//! Review existing decks on-device with Anki's real backend (rslib): true `.anki2`
//! collection, real FSRS scheduler, AnkiWeb sync, full HTML/CSS card rendering.
//! External AppLoad app — no xochitl injection, no bank-flip risk.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;

use ankimarkable::backend::{self, Backend, Counts};
use ankimarkable::qtfb::{Qtfb, INPUT_TOUCH_PRESS};
use ankimarkable::render::Renderer;
use ankimarkable::ui::{self, Hit, Phase};

static RUNNING: AtomicBool = AtomicBool::new(true);

extern "C" fn on_sigterm(_sig: i32) {
    RUNNING.store(false, Ordering::SeqCst);
}

const DATA_DIR: &str = "/home/root/.ankimarkable";

fn collection_path() -> String {
    std::env::var("ANKIMARKABLE_COL").unwrap_or_else(|_| format!("{DATA_DIR}/collection.anki2"))
}

fn main() {
    // Graceful shutdown on swipe-from-top (AppLoad sends SIGTERM).
    unsafe {
        let h = on_sigterm as extern "C" fn(i32) as libc::sighandler_t;
        libc::signal(libc::SIGTERM, h);
        libc::signal(libc::SIGINT, h);
    }

    let qtfb = match Qtfb::connect_from_env() {
        Ok(q) => q,
        Err(e) => {
            eprintln!("ankimarkable: QTFB connect failed: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = run(qtfb) {
        eprintln!("ankimarkable: {e:#}");
    }
}

fn run(mut qtfb: Qtfb) -> Result<()> {
    let _ = std::fs::create_dir_all(DATA_DIR);
    let col_path = collection_path();
    // Card media lives in the sibling `<collection>.media` folder.
    let media_dir = std::path::Path::new(&col_path).with_extension("media");
    let renderer = Renderer::new().with_media_dir(media_dir);

    let mut backend = match Backend::open(&col_path) {
        Ok(b) => b,
        Err(e) => {
            ui::draw_message(
                &mut qtfb,
                &renderer,
                "No collection",
                &format!("{e}. Tap Exit, set up AnkiWeb creds, then Sync."),
            );
            wait_for_exit(&qtfb);
            return Ok(());
        }
    };

    let mut counts = backend.counts().unwrap_or_default();
    let mut current = backend.next_card()?;
    let mut phase = Phase::Question;
    let mut status = String::new();

    redraw(&mut qtfb, &renderer, &current, phase, counts, &status);

    while RUNNING.load(Ordering::SeqCst) {
        let ev = match qtfb.poll_input() {
            Ok(Some(ev)) => ev,
            Ok(None) => continue,
            Err(_) => break, // socket closed or interrupted by SIGTERM
        };
        if ev.input_type != INPUT_TOUCH_PRESS {
            continue;
        }

        match ui::hit_test(ev.x, ev.y, phase) {
            Hit::Exit => break,
            Hit::Sync => {
                status = do_sync(&mut backend);
                counts = backend.counts().unwrap_or_default();
                current = backend.next_card().unwrap_or(None);
                phase = Phase::Question;
                redraw(&mut qtfb, &renderer, &current, phase, counts, &status);
            }
            Hit::ShowAnswer => {
                if current.is_some() && phase == Phase::Question {
                    phase = Phase::Answer;
                    redraw(&mut qtfb, &renderer, &current, phase, counts, &status);
                }
            }
            Hit::Grade(g) => {
                if phase == Phase::Answer {
                    if let Some(card) = &current {
                        if let Err(e) = backend.answer(card, g) {
                            status = format!("answer error: {e}");
                        }
                    }
                    counts = backend.counts().unwrap_or_default();
                    current = backend.next_card().unwrap_or(None);
                    phase = Phase::Question;
                    redraw(&mut qtfb, &renderer, &current, phase, counts, &status);
                }
            }
            Hit::None => {}
        }
    }

    // Drop(Qtfb) sends MESSAGE_TERMINATE.
    Ok(())
}

fn redraw(
    qtfb: &mut Qtfb,
    renderer: &Renderer,
    current: &Option<backend::ReviewCard>,
    phase: Phase,
    counts: Counts,
    status: &str,
) {
    match current {
        Some(card) => ui::draw_review(qtfb, renderer, card, phase, counts, status),
        None => ui::draw_message(
            qtfb,
            renderer,
            "All done",
            "No more cards due. Tap Sync to fetch, or Exit.",
        ),
    }
}

/// Read AnkiWeb credentials from `DATA_DIR/ankiweb.txt` (line 1 = email/user,
/// line 2 = password) and run a sync. Returns a short status for the chrome.
fn do_sync(backend: &mut Backend) -> String {
    let creds_path = format!("{DATA_DIR}/ankiweb.txt");
    let raw = match std::fs::read_to_string(&creds_path) {
        Ok(s) => s,
        Err(_) => return "no AnkiWeb creds (ankiweb.txt)".to_string(),
    };
    let mut lines = raw.lines();
    let (Some(user), Some(pass)) = (lines.next(), lines.next()) else {
        return "ankiweb.txt needs user + pass lines".to_string();
    };
    match backend.sync(user.trim(), pass.trim()) {
        Ok(s) => s,
        Err(e) => format!("sync failed: {e}"),
    }
}

fn wait_for_exit(qtfb: &Qtfb) {
    while RUNNING.load(Ordering::SeqCst) {
        match qtfb.poll_input() {
            Ok(Some(ev)) => {
                if ev.input_type == INPUT_TOUCH_PRESS {
                    if let Hit::Exit = ui::hit_test(ev.x, ev.y, Phase::Question) {
                        break;
                    }
                }
            }
            Ok(None) => continue,
            Err(_) => break,
        }
    }
}
