//! The title's page pool as the card's own memory: each section the
//! runtime made committed (`VICTORY_POOL_COMMIT=1`) is opened once as a
//! host heap, with buffers placed over it, so the card reads the title's
//! pages where they are, with no copy by the CPU, whatever views the title
//! maps or unmaps (run `inplace-probe-*`).
//!
//! Sharing the title's graphics memory with the card without copies is
//! what XCL does (github, `xcl`, "title graphics memory shared by CPU and
//! GPU without copies"); none of its code is used here (GPL-2.0-only).
//!
//! The buffers are 1 GiB each (a section of 6.2 GiB is seven), so a raw
//! view's byte offsets stay inside 32 bits; a 64 KiB page never straddles
//! two of them.

use std::collections::HashMap;
use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D12::*;

/// Bytes of one placed buffer.
const CHUNK: u64 = 1 << 30;
/// The title's pages.
const PAGE: u64 = 64 << 10;

struct Opened {
    /// Keeps the heap alive while its buffers are.
    _heap: ID3D12Heap,
    chunks: Vec<ID3D12Resource>,
}

/// By section handle: the section opened, or `None` if the host refused it
/// (asked once).
static OPENED: Mutex<Option<HashMap<usize, Option<Opened>>>> = Mutex::new(None);

/// Whether title pages are read in place by the card
/// (`VICTORY_POOL_IN_PLACE=1`; needs `VICTORY_POOL_COMMIT=1`, else no
/// section can be opened and every texture is detiled by the CPU as
/// before).
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_POOL_IN_PLACE").is_some_and(|v| v == "1"))
}

/// The buffer and the byte offset in it where the card reads the title's
/// 64 KiB page at `page` (a multiple of 64 KiB), if that page is a view of
/// a committed section the host opened.
pub(crate) fn source(device: &ID3D12Device, page: usize) -> Option<(ID3D12Resource, u64)> {
    let (section, size, offset) = crate::inplace::committed_view_at(page)?;
    if !offset.is_multiple_of(PAGE) {
        return None;
    }
    let mut opened = OPENED.lock().unwrap_or_else(|e| e.into_inner());
    let entry = opened.get_or_insert_with(HashMap::new).entry(section).or_insert_with(|| open(device, section, size));
    let section = entry.as_ref()?;
    let chunk = section.chunks.get((offset / CHUNK) as usize)?;
    Some((chunk.clone(), offset % CHUNK))
}

fn open(device: &ID3D12Device, section: usize, size: u64) -> Option<Opened> {
    let made = (|| -> windows::core::Result<Opened> {
        let heap = crate::inplace::open(device, crate::inplace::From::Section(section))?;
        let mut chunks = Vec::new();
        let mut at = 0;
        while at < size {
            let bytes = (size - at).min(CHUNK).next_multiple_of(PAGE);
            let buffer = crate::inplace::place(device, &heap, at, bytes)?;
            crate::host::name(&buffer, || format!("title page pool in place: section {section:#x} from {at:#x}"));
            chunks.push(buffer);
            at += CHUNK;
        }
        Ok(Opened { _heap: heap, chunks })
    })();
    match made {
        Ok(opened) => {
            tracing::info!(section, mb = size >> 20, buffers = opened.chunks.len(), "a section of the title's page pool opened on the card: its pages are read in place");
            Some(opened)
        }
        Err(error) => {
            tracing::warn!(section, mb = size >> 20, %error, "a section of the title's page pool could not be opened on the card: its textures are detiled by the CPU");
            None
        }
    }
}
