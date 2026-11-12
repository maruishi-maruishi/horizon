//! The two tiers of a world in Horizon's storage (docs/horizon-storage.md): per folder
//! (`region`, `entities`, `poi` of a dimension) recently written regions in `.hzr` files (the hot
//! tier, [`crate::hzr`]) over one `.hza` archive (the cold tier, [`crate::archive`]).
//!
//! A world is in this storage when its folder holds [`MARKER`]; the game never adds it to or
//! removes it from a world that has chunks (the conversion tool does, [`crate::region_convert`]),
//! but the dedicated server writes it into a world it is about to create ([`adopt_new_world`]). Reads look in the hot tier, then the
//! cold one; writes go to the hot tier; a region nobody wrote for `settle_after_seconds` settles
//! into the archive on a background thread (recompressed with the archive's dictionary) and its
//! `.hzr` file goes away.
//!
//! Locks nest one way only: a region's hot slot, then its folder's archive.
use crate::archive::{ColdArchive, Prepared, train_dictionary};
use crate::compression::{invalid, set_horizon_format};
use crate::hzr::{HzrRegion, Slot, to_zstd};
use crate::region::ChunkRecord;
use horizon_core::position::ChunkPos;
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Once, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use xxhash_rust::xxh32::xxh32;

/// The file in a world folder that says the world is in this storage.
pub const MARKER: &str = "horizon-storage.json";
/// The archive's file name in each folder.
pub const ARCHIVE: &str = "archive.hza";
/// Raw chunks a dictionary is trained on, at most.
const DICT_SAMPLES: usize = 2000;

/// A world's storage settings, from its [`MARKER`].
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Marker {
    pub format: String,
    pub version: u32,
    #[serde(default = "default_settle")]
    pub settle_after_seconds: u64,
}

fn default_settle() -> u64 {
    600
}

impl Default for Marker {
    fn default() -> Self {
        Self {
            format: "hzr".into(),
            version: 1,
            settle_after_seconds: default_settle(),
        }
    }
}

#[derive(Debug)]
pub struct Storage {
    pub root: PathBuf,
    pub marker: Marker,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn storages() -> &'static Mutex<HashMap<PathBuf, Option<Arc<Storage>>>> {
    static S: OnceLock<Mutex<HashMap<PathBuf, Option<Arc<Storage>>>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

fn folders() -> &'static Mutex<HashMap<PathBuf, Arc<Folder>>> {
    static F: OnceLock<Mutex<HashMap<PathBuf, Arc<Folder>>>> = OnceLock::new();
    F.get_or_init(Default::default)
}

/// The storage of the world a region folder belongs to, None for a vanilla world.
pub fn storage_for(folder: &Path) -> Option<Arc<Storage>> {
    let mut cache = storages().lock().unwrap();
    if let Some(s) = cache.get(folder) {
        return s.clone();
    }
    let mut found = None;
    let mut dir = Some(folder);
    for _ in 0..6 {
        let Some(d) = dir else { break };
        let marker = d.join(MARKER);
        if marker.is_file() {
            let parsed = std::fs::read_to_string(&marker)
                .ok()
                .and_then(|t| serde_json::from_str::<Marker>(&t).ok())
                .unwrap_or_else(|| {
                    log::error!("{}: unreadable, using the defaults", marker.display());
                    Marker::default()
                });
            found = Some(Arc::new(Storage {
                root: d.to_owned(),
                marker: parsed,
            }));
            break;
        }
        if d.join("level.dat").is_file() {
            break;
        }
        dir = d.parent();
    }
    if found.is_some() && !cfg!(test) {
        // Chunks are written as zstd from here on, the hot tier's own codec (a chunk of a vanilla
        // world is turned back into zlib on its way into a `.mca`).
        set_horizon_format(true);
    }
    cache.insert(folder.to_owned(), found.clone());
    found
}

/// Whether any `.mca` file lies under `dir` (a few folders deep, as a world's dimensions do).
fn holds_anvil(dir: &Path, depth: u32) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|e| {
        let path = e.path();
        if path.is_dir() {
            depth > 0 && holds_anvil(&path, depth - 1)
        } else {
            path.extension().is_some_and(|x| x == "mca")
        }
    })
}

/// Puts a world the server is about to create into this storage: writes [`MARKER`] with the
/// defaults when `root` has neither a marker nor any `.mca` file (so nothing is converted; a
/// world that already has chunks keeps its format). True when the marker was written.
pub fn adopt_new_world(root: &Path) -> io::Result<bool> {
    if root.join(MARKER).exists() || holds_anvil(root, 4) {
        return Ok(false);
    }
    std::fs::create_dir_all(root)?;
    let text = serde_json::to_string_pretty(&Marker::default()).map_err(|e| invalid(e.to_string()))?;
    std::fs::write(root.join(MARKER), text)?;
    forget_storages();
    Ok(true)
}

/// Forget the cached answers of [`storage_for`] (a world converted while the process runs).
pub fn forget_storages() {
    storages().lock().unwrap().clear();
    folders().lock().unwrap().clear();
}

/// One region's hot file.
pub struct HotSlot {
    path: PathBuf,
    position: ChunkPos,
    region: Mutex<Option<HzrRegion>>,
    last_write: AtomicU64,
    /// Whether the hot file exists (known without asking the file system on every read).
    exists: AtomicBool,
}

impl HotSlot {
    fn new(path: PathBuf, position: ChunkPos, last_write: u64) -> Self {
        let exists = path.is_file();
        Self {
            path,
            position,
            region: Mutex::new(None),
            last_write: AtomicU64::new(last_write),
            exists: AtomicBool::new(exists),
        }
    }

    /// The open region, opening the file when it exists (or creating it with `create`).
    fn region<'a>(
        &self,
        guard: &'a mut Option<HzrRegion>,
        create: bool,
    ) -> io::Result<Option<&'a mut HzrRegion>> {
        if guard.is_none() && (create || self.exists.load(Ordering::Relaxed)) {
            *guard = Some(HzrRegion::open_writable(&self.path, create)?);
            self.exists.store(true, Ordering::Relaxed);
        }
        Ok(guard.as_mut())
    }
}

/// One folder (`region`, `entities` or `poi` of a dimension).
pub struct Folder {
    path: PathBuf,
    storage: Arc<Storage>,
    archive: Mutex<Option<ColdArchive>>,
    slots: Mutex<HashMap<(i32, i32), Arc<HotSlot>>>,
}

/// The folder at `path`, with the hot files already in it.
pub fn folder(path: &Path, storage: Arc<Storage>) -> Arc<Folder> {
    let mut all = folders().lock().unwrap();
    if let Some(f) = all.get(path) {
        return f.clone();
    }
    let folder = Arc::new(Folder {
        path: path.to_owned(),
        storage,
        archive: Mutex::new(None),
        slots: Mutex::new(HashMap::new()),
    });
    if let Ok(entries) = std::fs::read_dir(path) {
        let mut slots = folder.slots.lock().unwrap();
        for e in entries.flatten() {
            let p = e.path();
            if let Ok(pos) = crate::hzr::position_from_path(&p) {
                let modified = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs());
                slots.insert((pos.x, pos.z), Arc::new(HotSlot::new(p, pos, modified)));
            }
        }
    }
    all.insert(path.to_owned(), folder.clone());
    start_settler();
    folder
}

impl Folder {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Run `f` on the archive, opened (or created) on first use.
    pub fn with_archive<R>(
        &self,
        f: impl FnOnce(&mut ColdArchive) -> io::Result<R>,
    ) -> io::Result<R> {
        let mut guard = self.archive.lock().unwrap();
        if guard.is_none() {
            *guard = Some(ColdArchive::open(self.path.join(ARCHIVE), true)?);
        }
        f(guard.as_mut().unwrap())
    }

    fn slot(&self, region: ChunkPos) -> Arc<HotSlot> {
        self.slots
            .lock()
            .unwrap()
            .entry((region.x, region.z))
            .or_insert_with(|| {
                Arc::new(HotSlot::new(
                    self.path.join(format!("r.{}.{}.hzr", region.x, region.z)),
                    region,
                    now(),
                ))
            })
            .clone()
    }

    /// Settle every region nobody wrote for the world's `settle_after_seconds` (all with `force`).
    /// The regions settled.
    pub fn settle_idle(&self, force: bool) -> io::Result<usize> {
        let after = self.storage.marker.settle_after_seconds;
        let slots: Vec<Arc<HotSlot>> = self.slots.lock().unwrap().values().cloned().collect();
        let mut settled = 0;
        for slot in slots {
            let idle = now().saturating_sub(slot.last_write.load(Ordering::Relaxed));
            if (force || idle >= after) && self.settle(&slot)? {
                settled += 1;
            }
        }
        Ok(settled)
    }

    /// Move a hot region into the archive. False when there was nothing to move, or a write came
    /// in meanwhile (the region stays hot then).
    fn settle(&self, slot: &HotSlot) -> io::Result<bool> {
        // 1. What the hot file holds.
        let (records, deleted, generation) = {
            let mut guard = slot.region.lock().unwrap();
            let Some(region) = slot.region(&mut guard, false)? else {
                self.slots
                    .lock()
                    .unwrap()
                    .remove(&(slot.position.x, slot.position.z));
                return Ok(false);
            };
            let present: Vec<ChunkPos> = region.chunks().map(|(p, _)| p).collect();
            let mut records = Vec::with_capacity(present.len());
            for pos in present {
                if let Some(r) = region.read(pos)? {
                    records.push((pos, r));
                }
            }
            let deleted: Vec<ChunkPos> = region.deleted().collect();
            (records, deleted, region.generation())
        };
        // 2. Recompressed with the archive's dictionary, outside every lock.
        let mut raws = Vec::with_capacity(records.len());
        for (pos, r) in &records {
            raws.push((
                *pos,
                r.compression.decode(&r.compressed, 64 * 1024 * 1024)?,
                r,
            ));
        }
        let mut dict = self.with_archive(|a| Ok(a.dictionary()))?;
        let mut new_dict = None;
        if dict.is_none() {
            let samples: Vec<Vec<u8>> = raws
                .iter()
                .take(DICT_SAMPLES)
                .map(|(_, raw, _)| raw.clone())
                .collect();
            if let Some(d) = train_dictionary(&samples) {
                dict = Some(Arc::new(d.clone()));
                new_dict = Some(d);
            }
        }
        let mut prepared = Vec::with_capacity(raws.len());
        for (pos, raw, r) in &raws {
            let hot = (r.compressed.len() as u32, xxh32(&r.compressed, 0));
            prepared.push(Prepared::new(
                *pos,
                raw,
                dict.as_deref().map(|d| d.as_slice()),
                r.timestamp,
                hot,
            )?);
        }
        // 3. One commit.
        self.with_archive(|a| {
            if let Some(d) = new_dict {
                a.set_dictionary(d);
            }
            a.commit(&prepared, &deleted)?;
            if a.wants_compaction() {
                log::info!("{}: compacting", a.path().display());
                a.compact()?;
            }
            Ok(())
        })?;
        // 4. The hot file goes, unless it was written since.
        let mut guard = slot.region.lock().unwrap();
        if guard.as_ref().is_some_and(|r| r.generation() != generation) {
            return Ok(false);
        }
        *guard = None;
        slot.exists.store(false, Ordering::Relaxed);
        match std::fs::remove_file(&slot.path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        Ok(true)
    }
}

fn start_settler() {
    static START: Once = Once::new();
    START.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("hzr-settler".into())
            .spawn(|| {
                loop {
                    std::thread::sleep(Duration::from_secs(15));
                    let all: Vec<Arc<Folder>> =
                        folders().lock().unwrap().values().cloned().collect();
                    for f in all {
                        match f.settle_idle(false) {
                            Ok(0) => {}
                            Ok(n) => log::debug!(
                                "{}: settled {n} region(s) into the archive",
                                f.path.display()
                            ),
                            Err(e) => log::error!("{}: settling: {e}", f.path.display()),
                        }
                    }
                }
            });
        if let Err(e) = spawned {
            log::error!("hzr: cannot start the settler, regions stay hot: {e}");
        }
    });
}

/// A region of a world in this storage, as [`crate::region::RegionFile`] uses it.
pub struct TieredRegion {
    folder: Arc<Folder>,
    slot: Arc<HotSlot>,
    position: ChunkPos,
}

impl TieredRegion {
    /// The region `position` of the folder at `dir`. Without `create`, NotFound when neither tier
    /// has any of its chunks.
    pub fn open(
        dir: &Path,
        position: ChunkPos,
        storage: Arc<Storage>,
        create: bool,
    ) -> io::Result<Self> {
        let folder = folder(dir, storage);
        let slot = folder.slot(position);
        if !create
            && !slot.exists.load(Ordering::Relaxed)
            && !folder.with_archive(|a| Ok(a.has_region(position)))?
        {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        Ok(Self {
            folder,
            slot,
            position,
        })
    }

    pub fn position(&self) -> ChunkPos {
        self.position
    }

    pub fn read(&mut self, chunk: ChunkPos) -> io::Result<Option<ChunkRecord>> {
        {
            let mut guard = self.slot.region.lock().unwrap();
            if let Some(region) = self.slot.region(&mut guard, false)? {
                match region.slot_of(chunk)? {
                    Slot::Present { .. } => return region.read(chunk),
                    Slot::Deleted => return Ok(None),
                    Slot::Absent => {}
                }
            }
        }
        self.folder.with_archive(|a| a.read(chunk))
    }

    /// Write a chunk to the hot tier, unless its bytes are the ones already stored.
    pub fn write(&mut self, chunk: ChunkPos, record: &ChunkRecord) -> io::Result<()> {
        if chunk.region() != self.position {
            return Err(invalid("chunk does not belong to this region"));
        }
        let record = to_zstd(record)?;
        let checksum = xxh32(&record.compressed, 0);
        let length = record.compressed.len() as u32;
        let mut guard = self.slot.region.lock().unwrap();
        let state = match self.slot.region(&mut guard, false)? {
            Some(r) => r.slot_of(chunk)?,
            None => Slot::Absent,
        };
        let unchanged = match state {
            Slot::Present {
                length: l,
                checksum: c,
            } => l == length && c == checksum,
            Slot::Deleted => false,
            Slot::Absent => self
                .folder
                .with_archive(|a| Ok(a.get(chunk)))?
                .is_some_and(|e| e.hot_len == length && e.hot_sum == checksum),
        };
        if unchanged {
            return Ok(());
        }
        let region = self.slot.region(&mut guard, true)?.unwrap();
        region.write(chunk, &record)?;
        self.slot.last_write.store(now(), Ordering::Relaxed);
        Ok(())
    }

    /// Delete a chunk. Whether either tier held it.
    pub fn clear(&mut self, chunk: ChunkPos, timestamp: u32) -> io::Result<bool> {
        let in_archive = self.folder.with_archive(|a| Ok(a.get(chunk).is_some()))?;
        let mut guard = self.slot.region.lock().unwrap();
        let state = match self.slot.region(&mut guard, false)? {
            Some(r) => r.slot_of(chunk)?,
            None => Slot::Absent,
        };
        let held = matches!(state, Slot::Present { .. }) || (in_archive && state != Slot::Deleted);
        if !held {
            return Ok(false);
        }
        let region = self.slot.region(&mut guard, true)?.unwrap();
        if in_archive {
            region.delete(chunk, timestamp)?;
        } else {
            region.clear(chunk, timestamp)?;
        }
        self.slot.last_write.store(now(), Ordering::Relaxed);
        Ok(true)
    }

    pub fn sync(&self) -> io::Result<()> {
        match &*self.slot.region.lock().unwrap() {
            Some(r) => r.sync(),
            None => Ok(()),
        }
    }

    /// The region's chunks in either tier, with their timestamps.
    pub fn chunks(&self) -> Vec<(ChunkPos, u32)> {
        let mut out: HashMap<(i32, i32), u32> = HashMap::new();
        let mut hidden = Vec::new();
        {
            let mut guard = self.slot.region.lock().unwrap();
            if let Ok(Some(r)) = self.slot.region(&mut guard, false) {
                for (p, t) in r.chunks() {
                    out.insert((p.x, p.z), t);
                }
                hidden.extend(r.deleted());
            }
        }
        if let Ok(cold) = self.folder.with_archive(|a| Ok(a.chunks_in(self.position))) {
            for (p, t) in cold {
                if !hidden.contains(&p) {
                    out.entry((p.x, p.z)).or_insert(t);
                }
            }
        }
        let mut v: Vec<_> = out
            .into_iter()
            .map(|((x, z), t)| (ChunkPos::new(x, z), t))
            .collect();
        v.sort_by_key(|(p, _)| (p.z, p.x));
        v
    }
}

/// Settle every hot region of every folder of `world` now (tests, the conversion tool).
pub fn settle_world_now(world: &Path) -> io::Result<usize> {
    let all: Vec<Arc<Folder>> = folders()
        .lock()
        .unwrap()
        .values()
        .filter(|f| f.path.starts_with(world))
        .cloned()
        .collect();
    let mut n = 0;
    for f in all {
        n += f.settle_idle(true)?;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compression::Compression;
    use crate::region::RegionFile;

    fn world(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("horizon-tiered-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("region")).unwrap();
        std::fs::write(d.join("level.dat"), b"").unwrap();
        std::fs::write(
            d.join(MARKER),
            serde_json::to_string(&Marker::default()).unwrap(),
        )
        .unwrap();
        d
    }

    fn chunk(x: i32, z: i32, n: usize) -> ChunkRecord {
        let raw: Vec<u8> = (0..n)
            .map(|i| ((i as i32 * (x + 3) + z) % 97) as u8)
            .collect();
        ChunkRecord {
            compression: Compression::Zlib,
            compressed: Compression::Zlib.encode(&raw).unwrap(),
            timestamp: 40 + x as u32,
        }
    }

    fn raw_of(r: &ChunkRecord) -> Vec<u8> {
        r.compression.decode(&r.compressed, 1 << 24).unwrap()
    }

    #[test]
    fn writes_go_hot_settle_cold_and_deletions_hide_cold_copies() {
        let w = world("tiers");
        let path = w.join("region/r.0.0.mca");
        assert!(RegionFile::open(&path).is_err(), "nothing yet");
        let mut r = RegionFile::open_writable(&path, true).unwrap();
        for i in 0..40 {
            r.write(ChunkPos::new(i % 32, i / 32), &chunk(i % 32, i / 32, 6000))
                .unwrap();
        }
        r.sync().unwrap();
        assert!(w.join("region/r.0.0.hzr").is_file());
        drop(r);
        // Settle: the hot file goes, the archive holds the chunks.
        assert_eq!(settle_world_now(&w).unwrap(), 1);
        assert!(!w.join("region/r.0.0.hzr").exists());
        let mut r = RegionFile::open(&path).unwrap();
        assert_eq!(r.chunks().count(), 40);
        assert_eq!(
            raw_of(&r.read(ChunkPos::new(5, 0)).unwrap().unwrap()),
            raw_of(&chunk(5, 0, 6000))
        );
        assert_eq!(r.read(ChunkPos::new(5, 0)).unwrap().unwrap().timestamp, 45);
        // Writing the same bytes changes nothing: no hot file.
        let mut r = RegionFile::open_writable(&path, false).unwrap();
        r.write(ChunkPos::new(5, 0), &chunk(5, 0, 6000)).unwrap();
        assert!(
            !w.join("region/r.0.0.hzr").exists(),
            "an unchanged chunk is not written"
        );
        // A change and a deletion go hot; the deleted cold copy is hidden.
        r.write(ChunkPos::new(6, 0), &chunk(6, 0, 7000)).unwrap();
        assert!(r.clear(ChunkPos::new(7, 0), 99).unwrap());
        assert!(!r.clear(ChunkPos::new(7, 0), 99).unwrap(), "already gone");
        assert!(r.read(ChunkPos::new(7, 0)).unwrap().is_none());
        assert_eq!(
            raw_of(&r.read(ChunkPos::new(6, 0)).unwrap().unwrap()),
            raw_of(&chunk(6, 0, 7000))
        );
        assert_eq!(r.chunks().count(), 39);
        drop(r);
        assert_eq!(settle_world_now(&w).unwrap(), 1);
        let mut r = RegionFile::open(&path).unwrap();
        assert!(r.read(ChunkPos::new(7, 0)).unwrap().is_none());
        assert_eq!(
            raw_of(&r.read(ChunkPos::new(6, 0)).unwrap().unwrap()),
            raw_of(&chunk(6, 0, 7000))
        );
        assert_eq!(r.chunks().count(), 39);
        let _ = std::fs::remove_dir_all(&w);
    }

    #[test]
    fn a_world_without_the_marker_stays_vanilla() {
        let d = std::env::temp_dir().join(format!("horizon-tiered-vanilla-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("region")).unwrap();
        std::fs::write(d.join("level.dat"), b"").unwrap();
        let path = d.join("region/r.0.0.mca");
        let mut r = RegionFile::open_writable(&path, true).unwrap();
        r.write(ChunkPos::new(1, 1), &chunk(1, 1, 3000)).unwrap();
        r.sync().unwrap();
        assert!(!r.is_hzr());
        assert!(path.is_file());
        assert!(!d.join("region/r.0.0.hzr").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn new_worlds_are_adopted_but_worlds_with_anvil_chunks_are_not() {
        let base = std::env::temp_dir().join(format!("horizon-tiered-adopt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        // A brand-new world: the marker is written and its chunks go to the hot tier.
        let fresh = base.join("fresh");
        assert!(adopt_new_world(&fresh).unwrap());
        assert!(!adopt_new_world(&fresh).unwrap(), "only once");
        std::fs::write(fresh.join("level.dat"), b"").unwrap();
        std::fs::create_dir_all(fresh.join("region")).unwrap();
        let path = fresh.join("region/r.0.0.mca");
        let mut r = RegionFile::open_writable(&path, true).unwrap();
        r.write(ChunkPos::new(2, 3), &chunk(2, 3, 3000)).unwrap();
        r.sync().unwrap();
        assert!(r.is_hzr());
        assert!(fresh.join("region/r.0.0.hzr").is_file());
        assert!(!path.exists());
        // Region files without a level.dat (copied in by hand) keep the vanilla format.
        let copied = base.join("copied");
        std::fs::create_dir_all(copied.join("DIM-1/region")).unwrap();
        std::fs::write(copied.join("DIM-1/region/r.0.0.mca"), b"").unwrap();
        assert!(!adopt_new_world(&copied).unwrap());
        assert!(!copied.join(MARKER).exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
