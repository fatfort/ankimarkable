//! ankimarkable — Anki review client for the reMarkable Paper Pro (AppLoad/QTFB).
//!
//! Review the `uni` deck on-device with Anki's real backend (rslib): true `.anki2`
//! collection, real FSRS scheduler, AnkiWeb sync, full HTML/CSS card rendering,
//! plus an AnkiDroid-style pen **whiteboard** over the card so you can write your
//! answer before revealing. External AppLoad app — no xochitl injection, no
//! bank-flip risk.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;

use ankimarkable::backend::{self, Backend, Counts, DeckInfo, Stats};
use ankimarkable::pen::Pen;
use ankimarkable::qtfb::{Qtfb, INPUT_TOUCH_PRESS};
use ankimarkable::render::Renderer;
use ankimarkable::ui::{self, Hit, HomeHit, Phase};
use ankimarkable::whiteboard::Whiteboard;

static RUNNING: AtomicBool = AtomicBool::new(true);

extern "C" fn on_sigterm(_sig: i32) {
    RUNNING.store(false, Ordering::SeqCst);
}

const DATA_DIR: &str = "/home/root/.ankimarkable";

/// Which screen is showing. The app opens to Home (deck list + streak); tapping a
/// deck enters Review; the Home button returns.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Home,
    Review,
}

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
    let mut wb = Whiteboard::new();

    // Read the streak BEFORE opening the collection — anki takes an exclusive lock
    // on open, after which no other connection can read `revlog`.
    let base_stats = backend::read_stats(&col_path);

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

    // The pen ("Elan marker") is read directly from evdev; if it's unavailable the
    // app still works finger-only (buttons via the QTFB touch socket).
    let mut pen = match Pen::open() {
        Ok(p) => {
            eprintln!("ankimarkable: pen at {}", p.dev_path);
            Some(p)
        }
        Err(e) => {
            eprintln!("ankimarkable: no pen ({e}) — finger-only");
            None
        }
    };

    // App opens on the Home screen: a collapsible deck tree + streak.
    let mut screen = Screen::Home;
    let mut all_decks: Vec<DeckInfo> = backend.deck_infos().unwrap_or_default();
    // Runtime collapse state, keyed by deck id (empty = all expanded so subdecks
    // are visible; the chevron toggles per deck).
    let mut collapsed: HashSet<i64> = HashSet::new();
    let mut scroll: usize = 0;
    let mut visible = visible_decks(&all_decks, &collapsed);
    // Reviews graded this session — added to the startup `reviewed_today` (the DB
    // is locked now so we can't re-read it).
    let mut session_reviews: u32 = 0;
    // Palm rejection: touches within this window of the last pen sample are ignored.
    let mut last_pen: Option<Instant> = None;
    const PALM_WINDOW: Duration = Duration::from_millis(250);
    // Idle gap after which a colour settle fires — long enough that pauses BETWEEN
    // words while handwriting a sentence don't each trigger a refresh (they collapse
    // into one settle once you actually stop).
    const SETTLE_IDLE: Duration = Duration::from_millis(1200);

    // Review state, populated when a deck is picked.
    let mut counts = Counts::default();
    let mut current: Option<backend::ReviewCard> = None;
    let mut menu_open = false; // ⋮ overflow menu (Bury card / Suspend note)
    let mut phase = Phase::Question;
    let mut status = String::new();

    ui::draw_home(
        &mut qtfb,
        &renderer,
        &visible,
        scroll,
        home_stats(base_stats, session_reviews),
    );

    while RUNNING.load(Ordering::SeqCst) {
        // Flush any pending ink FIRST (non-blocking; keeps the dirty region if the
        // e-ink server is still busy). Retrying at the loop top means a deferred
        // update lands promptly without ever blocking the loop. Once writing has
        // clearly PAUSED (SETTLE_IDLE since the last pen sample — long enough that
        // mid-sentence word-pauses don't each trigger one), do ONE colour (GC16)
        // clean of just the inked REGION: the fast DU waveform used while writing is
        // grayscale + leaves "white rectangle" artifacts on the colour panel; the
        // region clean restores colour + wipes them with only a small localized
        // flash (never a whole-screen refresh). The debounce collapses the many
        // pauses in a sentence into a single settle.
        if screen == Screen::Review {
            wb.flush_ink(&mut qtfb);
            if wb.needs_settle() && last_pen.map_or(true, |t| t.elapsed() >= SETTLE_IDLE) {
                wb.settle_color(&mut qtfb);
            }
        }
        let reviewing = screen == Screen::Review;

        // Poll the QTFB touch socket and (if present) the pen evdev fd together.
        let mut fds: Vec<libc::pollfd> = Vec::with_capacity(2);
        fds.push(libc::pollfd {
            fd: qtfb.raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        if let Some(p) = &pen {
            fds.push(libc::pollfd {
                fd: p.fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        // Short timeout while ink is pending so a deferred flush retries within ~30ms;
        // a medium one while a colour settle is due so it fires shortly after a pause.
        let timeout = if reviewing && wb.has_pending_ink() {
            30
        } else if reviewing && wb.needs_settle() {
            120
        } else {
            1000
        };
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if rc <= 0 {
            // EINTR (SIGTERM) or timeout — loop back; the top-of-loop flush retries.
            continue;
        }

        // Pen: drain ALL samples and ink them (the refresh is flushed at the loop
        // top, coalesced and non-blocking — never per-sample, never blocking).
        if fds.len() == 2 && fds[1].revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
            let mut inked = false;
            let r = pen.as_mut().unwrap().drain(|s| {
                inked = true;
                if reviewing {
                    wb.ink_sample(s, &mut qtfb);
                }
            });
            if inked {
                last_pen = Some(Instant::now());
                // Ship the freshly-inked pixels NOW (same iteration) rather than
                // waiting for the next poll cycle — one less cycle of ink latency.
                if reviewing {
                    wb.flush_ink(&mut qtfb);
                }
            }
            if r.is_err() {
                eprintln!("ankimarkable: pen read error — dropping pen");
                pen = None;
            }
        }

        if fds[0].revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) == 0 {
            continue;
        }
        let ev = match qtfb.recv_input_nonblock() {
            Ok(Some(ev)) => ev,
            Ok(None) => continue,
            Err(_) => break, // socket closed
        };
        if ev.input_type != INPUT_TOUCH_PRESS {
            continue;
        }
        // Palm rejection: ignore finger/palm touches within PALM_WINDOW of the last
        // pen activity (auto-expires, so a missed pen-up can never lock out buttons).
        if reviewing && last_pen.map_or(false, |t| t.elapsed() < PALM_WINDOW) {
            continue;
        }

        match screen {
            Screen::Home => match ui::hit_test_home(ev.x, ev.y, &visible, scroll) {
                HomeHit::Deck(i) => {
                    if let Some(d) = visible.get(i) {
                        let _ = backend.set_deck_scope_id(d.id);
                        counts = backend.counts().unwrap_or_default();
                        current = backend.next_card().unwrap_or(None);
                        phase = Phase::Question;
                        status = d.name.clone();
                        wb.clear_ink();
                        screen = Screen::Review;
                        redraw(&mut qtfb, &renderer, &mut wb, &current, phase, counts, &status, menu_open);
                    }
                }
                HomeHit::Toggle(i) => {
                    if let Some(d) = visible.get(i) {
                        let id = d.id.0;
                        if !collapsed.remove(&id) {
                            collapsed.insert(id);
                        }
                        visible = visible_decks(&all_decks, &collapsed);
                        clamp_scroll(&mut scroll, visible.len());
                        ui::draw_home(
                            &mut qtfb,
                            &renderer,
                            &visible,
                            scroll,
                            home_stats(base_stats, session_reviews),
                        );
                    }
                }
                HomeHit::ScrollUp => {
                    let step = ui::home_page_rows().saturating_sub(1).max(1);
                    scroll = scroll.saturating_sub(step);
                    ui::draw_home(
                        &mut qtfb,
                        &renderer,
                        &visible,
                        scroll,
                        home_stats(base_stats, session_reviews),
                    );
                }
                HomeHit::ScrollDown => {
                    let step = ui::home_page_rows().saturating_sub(1).max(1);
                    let max_scroll = visible.len().saturating_sub(ui::home_page_rows());
                    scroll = (scroll + step).min(max_scroll);
                    ui::draw_home(
                        &mut qtfb,
                        &renderer,
                        &visible,
                        scroll,
                        home_stats(base_stats, session_reviews),
                    );
                }
                HomeHit::Sync => {
                    ui::draw_message(
                        &mut qtfb,
                        &renderer,
                        "Syncing…",
                        "Contacting AnkiWeb — this can take a moment.",
                    );
                    let s = do_sync(&mut backend);
                    all_decks = backend.deck_infos().unwrap_or_default();
                    visible = visible_decks(&all_decks, &collapsed);
                    clamp_scroll(&mut scroll, visible.len());
                    eprintln!("ankimarkable: sync from home: {s}");
                    ui::draw_home(
                        &mut qtfb,
                        &renderer,
                        &visible,
                        scroll,
                        home_stats(base_stats, session_reviews),
                    );
                }
                HomeHit::Exit => break,
                HomeHit::None => {}
            },
            // While the ⋮ menu is open, taps hit its items (or dismiss it).
            Screen::Review if menu_open => {
                let act = ui::hit_menu(ev.x, ev.y);
                menu_open = false;
                let advanced = match act {
                    ui::MenuHit::Bury => {
                        if let Some(card) = &current {
                            if let Err(e) = backend.bury_card(card.card_id) {
                                status = format!("bury error: {e}");
                            }
                        }
                        true
                    }
                    ui::MenuHit::Suspend => {
                        if let Some(card) = &current {
                            if let Err(e) = backend.suspend_note(card.note_id) {
                                status = format!("suspend error: {e}");
                            }
                        }
                        true
                    }
                    ui::MenuHit::None => false, // tapped outside → just dismiss
                };
                if advanced {
                    counts = backend.counts().unwrap_or_default();
                    current = backend.next_card().unwrap_or(None);
                    phase = Phase::Question;
                    wb.clear_ink();
                }
                redraw(&mut qtfb, &renderer, &mut wb, &current, phase, counts, &status, menu_open);
            }
            Screen::Review => match ui::hit_test(ev.x, ev.y, phase) {
                Hit::Exit => break,
                Hit::Home => {
                    // Back to the deck list; rebuild deck counts (streak is cached).
                    screen = Screen::Home;
                    all_decks = backend.deck_infos().unwrap_or_default();
                    visible = visible_decks(&all_decks, &collapsed);
                    clamp_scroll(&mut scroll, visible.len());
                    wb.clear_ink();
                    ui::draw_home(
                        &mut qtfb,
                        &renderer,
                        &visible,
                        scroll,
                        home_stats(base_stats, session_reviews),
                    );
                }
                Hit::Sync => {
                    ui::draw_message(
                        &mut qtfb,
                        &renderer,
                        "Syncing…",
                        "Contacting AnkiWeb — this can take a moment.",
                    );
                    status = do_sync(&mut backend);
                    counts = backend.counts().unwrap_or_default();
                    current = backend.next_card().unwrap_or(None);
                    phase = Phase::Question;
                    wb.clear_ink();
                    redraw(&mut qtfb, &renderer, &mut wb, &current, phase, counts, &status, menu_open);
                }
                Hit::ShowAnswer => {
                    if current.is_some() && phase == Phase::Question {
                        phase = Phase::Answer;
                        // Keep ink so you can compare your answer against the card.
                        redraw(&mut qtfb, &renderer, &mut wb, &current, phase, counts, &status, menu_open);
                    }
                }
                Hit::Grade(g) => {
                    if phase == Phase::Answer {
                        if let Some(card) = &current {
                            match backend.answer(card, g) {
                                Ok(()) => session_reviews += 1,
                                Err(e) => status = format!("answer error: {e}"),
                            }
                        }
                        counts = backend.counts().unwrap_or_default();
                        current = backend.next_card().unwrap_or(None);
                        phase = Phase::Question;
                        wb.clear_ink(); // next card starts on a clean whiteboard
                        redraw(&mut qtfb, &renderer, &mut wb, &current, phase, counts, &status, menu_open);
                    }
                }
                Hit::Undo => {
                    // Undo the last pen stroke; when the whiteboard is empty, fall
                    // through to undoing the last graded card (Anki collection undo).
                    if !wb.undo(&mut qtfb) {
                        match backend.undo() {
                            Ok(true) => {
                                session_reviews = session_reviews.saturating_sub(1);
                                counts = backend.counts().unwrap_or_default();
                                current = backend.next_card().unwrap_or(None);
                                phase = Phase::Question;
                                wb.clear_ink();
                                redraw(
                                    &mut qtfb, &renderer, &mut wb, &current, phase, counts,
                                    &status, menu_open,
                                );
                            }
                            _ => {} // nothing to undo
                        }
                    }
                }
                Hit::Clear => wb.clear(&mut qtfb),
                Hit::EraserToggle => {
                    wb.toggle_eraser();
                    // Recompose so the toolbar reflects the toggle; ink is kept.
                    redraw(&mut qtfb, &renderer, &mut wb, &current, phase, counts, &status, menu_open);
                }
                Hit::Menu => {
                    // Open the ⋮ overflow menu (Bury card / Suspend note).
                    menu_open = true;
                    redraw(&mut qtfb, &renderer, &mut wb, &current, phase, counts, &status, menu_open);
                }
                Hit::None => {}
            },
        }
    }

    // Drop(Qtfb) sends MESSAGE_TERMINATE.
    Ok(())
}

/// Compose the current screen into the whiteboard's base frame and push it (base +
/// any ink) to the panel with a clean full refresh.
fn redraw(
    qtfb: &mut Qtfb,
    renderer: &Renderer,
    wb: &mut Whiteboard,
    current: &Option<backend::ReviewCard>,
    phase: Phase,
    counts: Counts,
    status: &str,
    menu_open: bool,
) {
    let eraser = wb.eraser;
    let frame = match current {
        Some(card) => ui::compose_review(renderer, card, phase, counts, status, eraser, menu_open),
        None => ui::compose_message(
            renderer,
            "All done",
            "No more cards due. Tap Sync to fetch, or Exit.",
        ),
    };
    wb.set_base(frame);
    wb.blit_full(qtfb);
}

/// Filter the full deck tree to the rows visible given the runtime collapse state:
/// a deck is hidden when any ancestor is collapsed. Each row's `collapsed` field is
/// set to the runtime state so the chevron renders correctly.
fn visible_decks(all: &[DeckInfo], collapsed: &HashSet<i64>) -> Vec<DeckInfo> {
    let mut out = Vec::new();
    let mut hide: Option<u32> = None; // hide decks deeper than this level
    for d in all {
        if let Some(h) = hide {
            if d.level > h {
                continue;
            }
            hide = None;
        }
        let is_collapsed = collapsed.contains(&d.id.0);
        let mut row = d.clone();
        row.collapsed = is_collapsed;
        out.push(row);
        if is_collapsed && d.has_children {
            hide = Some(d.level);
        }
    }
    out
}

/// Keep `scroll` within range after the visible list changes.
fn clamp_scroll(scroll: &mut usize, len: usize) {
    let max_scroll = len.saturating_sub(ui::home_page_rows());
    if *scroll > max_scroll {
        *scroll = max_scroll;
    }
}

/// Combine the startup streak with this session's grades: `reviewed_today` adds
/// the in-session count, and if nothing had been studied today at startup, the
/// first grade extends the streak by one (today now counts).
fn home_stats(base: Stats, session_reviews: u32) -> Stats {
    let studied_today_at_start = base.reviewed_today > 0;
    Stats {
        reviewed_today: base.reviewed_today + session_reviews,
        streak: if !studied_today_at_start && session_reviews > 0 {
            base.streak + 1
        } else {
            base.streak
        },
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
