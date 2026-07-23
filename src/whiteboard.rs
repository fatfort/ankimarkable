//! Whiteboard ink layer — an AnkiDroid-style pen scratch surface over the card.
//!
//! Model: we keep the composited review frame as an opaque **base** (RGBA, no
//! ink) plus a single-channel **ink** coverage buffer (0=clear, 255=black). The
//! on-screen QTFB framebuffer is always `base` with `ink` composited over it
//! (ink is black, so `out = base * (255-cov) / 255`). Strokes are recorded for
//! undo; the ink buffer is authoritative and can be rebuilt by replaying them.
//!
//! Live inking writes only the moving segment into `ink`, recomputes the affected
//! framebuffer pixels, and issues a fast (DU) partial refresh over the dirty rect
//! — so writing tracks the pen. Phase changes (Show Answer) replace `base` and
//! recomposite with the ink kept on top; advancing to the next card clears ink.
//!
//! Ink is confined to the card region (`ui::CARD_Y .. CARD_Y+CARD_H`); pen samples
//! over the top/bottom chrome are ignored so writing never spills onto the bars.

use crate::pen::PenSample;
use crate::qtfb::{Qtfb, REFRESH_MODE_ANIMATE, REFRESH_MODE_CONTENT, REFRESH_MODE_FAST};
use crate::ui::{CARD_H, CARD_Y, HEIGHT, WIDTH};

/// Eraser brush radius (px) — much larger than a pen stroke so the eraser end
/// clears meaningfully, matching the scratchpad feel.
const ERASE_RADIUS: i32 = 26;

struct Stroke {
    erase: bool,
    // x (panel), y in CARD-CONTENT coords (panel y − CARD_Y + scroll at capture) —
    // so ink scrolls WITH the card: rebuild_ink maps back through the current
    // scroll offset.
    pts: Vec<(i32, i32, u8)>,
}

pub struct Whiteboard {
    base: Vec<u8>, // WIDTH*HEIGHT*4 RGBA — composited review/message, no ink
    ink: Vec<u8>,  // WIDTH*HEIGHT coverage
    strokes: Vec<Stroke>,
    active: Option<Stroke>,
    last: Option<(i32, i32)>,
    /// Card scroll offset (physical px) the ink is currently rebuilt for.
    scroll_y: i32,
    /// True while the card is zoomed (zoom ≠ 1.0): ink is hidden and new ink
    /// refused — strokes are anchored at 1.0 scale and would misalign.
    zoom_hidden: bool,
    // Accumulated dirty rect (x0,y0,x1,y1) pending a coalesced refresh — samples
    // ink into the framebuffer but the e-ink refresh is batched (one per input
    // drain, not one per sample), which is what keeps inking smooth.
    dirty: Option<(i32, i32, i32, i32)>,
    // Region inked with the fast (DU, grayscale) waveform since the last color
    // settle. On the rMPP colour panel a DU refresh drops colour; once writing
    // pauses we re-refresh this region in colour (GC16) so the card's colour comes
    // back with the ink kept crisp.
    settle: Option<(i32, i32, i32, i32)>,
    pub eraser: bool,
}

fn in_card(y: i32) -> bool {
    y >= CARD_Y as i32 && y < (CARD_Y + CARD_H) as i32
}

impl Whiteboard {
    pub fn new() -> Self {
        Self {
            base: vec![255u8; WIDTH * HEIGHT * 4],
            ink: vec![0u8; WIDTH * HEIGHT],
            strokes: Vec::new(),
            active: None,
            last: None,
            scroll_y: 0,
            zoom_hidden: false,
            dirty: None,
            settle: None,
            eraser: false,
        }
    }

    pub fn has_ink(&self) -> bool {
        !self.strokes.is_empty() || self.active.is_some()
    }

    /// Replace the base frame (e.g. after composing the answer side). Ink is kept.
    pub fn set_base(&mut self, frame: Vec<u8>) {
        debug_assert_eq!(frame.len(), self.base.len());
        self.base = frame;
    }

    /// Mutable access to the base frame so scroll steps can patch just the card
    /// band without a full recompose (see `ui::patch_card_band`).
    pub fn base_mut(&mut self) -> &mut [u8] {
        &mut self.base
    }

    /// The card scrolled: re-anchor the ink to the new offset (strokes are stored
    /// in card-content coords, so this is a translate + replay).
    pub fn set_scroll(&mut self, scroll_y: i32) {
        if scroll_y != self.scroll_y {
            self.scroll_y = scroll_y;
            // A stroke can't sanely continue across a scroll step.
            if let Some(st) = self.active.take() {
                if !st.pts.is_empty() {
                    self.strokes.push(st);
                }
            }
            self.last = None;
            self.rebuild_ink();
        }
    }

    /// Hide ink + refuse new strokes while the card is zoomed (≠ 1.0).
    pub fn set_zoom_hidden(&mut self, hidden: bool) {
        self.zoom_hidden = hidden;
    }

    /// Drop all ink + strokes (used when advancing to the next card).
    pub fn clear_ink(&mut self) {
        self.strokes.clear();
        self.active = None;
        self.last = None;
        self.dirty = None;
        self.settle = None;
        for b in self.ink.iter_mut() {
            *b = 0;
        }
    }

    pub fn toggle_eraser(&mut self) {
        self.eraser = !self.eraser;
    }

    /// Push `base` composited with `ink` into the framebuffer and do a clean full
    /// (GC16) refresh — used after any non-inking screen change.
    pub fn blit_full(&mut self, qtfb: &mut Qtfb) {
        let shm: &mut [u8] = &mut *qtfb.shm;
        for idx in 0..(WIDTH * HEIGHT) {
            let bi = idx * 4;
            let di = idx * 3;
            let cov = if self.zoom_hidden { 0 } else { self.ink[idx] as u32 };
            if cov == 0 {
                shm[di] = self.base[bi];
                shm[di + 1] = self.base[bi + 1];
                shm[di + 2] = self.base[bi + 2];
            } else {
                let ia = 255 - cov;
                for k in 0..3 {
                    shm[di + k] = ((self.base[bi + k] as u32 * ia) / 255) as u8;
                }
            }
        }
        let _ = qtfb.set_refresh_mode(REFRESH_MODE_CONTENT);
        let _ = qtfb.update_full();
        // The full GC16 colour refresh supersedes any pending partial AND clears
        // accumulated fast-waveform artifacts (ghosts / "empty white rectangles").
        self.dirty = None;
        self.settle = None;
    }

    /// Push ONLY the card band (base + ink) with a fast DU partial refresh — the
    /// scroll-step hot path. Non-blocking: if the e-ink server is behind, the frame
    /// is in shm and the next step (or the colour settle) will show it. DU (not A2)
    /// because the band is card CONTENT — grayscale/images — not just ink.
    pub fn blit_band_fast(&mut self, qtfb: &mut Qtfb) {
        let shm: &mut [u8] = &mut *qtfb.shm;
        for idx in (CARD_Y * WIDTH)..((CARD_Y + CARD_H) * WIDTH) {
            let bi = idx * 4;
            let di = idx * 3;
            let cov = if self.zoom_hidden { 0 } else { self.ink[idx] as u32 };
            if cov == 0 {
                shm[di] = self.base[bi];
                shm[di + 1] = self.base[bi + 1];
                shm[di + 2] = self.base[bi + 2];
            } else {
                let ia = 255 - cov;
                for k in 0..3 {
                    shm[di + k] = ((self.base[bi + k] as u32 * ia) / 255) as u8;
                }
            }
        }
        let _ = qtfb.try_set_refresh_mode(REFRESH_MODE_FAST);
        let _ = qtfb.try_update_partial(0, CARD_Y as i32, WIDTH as i32, CARD_H as i32);
        // The DU pass dropped colour over the whole band — queue a colour settle.
        self.settle = Some((0, CARD_Y as i32, (WIDTH - 1) as i32, (CARD_Y + CARD_H - 1) as i32));
        self.dirty = None; // superseded: the whole band was just pushed
    }

    /// Undo the last completed stroke: rebuild ink from the remainder, full refresh.
    /// Returns `false` when there was no stroke to undo (caller may then undo the card).
    pub fn undo(&mut self, qtfb: &mut Qtfb) -> bool {
        if self.strokes.pop().is_none() {
            return false;
        }
        self.rebuild_ink();
        self.blit_full(qtfb);
        true
    }

    /// Clear the whiteboard (button): drop ink, restore base, full refresh.
    pub fn clear(&mut self, qtfb: &mut Qtfb) {
        self.clear_ink();
        self.blit_full(qtfb);
    }

    fn rebuild_ink(&mut self) {
        for b in self.ink.iter_mut() {
            *b = 0;
        }
        let strokes = std::mem::take(&mut self.strokes);
        let off = CARD_Y as i32 - self.scroll_y; // content y → panel y
        for st in &strokes {
            let mut prev: Option<(i32, i32)> = None;
            for &(x, cy, w) in &st.pts {
                let y = cy + off;
                let r = ((w as i32) / 2).max(1);
                let (fx, fy) = prev.unwrap_or((x, y));
                // segment_ink clips to the card band per-stamp, so segments that
                // scrolled off-window vanish cleanly.
                segment_ink(&mut self.ink, fx, fy, x, y, r, st.erase);
                prev = Some((x, y));
            }
        }
        self.strokes = strokes;
    }

    /// Ink one pen sample into the framebuffer WITHOUT refreshing — the dirty rect
    /// is accumulated and pushed later by `flush_ink`. This split (many samples, one
    /// refresh) is what makes inking smooth: a QTFB partial-refresh per sample can't
    /// keep up with the pen's sample rate and reads as laggy/jittery.
    pub fn ink_sample(&mut self, s: PenSample, qtfb: &mut Qtfb) {
        // Zoomed card: ink is hidden and anchored at 1.0 scale — refuse new ink.
        if self.zoom_hidden {
            return;
        }
        if s.up {
            if let Some(st) = self.active.take() {
                if !st.pts.is_empty() {
                    self.strokes.push(st);
                }
            }
            self.last = None;
            return;
        }
        // The eraser end (flipped pen) OR the Eraser toggle both erase.
        let erasing = s.erase || self.eraser;
        if s.down || self.active.is_none() {
            self.active = Some(Stroke {
                erase: erasing,
                pts: Vec::new(),
            });
            self.last = None;
        }
        // Confine to the card region; leaving it breaks the current segment.
        if !in_card(s.y) {
            self.last = None;
            return;
        }
        let x = s.x.clamp(0, (WIDTH - 1) as i32);
        let y = s.y;
        let r = if erasing {
            ERASE_RADIUS
        } else {
            ((s.w as i32) / 2).max(1)
        };
        let (fx, fy) = self.last.unwrap_or((x, y));

        // 1. rasterize the segment into ink, tracking its bbox.
        let (mut minx, mut miny, mut maxx, mut maxy) = (x.min(fx), y.min(fy), x.max(fx), y.max(fy));
        segment_ink(&mut self.ink, fx, fy, x, y, r, erasing);
        let pad = r + 1;
        minx = (minx - pad).max(0);
        miny = (miny - pad).max(CARD_Y as i32);
        maxx = (maxx + pad).min((WIDTH - 1) as i32);
        maxy = (maxy + pad).min((CARD_Y + CARD_H - 1) as i32);

        // 2. recompute the framebuffer over that bbox from base+ink (no refresh).
        {
            let shm: &mut [u8] = &mut *qtfb.shm;
            for yy in miny..=maxy {
                for xx in minx..=maxx {
                    let idx = yy as usize * WIDTH + xx as usize;
                    let bi = idx * 4;
                    let di = idx * 3;
                    let cov = self.ink[idx] as u32;
                    if cov == 0 {
                        shm[di] = self.base[bi];
                        shm[di + 1] = self.base[bi + 1];
                        shm[di + 2] = self.base[bi + 2];
                    } else {
                        let ia = 255 - cov;
                        for k in 0..3 {
                            shm[di + k] = ((self.base[bi + k] as u32 * ia) / 255) as u8;
                        }
                    }
                }
            }
        }

        // 3. record the point in CARD-CONTENT coords (so ink scrolls with the card).
        // Erase strokes record too — with the effective eraser width — so both
        // scroll rebuilds and undo replay them faithfully.
        {
            let cy = y - CARD_Y as i32 + self.scroll_y;
            let wrec = if erasing { (ERASE_RADIUS * 2) as u8 } else { s.w };
            if let Some(st) = self.active.as_mut() {
                st.pts.push((x, cy, wrec));
            }
        }
        self.last = Some((x, y));
        self.dirty = Some(match self.dirty {
            None => (minx, miny, maxx, maxy),
            Some((dx0, dy0, dx1, dy1)) => {
                (dx0.min(minx), dy0.min(miny), dx1.max(maxx), dy1.max(maxy))
            }
        });
    }

    /// Push one coalesced fast (DU) partial refresh over everything inked since the
    /// last flush, using NON-BLOCKING sends. If the e-ink server is behind (socket
    /// buffer full) the dirty region is KEPT and retried next flush — this both
    /// prevents the mid-writing hang (a blocking send would stall the loop and let
    /// the marker event buffer back up) and rate-limits refreshes to panel speed.
    pub fn flush_ink(&mut self, qtfb: &mut Qtfb) {
        if let Some((x0, y0, x1, y1)) = self.dirty {
            // A2 waveform for ink — far lower perceived latency than DU. Mode is
            // only actually sent when it changes (see Qtfb), so a continuous stroke
            // stays in A2 with no per-flush mode messages.
            let _ = qtfb.try_set_refresh_mode(REFRESH_MODE_ANIMATE);
            if qtfb
                .try_update_partial(x0, y0, x1 - x0 + 1, y1 - y0 + 1)
                .is_ok()
            {
                // Sent with the grayscale DU waveform — remember the region so a
                // colour settle can restore it once writing pauses.
                self.settle = Some(match self.settle {
                    None => (x0, y0, x1, y1),
                    Some((sx0, sy0, sx1, sy1)) => {
                        (sx0.min(x0), sy0.min(y0), sx1.max(x1), sy1.max(y1))
                    }
                });
                self.dirty = None; // sent; otherwise keep it and retry
            }
        }
    }

    /// True while ink is waiting to be flushed (used to shorten the poll timeout so
    /// a dropped update retries promptly).
    pub fn has_pending_ink(&self) -> bool {
        self.dirty.is_some()
    }

    /// True while a fast-waveform-inked region is waiting for its colour (GC16)
    /// clean.
    pub fn needs_settle(&self) -> bool {
        self.settle.is_some()
    }

    /// Colour-clean ONLY the region inked with the fast waveform (one GC16
    /// `update_partial`, not a whole-screen refresh) — restores the card's colour
    /// and clears fast-waveform artifacts there, with a localized (not full-screen)
    /// flash. Called once writing has paused; the debounce collapses many
    /// mid-sentence pauses into a single settle. Blocking send is fine (we're idle).
    pub fn settle_color(&mut self, qtfb: &mut Qtfb) {
        if let Some((x0, y0, x1, y1)) = self.settle.take() {
            let _ = qtfb.set_refresh_mode(REFRESH_MODE_CONTENT);
            let _ = qtfb.update_partial(x0, y0, x1 - x0 + 1, y1 - y0 + 1);
        }
    }

    /// True while a pen stroke is in progress — used for palm rejection (ignore
    /// finger touches while the pen is writing).
    pub fn is_inking(&self) -> bool {
        self.active.is_some()
    }
}

impl Default for Whiteboard {
    fn default() -> Self {
        Self::new()
    }
}

/// Stamp a filled disc of radius `r` into `ink` at (cx,cy). Draw sets full
/// coverage; erase clears it. Clipped to the card region.
fn stamp(ink: &mut [u8], cx: i32, cy: i32, r: i32, erase: bool) {
    let val = if erase { 0 } else { 255 };
    let r2 = r * r;
    for dy in -r..=r {
        let y = cy + dy;
        if !in_card(y) || y < 0 || y >= HEIGHT as i32 {
            continue;
        }
        for dx in -r..=r {
            if dx * dx + dy * dy > r2 {
                continue;
            }
            let x = cx + dx;
            if x < 0 || x >= WIDTH as i32 {
                continue;
            }
            ink[y as usize * WIDTH + x as usize] = val;
        }
    }
}

/// Rasterize a thick line (stamped discs) from (x0,y0) to (x1,y1) into `ink`.
fn segment_ink(ink: &mut [u8], x0: i32, y0: i32, x1: i32, y1: i32, r: i32, erase: bool) {
    let steps = (x1 - x0).abs().max((y1 - y0).abs()).max(1);
    for i in 0..=steps {
        let x = x0 + (x1 - x0) * i / steps;
        let y = y0 + (y1 - y0) * i / steps;
        stamp(ink, x, y, r, erase);
    }
}
