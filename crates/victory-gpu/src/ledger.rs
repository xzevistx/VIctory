//! The asset ledger: what each title texture address holds, by the game's
//! own asset catalog, so that a draw's texture slot can be named down to the
//! packed asset it came from. `victory_hle::modules::dstorage::asset_catalog`
//! notes every catalog record whose data a storage read put in title memory
//! (`VICTORY_ASSET_CATALOG=1`); the slot descriptions of the watched-shader
//! and heap-slot logs (`HostHeap::describe_slot`) then say which asset a
//! placed texture view starts at, and in what state VIctory's copy of it is.
//! One place to see, for a black or blotchy surface, whether the asset was
//! never read, read but not uploaded, uploaded in part, or let go.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// Asset starts by title address: the catalog record's text, and the frame
/// its data arrived at that address.
static ASSETS: Mutex<BTreeMap<u64, (String, u64)>> = Mutex::new(BTreeMap::new());

/// At most this many addresses kept: the oldest arrivals go first.
const KEPT: usize = 1 << 18;

/// Notes that the asset `what` (pool, ordinal, key, format) now starts at
/// title address `at`.
pub fn note_asset(at: u64, what: String) {
    let frame = crate::frame::current();
    let mut assets = ASSETS.lock().unwrap_or_else(|e| e.into_inner());
    assets.insert(at, (what, frame));
    if assets.len() > KEPT {
        // (rare: drop the lowest-frame tenth in one sweep)
        let mut frames: Vec<u64> = assets.values().map(|v| v.1).collect();
        frames.sort_unstable();
        let cut = frames[frames.len() / 10];
        assets.retain(|_, v| v.1 > cut);
    }
}

/// The asset that starts at title address `at`, with the frame it arrived.
pub(crate) fn asset_at(at: u64) -> Option<(String, u64)> {
    ASSETS.lock().unwrap_or_else(|e| e.into_inner()).get(&at).cloned()
}

/// How many asset starts are known.
pub(crate) fn known() -> usize {
    ASSETS.lock().unwrap_or_else(|e| e.into_inner()).len()
}
