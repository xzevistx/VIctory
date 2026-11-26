//! Title memory by address, for translated shaders that read and write
//! through raw 40-bit addresses (`victory_shader::emit::SLOT_MEMORY`: the
//! console's ray-tracing structure builder and the programs that walk its
//! structures; the structures themselves lie in plain title memory, in no
//! buffer object).
//!
//! The copies are the ones every other access uses ([`crate::shadow`]: a
//! title buffer's copy, or the 4 MiB window of memory outside buffers), so
//! a root view and a raw address reach the same host bytes. This module
//! keeps what a shader needs to find them:
//!
//! - the **directory**: per 64 KiB page of title address space the number
//!   of the copy that holds the page (0: none) and the page's byte offset
//!   in it; one buffer for all lists, entries only ever go from empty to
//!   filled;
//! - per view heap a run of slots with a raw unordered access view of each
//!   numbered copy (slot 0 of the run: a null view, for pages without a
//!   copy);
//! - the **faults**: the page numbers shaders found empty, read back here
//!   once a frame, so the copies exist from the next frame on. A pointer a
//!   shader takes out of memory cannot be known before it runs; the
//!   addresses a dispatch is given as root views are made ready before it.
//!
//! **A page that is not there when a shader touches it is a lost access**:
//! the read gives 0, the store is dropped, and the fault note only helps
//! the next frame. For the structure builder that is the same as a dropped
//! stage, so before a dispatch the host makes ready everything it can see
//! the dispatch name: the whole copy around each root view, around each
//! pair of root constants that is a title address and around each root
//! constant that is one in 256-byte units (the builder passes structures
//! all three ways), and the 4 MiB after each (a structure may cross a
//! window's end).
//! shortcut: what a stage reaches only through pointers it reads out of
//! memory (InitHeaders' table of structure addresses, LoadInstances'
//! instance records) is not made ready unless something else named it;
//! the fault count in the log says whether that happens. The whole answer
//! is the extent of the title's allocation around an address, which the
//! memory module of the layer knows and this one does not ask yet.
//!
//! shortcut: a copy that is numbered is kept for the rest of the run, and
//! there are at most [`COPIES`] of them (4 MiB windows mostly: 4 GiB at
//! most). Every dispatch of such a shader brings all numbered copies up to
//! date and, if it can store, declares all of them written (the bytes are
//! compared on the CPU afterwards, as for any unordered access view): a
//! dirty-page mask written by the shader would bound both.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use victory_shader::emit::{MEMORY_FAULTS, MEMORY_PAGES};
use windows::Win32::Graphics::Direct3D12::*;

use crate::descriptor::host::HostHeap;
use crate::host::{HostList, host};
use crate::shadow::Shadow;

/// Copies that can be numbered (the run of heap slots is one longer). A
/// listed frame of the opening cutscene names about 50 windows.
const COPIES: u32 = 1023;
/// A window of memory outside buffers (`shadow.rs`).
const CELL: u64 = 4 << 20;
/// A directory page.
const PAGE: u64 = 1 << 16;

struct Memory {
    /// Buffer, its mapping, its GPU address.
    directory: (ID3D12Resource, *mut u32, u64),
    faults: (ID3D12Resource, *mut u32, u64),
    /// Copy `n` is `copies[n - 1]`.
    copies: Vec<Arc<Shadow>>,
    /// Per host view heap (by address): the first slot of its run, and how
    /// many copies have their view there.
    heaps: HashMap<usize, (u32, u32)>,
    /// The frame the faults were last read in.
    frame: u64,
    /// Pages asked for this frame that are not the title's memory.
    refused: std::collections::HashSet<u64>,
    #[cfg(feature = "diagnostics")]
    trace: Option<trace::Trace>,
}

// SAFETY: the mappings are of buffers the struct owns; all access is under
// the mutex.
unsafe impl Send for Memory {}

static MEMORY: Mutex<Option<Memory>> = Mutex::new(None);

/// The global directory only appends copies. Each command list must prepare
/// every copy it can address, independently of which list first numbered it.
#[derive(Default)]
pub(crate) struct PreparedCopies {
    reads: usize,
    writes: usize,
}

impl PreparedCopies {
    /// First new read and write for this dispatch. Each range ends at `end`.
    pub(crate) fn extend(&mut self, end: usize, writes: bool) -> (usize, usize) {
        let read_from = std::mem::replace(&mut self.reads, end);
        let write_from = if writes { std::mem::replace(&mut self.writes, end) } else { end };
        (read_from, write_from)
    }
}

/// What a draw binds.
pub(crate) struct Ready {
    pub(crate) directory: u64,
    pub(crate) faults: u64,
    /// The view heap slot of the null view; copy `n`'s view is `n` further.
    pub(crate) base: u32,
}

impl Memory {
    fn new() -> Option<Memory> {
        let host = host()?;
        let make = |dwords: u64, what: &str| {
            let (buffer, data) = host.shared_buffer(dwords * 4)?;
            crate::host::name(&buffer, || format!("title memory by address: {what}"));
            // SAFETY: a fresh mapping of at least that many bytes.
            unsafe { std::ptr::write_bytes(data, 0, (dwords * 4) as usize) };
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            Some((buffer, data.cast::<u32>(), gpu))
        };
        tracing::info!("title memory by address: a shader reads or writes through raw addresses");
        Some(Memory {
            directory: make(u64::from(MEMORY_PAGES) * 2, "directory")?,
            faults: make(u64::from(MEMORY_FAULTS), "faults")?,
            copies: Vec::new(),
            heaps: HashMap::new(),
            frame: u64::MAX,
            refused: Default::default(),
            #[cfg(feature = "diagnostics")]
            trace: trace::on().then(trace::Trace::default),
        })
    }

    /// Gives the page of `address` its copy, if it has none and the memory
    /// is the title's.
    fn map(&mut self, address: u64) {
        let page = address / PAGE;
        // The last entry stays empty: addresses past the directory read it.
        if page >= u64::from(MEMORY_PAGES) - 1 {
            return;
        }
        // SAFETY: inside the directory's mapping.
        let entry = unsafe { self.directory.1.add(page as usize * 2) };
        // SAFETY: as above.
        if unsafe { entry.read() } != 0 {
            return;
        }
        let start = page * PAGE;
        // A shader's pointer, or a constant taken for an address, may be
        // anything: only memory the title has.
        if self.refused.contains(&page) {
            return;
        }
        if !crate::texture::committed(start as *const u8, PAGE as usize) {
            self.refused.insert(page);
            return;
        }
        // The copy a root view at that address would get; a buffer that
        // ends inside the page is passed over for what holds all of it.
        let whole = |s: &Arc<Shadow>| s.offset_in(start).is_some_and(|_| s.left(start) >= PAGE);
        let Some(shadow) = crate::shadow::find(start, 1)
            .filter(whole)
            .or_else(|| crate::shadow::find(start, PAGE).filter(whole))
        else {
            return;
        };
        // SAFETY: a live buffer.
        let flags = unsafe { shadow.buffer.GetDesc() }.Flags;
        if flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
            return;
        }
        let n = match self.copies.iter().position(|c| Arc::ptr_eq(c, &shadow)) {
            Some(n) => n,
            None if self.copies.len() < COPIES as usize => {
                self.copies.push(shadow.clone());
                let n = self.copies.len() - 1;
                // Every page the copy holds whole, not only the one asked
                // for: a structure is more than 64 KiB.
                // shortcut: a page of a title buffer inside this one that
                // has a copy of its own is given this copy too.
                let (first, len) = shadow.span();
                let mut at = first.next_multiple_of(PAGE);
                while at + PAGE <= first + len && at / PAGE < u64::from(MEMORY_PAGES) - 1 {
                    // SAFETY: inside the directory's mapping.
                    let other = unsafe { self.directory.1.add((at / PAGE) as usize * 2) };
                    // SAFETY: as above; the offset first, as below.
                    unsafe {
                        if other.read() == 0 {
                            other.add(1).write_volatile((at - first) as u32);
                            other.write_volatile(n as u32 + 1);
                        }
                    }
                    at += PAGE;
                }
                n
            }
            None => {
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| {
                    tracing::warn!(COPIES, "title memory by address: no number left for another copy; its pages read zeros")
                });
                return;
            }
        };
        let offset = shadow.offset_in(start).unwrap_or(0);
        // The offset first: a shader that sees the number sees it too.
        // SAFETY: inside the directory's mapping.
        unsafe {
            entry.add(1).write_volatile(offset as u32);
            entry.write_volatile(n as u32 + 1);
        }
    }

    /// The pages shaders found empty since the last frame.
    fn take_faults(&mut self) {
        let mut pages = Vec::new();
        for i in 1..MEMORY_FAULTS as usize {
            // SAFETY: inside the faults' mapping; the GPU may be writing,
            // a number missed now is written again next frame.
            let at = unsafe { self.faults.1.add(i) };
            // SAFETY: as above.
            let page = unsafe { at.read_volatile() } & 0x7FFF_FFFF;
            if page != 0 {
                pages.push(page);
                // SAFETY: as above.
                unsafe { at.write_volatile(0) };
            }
        }
        if !pages.is_empty() {
            // Each is an access that read 0 or was dropped.
            static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 50 {
                tracing::warn!(
                    pages = pages.len(),
                    first = format_args!("{:#x}", u64::from(pages[0]) * PAGE),
                    "title memory by address: shaders touched pages that had no copy (those accesses read 0 or were dropped); the copies are made now"
                );
            }
        }
        for page in pages {
            self.map(u64::from(page) * PAGE);
        }
        #[cfg(feature = "diagnostics")]
        self.take_traced_faults();
    }

    /// Whether `page` has a copy now, else why not (diagnostics).
    #[cfg(feature = "diagnostics")]
    fn page_state(&self, page: u64) -> String {
        if page >= u64::from(MEMORY_PAGES) - 1 {
            return "past the directory".to_owned();
        }
        // SAFETY: inside the directory's mapping.
        let copy = unsafe { self.directory.1.add(page as usize * 2).read() };
        let buffer = crate::shadow::extent(page * PAGE)
            .map_or_else(|| "in no buffer".to_owned(), |(s, l)| format!("in buffer {s:#x}+{l:#x}"));
        if copy != 0 {
            format!("copied (copy {copy}, {buffer})")
        } else if !crate::texture::committed((page * PAGE) as *const u8, PAGE as usize) {
            "not the title's memory".to_owned()
        } else {
            format!("no whole copy with unordered access ({buffer})")
        }
    }

    /// Reads each traced dispatch's own fault notes, makes their copies and
    /// reports a dispatch once its notes are final (diagnostics).
    #[cfg(feature = "diagnostics")]
    fn take_traced_faults(&mut self) {
        let Some(mut t) = self.trace.take() else {
            return;
        };
        let mut done = Vec::new();
        for (i, d) in t.live.iter_mut().enumerate() {
            let before = d.pages.len();
            let stored_before = d.stored.len();
            for j in 1..MEMORY_FAULTS as usize {
                // SAFETY: inside that buffer's mapping; the GPU may still
                // be writing, which a later scan picks up.
                let at = unsafe { d.faults.1.add(j) };
                // SAFETY: as above.
                let note = unsafe { at.read_volatile() };
                if note != 0 {
                    // SAFETY: as above.
                    unsafe { at.write_volatile(0) };
                    let page = note & 0x7FFF_FFFF;
                    if note & 0x8000_0000 != 0 {
                        d.stored.insert(page);
                    }
                    // SAFETY: inside the directory's mapping (pages from
                    // the shader are clamped to it).
                    let copied = unsafe { self.directory.1.add(page as usize * 2).read() } != 0;
                    if !copied && d.pages.insert(page) {
                        self.map(u64::from(page) * PAGE);
                    }
                }
            }
            // At once too, in case the run ends before the full report.
            if before == 0 && !d.pages.is_empty() {
                trace::first_seen(d);
            }
            if d.stored.len() > stored_before {
                trace::stored(d, stored_before);
            }
            d.scans += 1;
            if d.scans >= trace::SCANS {
                done.push(i);
            }
        }
        for i in done.into_iter().rev() {
            let d = t.live.swap_remove(i);
            t.report(&d, |page| self.page_state(page));
            // SAFETY: its whole mapping; the dispatch finished frames ago.
            unsafe { std::ptr::write_bytes(d.faults.1, 0, MEMORY_FAULTS as usize) };
            t.free.push(d.faults);
        }
        self.trace = Some(t);
    }

    /// What a dispatch names: the copy around `address` and the 4 MiB
    /// after it.
    fn name(&mut self, address: u64) {
        self.map(address);
        self.map((address & !(CELL - 1)) + CELL);
    }
}

/// Before a draw whose shader reads title memory by address: what its
/// root arguments name gets its copies (`addresses`: its root views;
/// `constants`: its root constants, see the module), every numbered copy
/// has its view in `heap`'s run and is brought up to date when `list` is
/// submitted (and, with `writes`, copied back after). None if the host
/// cannot hold it; the draw is then dropped.
pub(crate) fn ready(
    list: &HostList,
    heap: &HostHeap,
    addresses: &[u64],
    constants: &[u32],
    writes: bool,
) -> Option<Ready> {
    let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Memory::new();
        // The builder's dispatches come in a burst; the title may fail
        // seconds later with no further dispatch to read the notes at.
        #[cfg(feature = "diagnostics")]
        if trace::on() {
            std::thread::spawn(|| loop {
                std::thread::sleep(std::time::Duration::from_millis(250));
                let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(m) = guard.as_mut() {
                    m.take_traced_faults();
                }
            });
        }
    }
    let m = guard.as_mut()?;
    let frame = crate::frame::current();
    if m.frame != frame {
        m.frame = frame;
        m.refused.clear();
        m.take_faults();
    }
    // A frame can last seconds while the builder runs (and the title may
    // crash inside it): its notes are read in between too.
    #[cfg(feature = "diagnostics")]
    if m.trace.as_mut().is_some_and(trace::Trace::due) {
        m.take_traced_faults();
    }
    for &address in addresses {
        m.name(address);
    }
    // A title address in two constants (low dword, then bits 32 to 39), or
    // in one in 256-byte units. A constant that only looks like one names
    // memory nothing reads; one that is not the title's memory is refused.
    for (i, &c) in constants.iter().enumerate() {
        if let Some(&high) = constants.get(i + 1)
            && (1..=0xFF).contains(&high)
            && c >= 0x1_0000
        {
            m.name(u64::from(c) | u64::from(high) << 32);
        }
        if c >= 0x0100_0000 {
            m.name(u64::from(c) << 8);
        }
    }
    let key = std::ptr::from_ref(heap) as usize;
    let (base, made) = match m.heaps.get(&key) {
        Some(&run) => run,
        None => {
            // Held for the heap's life: not given back with a list's slots.
            let base = heap.dynamic(COPIES + 1)?;
            heap.null_raw_uav_at(base);
            (base, 0)
        }
    };
    for (n, shadow) in m.copies.iter().enumerate().skip(made as usize) {
        let (_, len) = shadow.span();
        heap.raw_uav_at(base + 1 + n as u32, &shadow.buffer, 0, len);
    }
    m.heaps.insert(key, (base, m.copies.len() as u32));
    // Prepare newly visible copies even when a different list numbered them:
    // this list may reach the GPU first.
    const READ: usize = usize::MAX - 0x6d;
    let first_read = list.first_use(READ);
    let (read_from, write_from) = list.prepare_memory_copies(m.copies.len(), writes);
    for (n, shadow) in m.copies.iter().enumerate().skip(read_from.min(write_from)) {
        let (start, len) = shadow.span();
        if n >= read_from {
            shadow.refresh(list, start, len);
        }
        if n >= write_from {
            shadow.gpu_writes(list, start, len);
            #[cfg(feature = "diagnostics")]
            if trace::on() {
                trace::declared("by-address copy", start, len, true);
            }
        }
    }
    if first_read {
        list.keep(&m.directory.0);
        list.keep(&m.faults.0);
    }
    #[allow(unused_mut)]
    let mut faults = m.faults.2;
    #[cfg(feature = "diagnostics")]
    if let Some(t) = m.trace.as_mut()
        && let Some(own) = t.begin(frame, writes)
    {
        list.keep(&own.0);
        faults = own.2;
    }
    Some(Ready {
        directory: m.directory.2,
        faults,
        base,
    })
}

/// `VICTORY_MEMORY_FAULT_TRACE=1` (diagnostics): each dispatch that reads
/// title memory by address gets fault notes of its own instead of the shared
/// ones, so a page without a copy is pinned to the dispatch that touched it
/// (its pipeline, group counts and root arguments). Read with the shared
/// notes once a frame; a dispatch is reported after [`trace::SCANS`] reads.
#[cfg(feature = "diagnostics")]
pub(crate) mod trace {
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

    use super::{MEMORY, MEMORY_FAULTS, PAGE};

    /// Frames a dispatch's notes are read in before it is reported and its
    /// buffer used again.
    pub(super) const SCANS: u32 = 8;
    /// Fault buffers at most (16 KiB each); past that, the shared notes.
    const BUFFERS: usize = 4096;
    /// Report lines at most: dispatches with faults, and without.
    const SAY_FAULTS: u32 = 400;
    const SAY_CLEAN: u32 = 60;

    pub(crate) fn on() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("VICTORY_MEMORY_FAULT_TRACE").is_some_and(|v| v == "1"))
    }

    /// What the next dispatch runs, set by the draw state before its
    /// memory is made ready.
    static NEXT: Mutex<Option<(String, bool)>> = Mutex::new(None);

    pub(crate) fn label(text: String, builder: bool) {
        if builder {
            BUILDER_SEEN.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        *NEXT.lock().unwrap_or_else(|e| e.into_inner()) = Some((text, builder));
    }

    /// A range a draw or dispatch declares as written by shaders (or binds
    /// unwritten, `writes` false), once the builder has run: which views
    /// can put bytes back into which title memory.
    pub(crate) fn declared(what: &str, address: u64, len: u64, writes: bool) {
        static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if builder_seen() && SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 20_000 {
            tracing::info!(writes, "memory fault trace: {what} {address:#x}+{len:#x}");
        }
    }

    static BUILDER_SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// Whether a builder dispatch has been recorded yet.
    pub(crate) fn builder_seen() -> bool {
        BUILDER_SEEN.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Before `after` goes into title memory at `address`: [`ones`] with
    /// the title's bytes there now (none if not mapped).
    pub(crate) fn check_ones(address: usize, after: &[u8], copy: (u64, u64, bool)) {
        if !on() {
            return;
        }
        let mut before = vec![0u8; after.len()];
        if crate::host::read_into(address as *const u8, &mut before) {
            ones(address, &before, after, copy);
        }
    }

    /// A copy-back put `after` over title bytes `before` at `address`
    /// (`copy`: the host copy's title start and length, and whether it is a
    /// window outside buffers): notes 8-byte words it turned into all ones,
    /// the pattern of the pointers the title crashes on with the builder on.
    pub(crate) fn ones(address: usize, before: &[u8], after: &[u8], copy: (u64, u64, bool)) {
        static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let skip = (8 - address % 8) % 8;
        let mut words = 0;
        let mut first = None;
        let mut at = skip;
        while at + 8 <= after.len().min(before.len()) {
            // All ones, or two float infinities (the bounds a structure
            // build starts from, seen in the registers of those crashes).
            let word = |b: &[u8]| u64::from_le_bytes(b[at..at + 8].try_into().unwrap_or_default());
            let marked = |w: u64| w == u64::MAX || [0x7f80_0000, 0xff80_0000].contains(&(w >> 32)) && [0x7f80_0000, 0xff80_0000].contains(&(w & 0xffff_ffff));
            if marked(word(after)) && word(before) != word(after) {
                words += 1;
                first.get_or_insert(address + at);
            }
            at += 8;
        }
        if words == 0 || SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 400 {
            return;
        }
        let (start, len, window) = copy;
        let first = first.unwrap_or_default();
        let old = u64::from_le_bytes(before[first - address..first - address + 8].try_into().unwrap_or_default());
        tracing::warn!(
            address = format_args!("{address:#x}"),
            bytes = after.len(),
            words,
            first = format_args!("{first:#x}"),
            was = format_args!("{old:#x}"),
            copy = format_args!("{start:#x}+{len:#x}"),
            window,
            builder = builder_seen(),
            "memory fault trace: a copy-back writes all-ones words into title memory"
        );
    }

    /// The group counts of the dispatch just made ready.
    pub(crate) fn groups(groups: [u32; 3]) {
        let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = guard.as_mut().and_then(|m| m.trace.as_mut())
            && let Some(d) = t.live.last_mut()
            && d.seq + 1 == t.seq
            && d.groups.is_none()
        {
            d.groups = Some(groups);
            if d.builder {
                let [x, y, z] = groups;
                tracing::info!(seq = d.seq, "memory fault trace: builder dispatch groups {x}x{y}x{z}");
            }
        }
    }

    /// A dispatch's first pages without a copy, as soon as they are read.
    pub(super) fn first_seen(d: &Dispatch) {
        static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 200 {
            let first = d.pages.first().map_or(0, |&p| u64::from(p) * PAGE);
            tracing::warn!(
                seq = d.seq,
                frame = d.frame,
                builder = d.builder,
                pages = d.pages.len(),
                first = format_args!("{first:#x}"),
                "memory fault trace: first faults of {}",
                d.label
            );
        }
    }

    /// Pages a dispatch stored to, as runs, when its notes show new ones.
    pub(super) fn stored(d: &Dispatch, before: usize) {
        static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 3000 {
            return;
        }
        let mut runs: Vec<(u64, u64)> = Vec::new();
        for &p in &d.stored {
            let p = u64::from(p);
            match runs.last_mut() {
                Some(r) if r.1 == p => r.1 = p + 1,
                _ => runs.push((p, p + 1)),
            }
        }
        let text = runs
            .iter()
            .take(32)
            .map(|(a, b)| format!("{:#x}..{:#x}", a * PAGE, b * PAGE))
            .collect::<Vec<_>>()
            .join(" ");
        let name = d.label.split(" | ").next().unwrap_or_default();
        tracing::info!(
            seq = d.seq,
            frame = d.frame,
            builder = d.builder,
            new = d.stored.len() - before,
            pages = d.stored.len(),
            "memory fault trace: {name} stored to {text}"
        );
    }

    pub(super) struct Dispatch {
        pub(super) faults: (ID3D12Resource, *mut u32, u64),
        pub(super) pages: BTreeSet<u32>,
        /// Pages it stored to (`VICTORY_STORE_TRACE`).
        pub(super) stored: BTreeSet<u32>,
        pub(super) scans: u32,
        seq: u64,
        frame: u64,
        writes: bool,
        builder: bool,
        groups: Option<[u32; 3]>,
        label: String,
    }

    #[derive(Default)]
    pub(super) struct Trace {
        pub(super) live: Vec<Dispatch>,
        pub(super) free: Vec<(ID3D12Resource, *mut u32, u64)>,
        seq: u64,
        made: usize,
        said: [u32; 2],
        /// Per pipeline label: dispatches, those with faults, pages.
        tally: std::collections::BTreeMap<String, [u64; 3]>,
        last: Option<std::time::Instant>,
    }

    impl Trace {
        /// Whether a quarter second has passed since the notes were last
        /// read between frames.
        pub(super) fn due(&mut self) -> bool {
            let now = std::time::Instant::now();
            if self.last.is_some_and(|l| now.duration_since(l).as_millis() < 250) {
                return false;
            }
            self.last = Some(now);
            true
        }

        /// Fault notes for the dispatch being made ready.
        pub(super) fn begin(&mut self, frame: u64, writes: bool) -> Option<(ID3D12Resource, *mut u32, u64)> {
            let (label, builder) = NEXT
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .unwrap_or_else(|| ("(an indirect or unlabelled dispatch)".to_owned(), false));
            let faults = match self.free.pop() {
                Some(f) => f,
                None if self.made < BUFFERS => {
                    let host = crate::host::host()?;
                    let (buffer, data) = host.shared_buffer(u64::from(MEMORY_FAULTS) * 4)?;
                    crate::host::name(&buffer, || "title memory by address: one dispatch's faults".to_owned());
                    // SAFETY: a fresh mapping of that many bytes.
                    unsafe { std::ptr::write_bytes(data, 0, MEMORY_FAULTS as usize * 4) };
                    // SAFETY: a live buffer.
                    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
                    self.made += 1;
                    (buffer, data.cast::<u32>(), gpu)
                }
                None => {
                    static ONCE: std::sync::Once = std::sync::Once::new();
                    ONCE.call_once(|| {
                        tracing::warn!(BUFFERS, "memory fault trace: all fault buffers in use; later dispatches share the notes")
                    });
                    return None;
                }
            };
            // The builder's dispatches as they are recorded: their notes may
            // never be read if the title fails inside the frame.
            static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if builder && SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 600 {
                tracing::info!(seq = self.seq, frame, writes, "memory fault trace: builder dispatch {label}");
            }
            self.live.push(Dispatch {
                faults: faults.clone(),
                pages: BTreeSet::new(),
                stored: BTreeSet::new(),
                scans: 0,
                seq: self.seq,
                frame,
                writes,
                builder,
                groups: None,
                label,
            });
            self.seq += 1;
            Some(faults)
        }

        /// One line for a finished dispatch: its pages without a copy as
        /// runs, each with what it is now.
        pub(super) fn report(&mut self, d: &Dispatch, state: impl Fn(u64) -> String) {
            let name = d.label.split(" | ").next().unwrap_or_default().to_owned();
            let tally = self.tally.entry(name).or_default();
            tally[0] += 1;
            if !d.pages.is_empty() {
                tally[1] += 1;
                tally[2] += d.pages.len() as u64;
            }
            let which = usize::from(!d.pages.is_empty());
            let cap = if which == 1 { SAY_FAULTS } else { SAY_CLEAN };
            if self.said[which] >= cap || (which == 0 && !d.builder) {
                return;
            }
            self.said[which] += 1;
            let mut runs: Vec<(u64, u64, String)> = Vec::new();
            for &p in &d.pages {
                let p = u64::from(p);
                let s = state(p);
                match runs.last_mut() {
                    Some(r) if r.1 == p && r.2 == s => r.1 = p + 1,
                    _ => runs.push((p, p + 1, s)),
                }
            }
            let text = runs
                .iter()
                .take(24)
                .map(|(a, b, s)| format!("{:#x}..{:#x} {s}", a * PAGE, b * PAGE))
                .collect::<Vec<_>>()
                .join("; ");
            let [x, y, z] = d.groups.unwrap_or_default();
            tracing::warn!(
                seq = d.seq,
                frame = d.frame,
                builder = d.builder,
                writes = d.writes,
                groups = format_args!("{x}x{y}x{z}"),
                pages = d.pages.len(),
                runs = runs.len(),
                "memory fault trace: {} || missing: {}",
                d.label,
                if text.is_empty() { "none" } else { &text }
            );
            if (self.said[0] + self.said[1]) % 50 == 0 {
                for (name, [n, bad, pages]) in &self.tally {
                    tracing::info!(dispatches = n, with_faults = bad, pages, "memory fault trace tally: {name}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PreparedCopies;

    #[test]
    fn a_list_prepares_copies_numbered_by_another_recording() {
        let mut first = PreparedCopies::default();
        let mut second = PreparedCopies::default();
        assert_eq!(first.extend(1, true), (0, 0));
        // The second recording appends two mappings but has not been submitted.
        assert_eq!(second.extend(3, false), (0, 3));
        assert_eq!(first.extend(3, false), (1, 3));
        assert_eq!(first.extend(3, true), (3, 1));
        assert_eq!(first.extend(3, true), (3, 3));
        assert_eq!(second.extend(3, true), (3, 0));
    }
}
