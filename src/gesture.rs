//! Two-finger scroll + pinch-zoom recognition over AppLoad's QTFB touch stream.
//!
//! AppLoad forwards touch as PRESS(0x10)/RELEASE(0x11) events tagged with a
//! per-contact `dev_id`. Motion delivery on this panel is quirky (see fastterm's
//! gesture.rs, the proven prior art): a moving finger may re-send PRESS with the
//! SAME dev_id per position, and the RELEASE always carries the end position.
//! Some firmware may send UPDATE(0x12) instead. This tracker handles BOTH:
//!
//! - If motion events arrive, a two-finger drag emits live `ScrollStep`s
//!   (quantized so each step is one e-ink refresh) and a pinch is classified
//!   from the live distance ratio.
//! - If no motion arrives (gesture only knowable at release), the whole gesture
//!   is classified when the last finger lifts: net centroid travel → one big
//!   `ScrollStep`, net distance ratio → `PinchEnd`.
//!
//! Only contacts that STARTED in the card region participate in gestures; chrome
//! (top bar / grade bar / ⋮ button) taps are reported via `new_contact` so the
//! caller fires buttons exactly once per physical press (motion re-PRESSes of the
//! same dev_id don't re-trigger).

use std::collections::HashMap;

use crate::qtfb::{INPUT_TOUCH_PRESS, INPUT_TOUCH_RELEASE, INPUT_TOUCH_UPDATE};

/// Quantize live scrolling so each emitted step is one e-ink partial refresh.
const SCROLL_STEP_PX: f32 = 90.0;
/// Distance-change needed to classify the gesture as a pinch (live mode).
const PINCH_THRESHOLD_PX: f32 = 140.0;
/// Centroid travel needed to classify as a scroll (live mode).
const SCROLL_THRESHOLD_PX: f32 = 60.0;
/// Release-classified (no-motion) pinch: ratio outside this band zooms.
const RATIO_IN: f32 = 1.15;
const RATIO_OUT: f32 = 0.87;
/// Release-classified scroll needs at least this much net centroid travel.
const RELEASE_SCROLL_MIN_PX: f32 = 80.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CardGesture {
    None,
    /// Scroll the card by `dy` physical px (positive = content moves up, i.e. the
    /// fingers dragged upward and the reader wants to see further DOWN the card).
    ScrollStep(i32),
    /// Pinch finished with this final/initial finger-distance ratio.
    PinchEnd { ratio: f32 },
}

/// What the caller needs per event: any completed gesture action, plus whether
/// this event was the FIRST press of a new physical contact (for chrome buttons).
pub struct Feed {
    pub gesture: CardGesture,
    pub new_contact: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Undecided,
    Scroll,
    Pinch,
}

struct Contact {
    sx: f32,
    sy: f32,
    lx: f32,
    ly: f32,
    /// Started inside the card region → eligible for gestures.
    card: bool,
}

pub struct CardTouch {
    contacts: HashMap<i32, Contact>,
    /// Peak simultaneous CARD contacts this gesture (survives one-at-a-time lifts).
    peak_card: usize,
    mode: Mode,
    /// Two-finger reference state, captured when the second card contact lands.
    start_dist: f32,
    last_dist: f32,
    start_cy: f32,
    /// Centroid y up to which ScrollSteps have already been emitted.
    applied_cy: f32,
    /// Any live two-finger motion observed this gesture.
    moved: bool,
}

impl Default for CardTouch {
    fn default() -> Self {
        Self::new()
    }
}

impl CardTouch {
    pub fn new() -> Self {
        Self {
            contacts: HashMap::new(),
            peak_card: 0,
            mode: Mode::Undecided,
            start_dist: 0.0,
            last_dist: 0.0,
            start_cy: 0.0,
            applied_cy: 0.0,
            moved: false,
        }
    }

    /// True while a multi-finger card gesture is in progress — the caller must
    /// suppress hit-testing and pen input so a scrolling finger can't fire buttons.
    pub fn multi_active(&self) -> bool {
        self.card_down() >= 2 || (self.peak_card >= 2 && !self.contacts.is_empty())
    }

    pub fn is_tracked(&self, dev_id: i32) -> bool {
        self.contacts.contains_key(&dev_id)
    }

    /// Drop a contact whose events the caller is deliberately ignoring (palm
    /// rejection). Without this, a swallowed RELEASE leaves the contact in the map
    /// forever: `multi_active()` stays latched and every later touch is eaten as a
    /// phantom gesture — the exact wedge the STALE_IDLE recovery has to clean up.
    pub fn forget(&mut self, dev_id: i32) {
        self.contacts.remove(&dev_id);
        if self.contacts.is_empty() {
            self.reset_gesture();
        }
    }

    fn card_down(&self) -> usize {
        self.contacts.values().filter(|c| c.card).count()
    }

    /// The two active card contacts' (distance, centroid_y), if exactly ≥2 down.
    fn two_finger_state(&self) -> Option<(f32, f32)> {
        let pts: Vec<(f32, f32)> = self
            .contacts
            .values()
            .filter(|c| c.card)
            .map(|c| (c.lx, c.ly))
            .collect();
        if pts.len() < 2 {
            return None;
        }
        let d = ((pts[0].0 - pts[1].0).powi(2) + (pts[0].1 - pts[1].1).powi(2)).sqrt();
        let cy = pts.iter().map(|p| p.1).sum::<f32>() / pts.len() as f32;
        Some((d, cy))
    }

    /// Feed one touch event. `in_card` = the position lies in the card region
    /// (chrome presses participate only in `new_contact` dedup, never gestures).
    pub fn feed(&mut self, ty: i32, dev_id: i32, x: i32, y: i32, in_card: bool) -> Feed {
        let (xf, yf) = (x as f32, y as f32);
        match ty {
            INPUT_TOUCH_PRESS => {
                let mut new_contact = false;
                let before_card = self.card_down();
                self.contacts
                    .entry(dev_id)
                    .and_modify(|c| {
                        // Motion re-PRESS: update position only, keep the start.
                        c.lx = xf;
                        c.ly = yf;
                    })
                    .or_insert_with(|| {
                        new_contact = true;
                        Contact {
                            sx: xf,
                            sy: yf,
                            lx: xf,
                            ly: yf,
                            card: in_card,
                        }
                    });
                let now_card = self.card_down();
                if new_contact && now_card == 2 && before_card == 1 {
                    // Second card finger just landed — snapshot the reference state.
                    if let Some((d, cy)) = self.two_finger_state() {
                        self.start_dist = d;
                        self.last_dist = d;
                        self.start_cy = cy;
                        self.applied_cy = cy;
                        self.mode = Mode::Undecided;
                        self.moved = false;
                    }
                }
                self.peak_card = self.peak_card.max(now_card);
                let gesture = if !new_contact {
                    self.on_motion()
                } else {
                    CardGesture::None
                };
                Feed {
                    gesture,
                    new_contact,
                }
            }
            INPUT_TOUCH_UPDATE => {
                if let Some(c) = self.contacts.get_mut(&dev_id) {
                    c.lx = xf;
                    c.ly = yf;
                }
                Feed {
                    gesture: self.on_motion(),
                    new_contact: false,
                }
            }
            INPUT_TOUCH_RELEASE => {
                if let Some(c) = self.contacts.get_mut(&dev_id) {
                    c.lx = xf;
                    c.ly = yf;
                }
                // Capture final two-finger geometry BEFORE removing the contact
                // (this fires on the FIRST lift, while both fingers are still known).
                if let Some((d, cy)) = self.two_finger_state() {
                    self.last_dist = d;
                    if !self.moved {
                        // No live motion this gesture → remember the final centroid
                        // so classify_at_release can measure net travel.
                        self.applied_cy = cy;
                    }
                }
                self.contacts.remove(&dev_id);
                if !self.contacts.is_empty() {
                    return Feed {
                        gesture: CardGesture::None,
                        new_contact: false,
                    };
                }
                // Last finger lifted — classify if this was a ≥2-finger card gesture
                // that the live path did NOT already handle.
                let gesture = if self.peak_card >= 2 && !self.moved {
                    self.classify_at_release()
                } else if self.peak_card >= 2 && self.mode == Mode::Pinch {
                    // Live pinch: report the final ratio once, at completion.
                    if self.start_dist > 1.0 {
                        CardGesture::PinchEnd {
                            ratio: self.last_dist / self.start_dist,
                        }
                    } else {
                        CardGesture::None
                    }
                } else {
                    CardGesture::None
                };
                self.reset_gesture();
                Feed {
                    gesture,
                    new_contact: false,
                }
            }
            _ => Feed {
                gesture: CardGesture::None,
                new_contact: false,
            },
        }
    }

    /// Live two-finger motion: decide scroll-vs-pinch once, then emit quantized
    /// scroll steps (pinch is reported at release, when the ratio is final).
    fn on_motion(&mut self) -> CardGesture {
        let Some((d, cy)) = self.two_finger_state() else {
            return CardGesture::None;
        };
        self.last_dist = d;
        if self.mode == Mode::Undecided {
            if (d - self.start_dist).abs() > PINCH_THRESHOLD_PX {
                self.mode = Mode::Pinch;
                self.moved = true;
            } else if (cy - self.start_cy).abs() > SCROLL_THRESHOLD_PX {
                self.mode = Mode::Scroll;
                self.moved = true;
            }
        }
        if self.mode == Mode::Scroll {
            let dy = cy - self.applied_cy;
            if dy.abs() >= SCROLL_STEP_PX {
                self.applied_cy = cy;
                // Fingers moving UP (dy negative) = reveal content further down =
                // scroll forward by +|dy|.
                return CardGesture::ScrollStep(-dy as i32);
            }
        }
        CardGesture::None
    }

    /// No live motion arrived (PRESS/RELEASE-only firmware): classify the whole
    /// gesture from net start→end geometry at last-finger-lift.
    fn classify_at_release(&self) -> CardGesture {
        if self.start_dist > 1.0 {
            let ratio = self.last_dist / self.start_dist;
            if !(RATIO_OUT..=RATIO_IN).contains(&ratio) {
                return CardGesture::PinchEnd { ratio };
            }
        }
        // Net vertical centroid travel across the card contacts' full lifetimes.
        // (Contacts are gone from the map by now, so this uses the reference
        // captured at gesture start vs the last two-finger centroid we saw.)
        let dy = self.last_cy_estimate() - self.start_cy;
        if dy.abs() >= RELEASE_SCROLL_MIN_PX {
            return CardGesture::ScrollStep(-dy as i32);
        }
        CardGesture::None
    }

    /// Best estimate of the final two-finger centroid y. `applied_cy` tracks the
    /// live path; in no-motion mode it still holds the start, so use the last
    /// distance snapshot's companion: we update it on RELEASE via two_finger_state
    /// before removal — stored in `applied_cy` when not `moved`.
    fn last_cy_estimate(&self) -> f32 {
        self.applied_cy
    }

    fn reset_gesture(&mut self) {
        self.peak_card = 0;
        self.mode = Mode::Undecided;
        self.moved = false;
        self.start_dist = 0.0;
        self.last_dist = 0.0;
        self.start_cy = 0.0;
        self.applied_cy = 0.0;
    }
}
