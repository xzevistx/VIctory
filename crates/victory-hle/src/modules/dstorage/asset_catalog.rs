//! Opt-in attribution of completed file requests. Inspect only catalog metadata.
//! An allocation start in a streamed chunk does not identify a material slot.

use super::{HRESULT, PAGE_DESTINATION, Request};
use std::fs::File;
use std::os::windows::fs::FileExt;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_ASSET_CATALOG").is_some_and(|v| v == "1"))
}

pub(super) struct Probe {
    name: String,
    index: Option<Index>,
    reads: AtomicU64,
    matched: AtomicU64,
}

struct Index {
    chunks: Vec<Chunk>,
    assets: Vec<Asset>,
}

struct Chunk {
    source: u64,
    packed: u64,
    start: u64,
    end: u64,
}

struct Asset {
    base: u64,
    ordinal: usize,
    key: u64,
    format: u8,
    id: [u8; 32],
}

impl Probe {
    pub(super) fn open(file: &File, name: &str) -> Option<Self> {
        if !enabled() { return None; }
        let path = name.replace('/', "\\").to_ascii_lowercase();
        let name = path.rsplit('\\').next()?;
        let index = if name.ends_with(".ttpf") {
            match Index::load(file) {
                Some(index) => Some(index),
                None => { tracing::warn!(pool = name, "asset catalog not recognized"); return None; }
            }
        } else if name.ends_with(".rpf") { None } else { return None };
        // Distinguish pools under different title packs. For a local path,
        // use only the basename so machine-specific directories stay private.
        let label = path.strip_prefix("g:\\").map(|s| s.trim_start_matches(".\\")).unwrap_or(name);
        Some(Self { name: label.into(), index, reads: 0.into(), matched: 0.into() })
    }

    pub(super) fn completed(&self, r: &Request, result: &Result<(), HRESULT>) {
        let read = self.reads.fetch_add(1, Ordering::Relaxed) + 1;
        if read <= 2 || read.is_power_of_two() || (result.is_err() && read <= 64) {
            tracing::info!(pool = self.name, read, offset = r.offset, packed = r.source_size,
                decoded = r.destination_size, pages = r.options & PAGE_DESTINATION != 0,
                hr = result.as_ref().err().copied().unwrap_or(0), "asset stream request");
        }
        let Some(index) = &self.index else { return };
        if result.is_err() { return; }
        let Some(chunk) = index.chunk(r.offset, r.source_size) else { return };
        let candidates = index.starts(chunk);
        if candidates.is_empty() { return; }
        // Every allocation whose bytes arrived whole goes into the ledger
        // (`victory_gpu::ledger`), so slot descriptions can name it.
        if u64::from(r.destination_size) == chunk.end - chunk.start {
            for asset in candidates.iter() {
                for at in destination_views(r, (asset.base - chunk.start) as usize) {
                    victory_gpu::ledger::note_asset(at as u64, format!("{} #{} key {:016X} fmt {}",
                        self.name, asset.ordinal, asset.key, asset.format));
                }
            }
        }
        let sample = self.matched.fetch_add(1, Ordering::Relaxed) + 1;
        if sample > 12 && !sample.is_power_of_two() { return; }
        for asset in candidates.iter().take(3) {
            let id: String = asset.id.iter().map(|b| format!("{b:02X}")).collect();
            let views = if u64::from(r.destination_size) == chunk.end - chunk.start {
                destination_views(r, (asset.base - chunk.start) as usize)
            } else { Vec::new() };
            tracing::info!(pool = self.name, read, sample, packed_offset = r.offset, packed_bytes = r.source_size,
                virtual_base = asset.base, candidates = candidates.len(), ordinal = asset.ordinal,
                key = format_args!("{:016X}", asset.key), id, format = asset.format, title_views = ?views,
                "asset catalog allocation starts in completed chunk");
        }
    }
}

/// Where the first byte of this allocation is mapped at completion time.
/// A page can have several aliases, no live mapping, or a different mapping next frame.
fn destination_views(r: &Request, within: usize) -> Vec<usize> {
    if within >= r.destination_size as usize || r.destination.is_null() { return Vec::new(); }
    if r.options & PAGE_DESTINATION == 0 {
        return (r.destination as usize).checked_add(within).into_iter().collect();
    }
    let offset = (r.page_offset as usize & (super::PAGE - 1)) + within;
    // SAFETY: the completed request still holds its page array. The bound
    // above selects one page within the same range used by into_destination.
    let page = unsafe { r.destination.cast::<usize>().add(offset / super::PAGE).read_unaligned() };
    super::super::xmem::shown_at(&[page], offset % super::PAGE, 1).into_iter().map(|(at, _)| at).collect()
}

fn read_at(file: &File, mut offset: u64, mut bytes: &mut [u8]) -> Option<()> {
    while !bytes.is_empty() {
        let n = file.seek_read(bytes, offset).ok()?;
        if n == 0 { return None; }
        offset += n as u64;
        bytes = &mut bytes[n..];
    }
    Some(())
}

impl Index {
    fn chunk(&self, offset: u64, bytes: u32) -> Option<&Chunk> {
        let i = self.chunks.binary_search_by_key(&offset, |c| c.source).ok()?;
        let chunk = &self.chunks[i];
        (chunk.packed == u64::from(bytes)).then_some(chunk)
    }

    fn starts(&self, chunk: &Chunk) -> &[Asset] {
        let first = self.assets.partition_point(|a| a.base < chunk.start);
        let end = self.assets.partition_point(|a| a.base < chunk.end);
        &self.assets[first..end]
    }

    fn load(file: &File) -> Option<Self> {
        const LIMIT: usize = 64 << 20;
        let length = file.metadata().ok()?.len();
        let mut h = [0; 24];
        read_at(file, 0, &mut h)?;
        let word = |b: &[u8]| u32::from_le_bytes(b.try_into().expect("fixed-width word"));
        let wide = |b: &[u8]| u64::from_le_bytes(b.try_into().expect("fixed-width word"));
        let count = word(&h[4..8]) as usize;
        if &h[..4] != b"RDS5" || h[15] != 22 || !(3..=1_000_002).contains(&count) { return None; }
        let virtual_size = wide(&h[8..16]) & 0x00ff_ffff_ffff_ffff;
        let flags = word(&h[20..24]);
        if flags & 0x0fff_ffff != 0 { return None; }
        let padding = u64::from(flags >> 28) * 256;
        let table_bytes = (count - 1) * 8;
        if 24 + table_bytes as u64 > length { return None; }
        let mut table = vec![0; table_bytes];
        read_at(file, 24, &mut table)?;
        let etpf = ((24 + table.len()) as u64).next_multiple_of(256);
        let source = |w: u64| (w & 0x3ff_ffff) * 256;
        let virt = |w: u64| ((w >> 30) & 0x3f_ffff) * 4096;
        let mut chunks = Vec::with_capacity(count - 2);
        for pair in table.windows(16).step_by(8) {
            let (a, b) = (wide(&pair[..8]), wide(&pair[8..]));
            let size = source(b).checked_sub(source(a))?.checked_sub(a >> 52)?;
            if size == 0 || virt(b) <= virt(a) || source(b) > length || virt(b) > virtual_size { return None; }
            chunks.push(Chunk { source: source(a), packed: size, start: virt(a), end: virt(b) });
        }
        let last = wide(&table[table.len()-8..]);
        if source(last) != length || virt(last) != virtual_size { return None; }
        read_at(file, etpf, &mut h[..16])?;
        if &h[..4] != b"ETPF" || word(&h[4..8]) != 634 { return None; }
        let n = word(&h[8..12]) as usize;
        let area = word(&h[12..16]) as usize;
        if n == 0 || n > 250_000 || area < n * 16 { return None; }
        let len = area.checked_add(n * 32)?;
        let data_start = (16 + len as u64).next_multiple_of(4096);
        if len > LIMIT || chunks.first()?.start != data_start || etpf + data_start + padding != chunks[0].source { return None; }
        let mut meta = vec![0; len];
        read_at(file, etpf + 16, &mut meta)?;
        let mut assets = Vec::with_capacity(n);
        for i in 0..n {
            let row = &meta[i*16..][..16];
            let page = word(&row[8..12]) & 0x1f_ffff;
            let base = u64::from(page) * 4096;
            if base < data_start || base >= virtual_size || page == 0x1f_ffff { continue; }
            let key_at = 16*n + i*8;
            let key = wide(&meta[key_at..key_at+8]);
            let id_at = area + i*32;
            let id = meta[id_at..id_at+32].try_into().ok()?;
            assets.push(Asset { base, ordinal: i, key, format: ((word(&row[4..8]) >> 3) & 255) as u8, id });
        }
        assets.sort_unstable_by_key(|a| (a.base, a.ordinal));
        Some(Self { chunks, assets })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn only_exact_reads_report_catalog_starts_and_keep_shared_chunks_ambiguous() {
        // Two records start in one chunk. The high bits of a 64-bit binding
        // key and all 32 bytes of each independent opaque ID must survive.
        let mut blob = vec![0u8; 4608];
        blob[..4].copy_from_slice(b"RDS5");
        blob[4..8].copy_from_slice(&3u32.to_le_bytes());
        blob[8..16].copy_from_slice(&((22u64 << 56) | 8192).to_le_bytes());
        blob[24..32].copy_from_slice(&((1u64 << 30) | 17).to_le_bytes());
        blob[32..40].copy_from_slice(&((2u64 << 30) | 18).to_le_bytes());
        blob[256..260].copy_from_slice(b"ETPF");
        blob[260..264].copy_from_slice(&634u32.to_le_bytes());
        blob[264..268].copy_from_slice(&2u32.to_le_bytes());
        blob[268..272].copy_from_slice(&48u32.to_le_bytes());
        for i in 0..2 {
            let row = 272 + i*16;
            blob[row+8..row+12].copy_from_slice(&1u32.to_le_bytes());
            let at = 304 + i*8;
            blob[at..at+8].copy_from_slice(&(u64::MAX - i as u64).to_le_bytes());
            blob[320+i*32..352+i*32].fill(i as u8 + 1);
        }
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&blob).unwrap();
        let index = Index::load(&file).unwrap();
        let records = index.starts(index.chunk(4352, 256).unwrap());
        assert_eq!(records.len(), 2);
        assert_eq!((records[1].ordinal, records[1].key, records[1].id), (1, u64::MAX - 1, [2; 32]));
        assert!(index.chunk(4352, 128).is_none());
        assert!(index.chunk(4353, 255).is_none());
        file.set_len(4300).unwrap();
        assert!(Index::load(&file).is_none());
    }
}
