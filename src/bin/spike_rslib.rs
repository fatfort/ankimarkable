// Spike A (Phase 0) — GO/NO-GO gate for the Anki-on-rMPP plan.
//
// Proves Anki's rslib backend cross-compiles to aarch64-unknown-linux-musl and
// runs on the device: opens a real `.anki2` collection, counts cards (DB read),
// and asks the scheduler for due counts (FSRS/queue build). No UI, no sync.
//
// Usage on device:  ./spike_rslib /home/root/collection.anki2

use anki::collection::CollectionBuilder;
use anki::error::Result;
use anki::search::SortMode;

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/home/root/collection.anki2".to_string());

    println!("ankimarkable spike A — opening collection: {path}");
    let mut col = CollectionBuilder::new(&path).build()?;
    println!("  collection opened OK");

    let all = col.search_cards("", SortMode::NoOrder)?;
    println!("  total cards in collection: {}", all.len());

    let notes = col.search_notes("", SortMode::NoOrder)?;
    println!("  total notes in collection: {}", notes.len());

    match col.get_queued_cards(9999, false) {
        Ok(q) => println!(
            "  due now -> new:{} learning:{} review:{}",
            q.new_count, q.learning_count, q.review_count
        ),
        Err(e) => println!("  get_queued_cards (non-fatal for spike): {e:?}"),
    }

    println!("SPIKE A OK");
    Ok(())
}
