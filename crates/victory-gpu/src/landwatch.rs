//! Diagnostics: of the pages landing writes into title memory (what shaders
//! wrote, put back after a submission), which does the title's CPU read, and
//! how soon? `VICTORY_LANDED_WATCH=<pages a frame>` (diagnostics builds).
//!
//! A sample of the landed pages (4 KiB, the first page of each landed
//! stretch, up to the given number a frame) is made no-access right after
//! it is written. The first touch faults: the fault handler restores the
//! page as it was and counts who touched it, by the faulting instruction:
//! our executable, the title's code (its image and its DLLs), or a system
//! DLL either may call (`memcpy` and the like, named in the report). A page
//! untouched for [`WINDOW`] presents is restored and counted as such. What
//! it says decides whether landing could wait for a touch (homes' split
//! memory): pages nobody reads need not be put back at all.
//!
//! The kernel's own reads and writes of title memory (ours through
//! `ReadProcessMemory`, DirectStorage's) fail on such a page instead of
//! faulting; the safe copies (on by default) fault, and are counted.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Presents a watched page waits for a touch.
const WINDOW: u64 = 8;

const PAGE: usize = 4096;

/// Pages watched a frame, if on.
pub(crate) fn per_frame() -> u64 {
    static N: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *N.get_or_init(|| std::env::var("VICTORY_LANDED_WATCH").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0))
}

/// A watched page: the present it was landed at; whether it is no-access
/// now (it is opened for each of our touches and watched again after); how
/// often we touched it.
struct Watched {
    at_present: u64,
    armed: bool,
    ours: u32,
}

static WATCHED: Mutex<Option<HashMap<usize, Watched>>> = Mutex::new(None);

/// Whether any page is watched (the fault handler looks only then).
static ANY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Watched; read by the title at all in the window; of those, after a touch
/// of ours; touched by us; never by the title in the window; lost (another
/// change of protection came first); the presents from landing to the
/// title's touch, summed; our touches in all.
static STATS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

/// Touches by thread name: ours (first touch of each page) and the title's.
static NAMES: Mutex<Option<HashMap<(bool, String), u64>>> = Mutex::new(None);

/// The present count, and how many pages were watched since it changed.
static BUDGET: Mutex<(u64, u64)> = Mutex::new((0, 0));

/// This thread's name, and whether it is one of ours (named by us: the
/// runtime's threads, the mapping pool's, the D3D runtime's), asked once.
fn this_thread() -> (bool, String) {
    thread_local! {
        static NAME: std::cell::RefCell<Option<(bool, String)>> = const { std::cell::RefCell::new(None) };
    }
    NAME.with(|n| {
        n.borrow_mut()
            .get_or_insert_with(|| {
                use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadDescription};
                let mut text = String::new();
                // SAFETY: this thread's own name, freed after.
                unsafe {
                    let mut name: windows_sys::core::PWSTR = std::ptr::null_mut();
                    if GetThreadDescription(GetCurrentThread(), &mut name) >= 0 && !name.is_null() {
                        let len = (0..).take_while(|&i| *name.add(i) != 0).count();
                        text = String::from_utf16_lossy(std::slice::from_raw_parts(name, len));
                        windows_sys::Win32::Foundation::LocalFree(name.cast());
                    }
                }
                let ours = ["victory", "mapping", "D3D", "main"].iter().any(|p| text.starts_with(p));
                (ours, if text.is_empty() { "(unnamed)".into() } else { text })
            })
            .clone()
    })
}

/// Our executable's code: `[start, end)`.
fn ours() -> (usize, usize) {
    static OURS: std::sync::OnceLock<(usize, usize)> = std::sync::OnceLock::new();
    *OURS.get_or_init(|| {
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        // SAFETY: our own image's headers.
        unsafe {
            let base = GetModuleHandleW(std::ptr::null()) as usize;
            let pe = base + (base as *const u8).add(0x3c).cast::<u32>().read_unaligned() as usize;
            let size = (pe as *const u8).add(0x50).cast::<u32>().read_unaligned() as usize;
            (base, base + size)
        }
    })
}

/// Makes `page` no-access if it is plain read-write memory the guards leave
/// alone; whether it did.
fn arm(page: usize) -> bool {
    use windows_sys::Win32::System::Memory::{PAGE_NOACCESS, PAGE_READWRITE, VirtualProtect};
    // Only pages the guards leave alone: a guarded one, or one a guard is
    // opening, would be restored to a protection the guards no longer mean
    // (a title write then faulted on a read-only page nobody took: run cfAD).
    if crate::watch::guarded(page, page + PAGE) {
        return false;
    }
    let mut old = 0u32;
    // SAFETY: a page of title memory we landed; restored by `restore`.
    if unsafe { VirtualProtect(page as *const _, PAGE, PAGE_NOACCESS, &mut old) } == 0 {
        return false;
    }
    if old != PAGE_READWRITE {
        let mut was = 0u32;
        // SAFETY: as above: as it was.
        unsafe { VirtualProtect(page as *const _, PAGE, old, &mut was) };
        return false;
    }
    true
}

/// Landing wrote `[at, at + len)` of title memory: its first page may be
/// watched (the frame's budget allowing); pages we touched are watched again.
pub(crate) fn landed(at: usize, len: usize) {
    let most = per_frame();
    if most == 0 || len == 0 {
        return;
    }
    let present = crate::queue::presented();
    expire(present);
    rearm();
    {
        let mut budget = BUDGET.lock().unwrap_or_else(|e| e.into_inner());
        if budget.0 != present {
            *budget = (present, 0);
        }
        if budget.1 >= most {
            return;
        }
        budget.1 += 1;
    }
    let page = at & !(PAGE - 1);
    let mut watched = WATCHED.lock().unwrap_or_else(|e| e.into_inner());
    let map = watched.get_or_insert_with(HashMap::new);
    if map.contains_key(&page) || !arm(page) {
        return;
    }
    map.insert(page, Watched { at_present: present, armed: true, ours: 0 });
    ANY.store(true, Relaxed);
    STATS[0].fetch_add(1, Relaxed);
}

/// Pages opened for a touch of ours: watched again.
fn rearm() {
    let mut watched = WATCHED.lock().unwrap_or_else(|e| e.into_inner());
    let Some(map) = watched.as_mut() else { return };
    let mut lost = Vec::new();
    for (&page, w) in map.iter_mut().filter(|(_, w)| !w.armed) {
        if arm(page) {
            w.armed = true;
        } else {
            lost.push(page);
        }
    }
    for page in lost {
        map.remove(&page);
        STATS[5].fetch_add(1, Relaxed);
    }
}

/// From the fault handler: whether the fault at `at` was on a watched page,
/// which is then opened (the access goes on). A touch of the title's ends
/// the watch of the page; one of ours opens it until the next landing
/// watches it again.
pub(crate) fn fault(at: usize, rip: usize, outer: &str) -> bool {
    if !ANY.load(Relaxed) {
        return false;
    }
    let page = at & !(PAGE - 1);
    let mut watched = WATCHED.lock().unwrap_or_else(|e| e.into_inner());
    let Some(map) = watched.as_mut() else { return false };
    let Some(w) = map.get_mut(&page) else { return false };
    if !w.armed {
        return false;
    }
    restore(page);
    w.armed = false;
    let (ours_thread, name) = this_thread();
    let (lo, hi) = ours();
    let ours = ours_thread || (lo..hi).contains(&rip);
    if ours {
        if w.ours == 0 {
            STATS[3].fetch_add(1, Relaxed);
            let at = format!("{name}: {outer}");
            *NAMES.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).entry((true, at)).or_default() += 1;
        }
        w.ours += 1;
        STATS[7].fetch_add(1, Relaxed);
        return true;
    }
    let w = map.remove(&page).unwrap_or(Watched { at_present: 0, armed: false, ours: 0 });
    STATS[1].fetch_add(1, Relaxed);
    if w.ours > 0 {
        STATS[2].fetch_add(1, Relaxed);
    }
    STATS[6].fetch_add(crate::queue::presented().saturating_sub(w.at_present), Relaxed);
    *NAMES.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).entry((false, name)).or_default() += 1;
    true
}
/// A watched page back to what the guards mean it to be now: read-only if
/// its block is guarded, else read-write (never the protection it had when
/// watched, which a guard may have changed the meaning of since).
fn restore(page: usize) {
    use windows_sys::Win32::System::Memory::{PAGE_READONLY, PAGE_READWRITE, VirtualProtect};
    let to = if crate::watch::guarded(page, page + PAGE) { PAGE_READONLY } else { PAGE_READWRITE };
    let mut was = 0u32;
    // SAFETY: a page of title memory this module made no-access.
    unsafe { VirtualProtect(page as *const _, PAGE, to, &mut was) };
}

/// Pages watched for [`WINDOW`] presents the title did not touch: restored
/// (if no-access still), counted; every 2,048 watched pages, the totals.
fn expire(present: u64) {
    use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, VirtualQuery};
    let old: Vec<(usize, Watched)> = {
        let mut watched = WATCHED.lock().unwrap_or_else(|e| e.into_inner());
        let Some(map) = watched.as_mut() else { return };
        let due: Vec<usize> = map.iter().filter(|(_, w)| present >= w.at_present + WINDOW).map(|(&p, _)| p).collect();
        due.into_iter().filter_map(|p| map.remove(&p).map(|w| (p, w))).collect()
    };
    for (page, w) in old {
        if w.armed {
            // SAFETY: plain query.
            let protect = unsafe {
                let mut info: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
                VirtualQuery(page as *const _, &mut info, size_of_val(&info));
                info.Protect
            };
            if protect != PAGE_NOACCESS {
                // A remap or a guard's change of protection came first.
                STATS[5].fetch_add(1, Relaxed);
                continue;
            }
            restore(page);
        }
        STATS[4].fetch_add(1, Relaxed);
    }
    let watched = STATS[0].load(Relaxed);
    static SAID: AtomicU64 = AtomicU64::new(0);
    if watched >= SAID.load(Relaxed) + 2048 {
        SAID.store(watched, Relaxed);
        report();
    }
}

fn report() {
    let s: Vec<u64> = STATS.iter().map(|n| n.load(Relaxed)).collect();
    let names = NAMES.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default();
    let top = |of_ours: bool| {
        let mut v: Vec<(String, u64)> = names.iter().filter(|((o, _), _)| *o == of_ours).map(|((_, n), c)| (n.clone(), *c)).collect();
        v.sort_unstable_by_key(|x| std::cmp::Reverse(x.1));
        v.truncate(10);
        v
    };
    tracing::info!(
        "landed pages watched (VICTORY_LANDED_WATCH) for {WINDOW} presents, since the start: {} watched; read or written by \
         the title {} ({} of them after a touch of ours), {:.1} presents after landing on average; not by the title \
         {}; touched by us {} pages, {} times; lost to another change {}; our first touches by thread {:?}; the title's by \
         thread {:?}",
        s[0],
        s[1],
        s[2],
        s[6] as f64 / s[1].max(1) as f64,
        s[4],
        s[3],
        s[7],
        s[5],
        top(true),
        top(false)
    );
}