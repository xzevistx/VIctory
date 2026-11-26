//! `VICTORY_SAMPLE=1` (diagnostics): a sampling profiler of our own, for
//! machines where the kernel's profiler needs rights the user has not
//! given. Every 2 ms each thread of the process is stopped for as long as
//! reading its instruction pointer takes; every 20 s the log gets where the
//! samples fell, by module and by function (our functions through the
//! PDB beside the executable; the title's code by module only).
//!
//! Nothing is allocated while a thread is stopped (it may hold the heap's
//! lock).

use std::collections::HashMap;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HMODULE};
use windows_sys::Win32::System::Diagnostics::Debug::{
    CONTEXT, CONTEXT_CONTROL_AMD64, GetThreadContext, SYMBOL_INFOW, SymFromAddrW, SymInitializeW, SymSetOptions,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleExW};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId, OpenThread, ResumeThread, SuspendThread,
    THREAD_GET_CONTEXT, THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME,
};

/// Starts the sampler once, if asked for.
pub(crate) fn start() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    if std::env::var_os("VICTORY_SAMPLE").is_none_or(|v| v != "1") {
        return;
    }
    ONCE.call_once(|| {
        tracing::info!("VICTORY_SAMPLE: sampling every thread every 2 ms, reports every 20 s");
        let _ = std::thread::Builder::new().name("victory sampler".into()).spawn(run);
    });
}

#[repr(C, align(16))]
struct Aligned(CONTEXT);

fn threads(me: u32) -> Vec<(u32, HANDLE)> {
    let mut out = Vec::new();
    // SAFETY: plain calls; the snapshot is closed below.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap.is_null() || snap as isize == -1 {
            return out;
        }
        let pid = GetCurrentProcessId();
        let mut e: THREADENTRY32 = std::mem::zeroed();
        e.dwSize = size_of::<THREADENTRY32>() as u32;
        let mut ok = Thread32First(snap, &mut e) != 0;
        while ok {
            if e.th32OwnerProcessID == pid && e.th32ThreadID != me {
                let h = OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION, 0, e.th32ThreadID);
                if !h.is_null() {
                    out.push((e.th32ThreadID, h));
                }
            }
            ok = Thread32Next(snap, &mut e) != 0;
        }
        CloseHandle(snap);
    }
    out
}

fn run() {
    // SAFETY: plain call.
    let me = unsafe { GetCurrentThreadId() };
    let mut hits: HashMap<u64, u32> = HashMap::new();
    let mut list = threads(me);
    let mut listed = std::time::Instant::now();
    let mut reported = std::time::Instant::now();
    let mut samples = 0u64;
    let mut rips = Vec::with_capacity(256);
    loop {
        std::thread::sleep(std::time::Duration::from_millis(2));
        if listed.elapsed().as_millis() > 500 {
            for (_, h) in list.drain(..) {
                // SAFETY: a handle we opened.
                unsafe { CloseHandle(h) };
            }
            list = threads(me);
            listed = std::time::Instant::now();
        }
        rips.clear();
        for &(_, h) in &list {
            let mut c = Aligned(
                // SAFETY: a plain C struct.
                unsafe { std::mem::zeroed() },
            );
            c.0.ContextFlags = CONTEXT_CONTROL_AMD64;
            // SAFETY: the thread is resumed right after its context is
            // read; nothing allocates in between.
            let rip = unsafe {
                if SuspendThread(h) == u32::MAX {
                    continue;
                }
                let ok = GetThreadContext(h, &mut c.0) != 0;
                ResumeThread(h);
                if ok { c.0.Rip } else { 0 }
            };
            if rip != 0 && rips.len() < rips.capacity() {
                rips.push(rip);
            }
        }
        for &rip in &rips {
            *hits.entry(rip).or_default() += 1;
        }
        samples += rips.len() as u64;
        if reported.elapsed().as_secs() >= 20 {
            report(&hits, samples);
            hits.clear();
            samples = 0;
            reported = std::time::Instant::now();
        }
    }
}

fn module_of(rip: u64) -> (u64, String) {
    const FROM_ADDRESS: u32 = 4;
    const UNCHANGED_REFCOUNT: u32 = 2;
    let mut m: HMODULE = std::ptr::null_mut();
    // SAFETY: plain calls with our own buffers.
    unsafe {
        if GetModuleHandleExW(FROM_ADDRESS | UNCHANGED_REFCOUNT, rip as *const u16, &mut m) == 0 {
            return (0, "(no module: generated or title-mapped code)".into());
        }
        let mut name = [0u16; 260];
        let n = GetModuleFileNameW(m, name.as_mut_ptr(), 260) as usize;
        let full = String::from_utf16_lossy(&name[..n]);
        let short = full.rsplit(['\\', '/']).next().unwrap_or(&full).to_owned();
        (m as u64, short)
    }
}

fn symbol_of(process: HANDLE, rip: u64) -> Option<String> {
    #[repr(C, align(8))]
    struct Buf([u8; size_of::<SYMBOL_INFOW>() + 512 * 2]);
    let mut buf = Buf([0; size_of::<SYMBOL_INFOW>() + 512 * 2]);
    let info = buf.0.as_mut_ptr().cast::<SYMBOL_INFOW>();
    // SAFETY: the buffer holds the struct and 512 name characters.
    unsafe {
        (*info).SizeOfStruct = size_of::<SYMBOL_INFOW>() as u32;
        (*info).MaxNameLen = 512;
        let mut displacement = 0u64;
        if SymFromAddrW(process, rip, &mut displacement, info) == 0 {
            return None;
        }
        let len = ((*info).NameLen as usize).min(512);
        let name = std::slice::from_raw_parts((*info).Name.as_ptr(), len);
        Some(String::from_utf16_lossy(name))
    }
}

fn report(hits: &HashMap<u64, u32>, samples: u64) {
    static INIT: std::sync::Once = std::sync::Once::new();
    // SAFETY: plain call.
    let process = unsafe { GetCurrentProcess() };
    INIT.call_once(|| {
        // SAFETY: plain calls; undecorated names, symbols loaded as modules load.
        let dir: Vec<u16> = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.as_os_str().to_owned()))
            .map(|d| {
                use std::os::windows::ffi::OsStrExt;
                d.encode_wide().chain([0]).collect()
            })
            .unwrap_or_else(|| vec![0]);
        unsafe {
            SymSetOptions(0x2 | 0x4 | 0x10);
            SymInitializeW(process, dir.as_ptr(), 1);
        }
    });
    let mut modules: HashMap<String, u64> = HashMap::new();
    let mut functions: HashMap<String, u64> = HashMap::new();
    for (&rip, &n) in hits {
        let (_, module) = module_of(rip);
        *modules.entry(module.clone()).or_default() += u64::from(n);
        let name = match symbol_of(process, rip) {
            Some(s) => format!("{module}!{s}"),
            None => module,
        };
        *functions.entry(name).or_default() += u64::from(n);
    }
    // Threads standing in a wait are left out: the shares are of the
    // samples that found a thread running.
    let waiting = |name: &str| {
        ["Wait", "Delay", "RemoveIoCompletion", "SignalAndWait", "Sleep"]
            .iter()
            .any(|w| name.contains(w))
    };
    let idle: u64 = functions.iter().filter(|(k, _)| waiting(k)).map(|(_, v)| *v).sum();
    functions.retain(|k, _| !waiting(k));
    let samples = samples.saturating_sub(idle);
    let total = samples.max(1) as f64;
    let mut m: Vec<_> = modules.into_iter().collect();
    m.sort_by(|a, b| b.1.cmp(&a.1));
    let text = m
        .iter()
        .take(12)
        .map(|(k, v)| format!("{k} {:.1}%", *v as f64 * 100.0 / total))
        .collect::<Vec<_>>()
        .join(", ");
    tracing::info!(samples, idle, "VICTORY_SAMPLE modules (waits included): {text}");
    let mut f: Vec<_> = functions.into_iter().collect();
    f.sort_by(|a, b| b.1.cmp(&a.1));
    for (k, v) in f.iter().take(60) {
        tracing::info!("VICTORY_SAMPLE {:5.2}% {k}", *v as f64 * 100.0 / total);
    }
}
