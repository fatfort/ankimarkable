//! rslib wrapper — the Anki backend the app drives.
//!
//! Owns the open `Collection` and exposes exactly what the review UI needs:
//! due counts, the next due card (already rendered to full card HTML+CSS via
//! Anki's own template engine), grading (real FSRS scheduler), and AnkiWeb sync.
//! Card *rendering to pixels* lives in `render.rs`; this module only produces the
//! HTML/CSS strings that Anki itself would feed a webview.

use anki::card::CardId;
use anki::collection::{Collection, CollectionBuilder};
use anki::scheduler::answering::{CardAnswer, Rating};
use anki::scheduler::states::SchedulingStates;
use anki::sync::collection::normal::SyncActionRequired;
use anki::sync::login::sync_login;
use anki::sync::media::progress::MediaSyncProgress;
use anki::template::RenderedNode;
use anki::timestamp::TimestampMillis;
use anyhow::Result;

/// The four Anki grades, in button order.
#[derive(Clone, Copy, Debug)]
pub enum Grade {
    Again,
    Hard,
    Good,
    Easy,
}

/// Due counts for the current queue (shown as chrome).
#[derive(Clone, Copy, Debug, Default)]
pub struct Counts {
    pub new: usize,
    pub learning: usize,
    pub review: usize,
}

/// A card ready to review: full self-contained HTML for each side plus the
/// per-grade interval previews ("<1m", "1d", …). `states` is retained so the
/// chosen grade maps to the exact next CardState the scheduler computed.
pub struct ReviewCard {
    pub card_id: CardId,
    pub question_html: String,
    pub answer_html: String,
    /// [again, hard, good, easy]
    pub button_labels: [String; 4],
    states: SchedulingStates,
}

pub struct Backend {
    pub col: Collection,
}

impl Backend {
    /// Open (or create) the collection at `col_path`, wiring desktop-style media
    /// paths (`foo.media/`, `foo.mdb`) so card `<img>` and media sync resolve.
    pub fn open(col_path: &str) -> Result<Self> {
        let col = CollectionBuilder::new(col_path)
            .with_desktop_media_paths()
            .build()?;
        Ok(Self { col })
    }

    pub fn counts(&mut self) -> Result<Counts> {
        // fetch_limit 0 → no cards materialised, but counts still come from the queue.
        let q = self.col.get_queued_cards(0, false)?;
        Ok(Counts {
            new: q.new_count,
            learning: q.learning_count,
            review: q.review_count,
        })
    }

    /// The next due card, rendered to full card HTML, or None when the queue is
    /// exhausted (congrats screen).
    pub fn next_card(&mut self) -> Result<Option<ReviewCard>> {
        let queued = self.col.get_queued_cards(1, false)?;
        let Some(qc) = queued.cards.into_iter().next() else {
            return Ok(None);
        };
        let cid = qc.card.id();

        let out = self.col.render_existing_card(cid, false, false)?;
        let question = wrap_card(&out.css, &nodes_to_html(&out.qnodes));
        let answer = wrap_card(&out.css, &nodes_to_html(&out.anodes));

        let labels = self.col.describe_next_states(&qc.states)?;
        let button_labels = [
            labels.first().cloned().unwrap_or_default(),
            labels.get(1).cloned().unwrap_or_default(),
            labels.get(2).cloned().unwrap_or_default(),
            labels.get(3).cloned().unwrap_or_default(),
        ];

        Ok(Some(ReviewCard {
            card_id: cid,
            question_html: question,
            answer_html: answer,
            button_labels,
            states: qc.states,
        }))
    }

    /// Grade a card — writes the new scheduling state to the collection DB.
    pub fn answer(&mut self, card: &ReviewCard, grade: Grade) -> Result<()> {
        let (rating, new_state) = match grade {
            Grade::Again => (Rating::Again, card.states.again),
            Grade::Hard => (Rating::Hard, card.states.hard),
            Grade::Good => (Rating::Good, card.states.good),
            Grade::Easy => (Rating::Easy, card.states.easy),
        };
        let mut answer = CardAnswer {
            card_id: card.card_id,
            current_state: card.states.current,
            new_state,
            rating,
            answered_at: TimestampMillis::now(),
            milliseconds_taken: 0,
            custom_data: None,
            from_queue: true,
        };
        self.col.answer_card(&mut answer)?;
        Ok(())
    }

    /// Sync with AnkiWeb (login → normal sync). Returns a short status string.
    /// A full sync requirement is surfaced (not auto-resolved) — that's rare and
    /// destructive enough to warrant an explicit user choice later.
    pub fn sync(&mut self, username: &str, password: &str) -> Result<String> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        rt.block_on(async {
            // Matches anki's default web client.
            let client = reqwest::Client::builder().http1_only().build()?;
            let auth = sync_login(username, password, None, client.clone()).await?;
            let out = self.col.normal_sync(auth.clone(), client.clone()).await?;

            // Media files (card images/audio) sync separately from the collection DB.
            let mgr = self.col.media()?;
            let progress = self.col.new_progress_handler::<MediaSyncProgress>();
            mgr.sync_media(progress, auth, client, None).await?;

            Ok(match out.required {
                SyncActionRequired::NoChanges => "up to date".to_string(),
                SyncActionRequired::NormalSyncRequired => "synced".to_string(),
                SyncActionRequired::FullSyncRequired { .. } => {
                    "full sync required (resolve on desktop)".to_string()
                }
            })
        })
    }
}

/// Join Anki's rendered template nodes into an HTML string. The template engine
/// has already substituted fields and applied filters, so Text and Replacement
/// both contribute literal HTML.
fn nodes_to_html(nodes: &[RenderedNode]) -> String {
    let mut s = String::new();
    for n in nodes {
        match n {
            RenderedNode::Text { text } => s.push_str(text),
            RenderedNode::Replacement { current_text, .. } => s.push_str(current_text),
        }
    }
    s
}

/// Wrap a card side in the same shell Anki's webview uses: the notetype CSS in a
/// `<style>` block, body class `card`, content in `#qa`.
fn wrap_card(css: &str, body: &str) -> String {
    // White background + black text baseline BEFORE the notetype CSS (which may
    // override) — matches Anki's webview default and keeps blitz output opaque.
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><style>\
         html,body{{background:#fff;color:#000;margin:0;}}{css}</style></head>\
         <body class=\"card\"><div id=\"qa\">{body}</div></body></html>"
    )
}
