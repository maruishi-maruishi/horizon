//! The conversion tool's side of Horizon's storage (docs/horizon-storage.md): a whole world from
//! vanilla's `.mca` regions to the two tiers and back, a check of every chunk, and sizes. Run it on
//! a world no game has open; the game itself never converts.
use crate::archive::{ColdArchive, Prepared, train_dictionary};
use crate::compression::Compression;
use crate::hzr::{HzrRegion, Slot};
use crate::nbt::Limits;
use crate::region::{AnvilRegion, ChunkRecord, RegionWriter, anvil_bytes, remove_anvil};
use crate::tiered::{ARCHIVE, MARKER, Marker};
use horizon_core::position::ChunkPos;
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};
use xxhash_rust::xxh3::xxh3_64;
use xxhash_rust::xxh32::xxh32;

/// Which way to convert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Hzr,
    Mca,
}

/// What a conversion did.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub folders: usize,
    pub chunks: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

const FOLDERS: [&str; 3] = ["region", "entities", "poi"];
const DICT_SAMPLES: usize = 2000;
const MAX_RAW: usize = 64 * 1024 * 1024;

/// The `region`, `entities` and `poi` folders of every dimension of the world at `root`.
pub fn chunk_folders(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    walk(root, &mut found, 0)?;
    found.sort();
    Ok(found)
}

fn walk(dir: &Path, found: &mut Vec<PathBuf>, depth: usize) -> io::Result<()> {
    if depth > 6 {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path();
        if FOLDERS.iter().any(|f| entry.file_name() == *f) {
            found.push(path);
        } else {
            walk(&path, found, depth + 1)?;
        }
    }
    Ok(())
}

fn files_with(dir: &Path, extension: &str) -> io::Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension().is_some_and(|e| e == extension)
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("r."))
        })
        .collect();
    out.sort();
    Ok(out)
}

fn raw_of(record: &ChunkRecord) -> io::Result<Vec<u8>> {
    record.compression.decode(&record.compressed, MAX_RAW)
}

/// Whether the world at `root` is in Horizon's storage.
pub fn is_hzr_world(root: &Path) -> bool {
    root.join(MARKER).is_file()
}

/// Convert the world at `root` to `target`. Everything is written and read back before the old
/// files go; a failure leaves the world as it was.
pub fn convert_world(
    root: &Path,
    target: Target,
    mut progress: impl FnMut(&str),
) -> io::Result<Report> {
    if !root.join("level.dat").is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{}: no level.dat: not a world", root.display()),
        ));
    }
    match (target, is_hzr_world(root)) {
        (Target::Hzr, true) => return Err(io::Error::other("the world is already in hzr")),
        (Target::Mca, false) => return Err(io::Error::other("the world is already in mca")),
        _ => {}
    }
    let mut report = Report::default();
    let folders = chunk_folders(root)?;
    match target {
        Target::Hzr => {
            for folder in &folders {
                if folder.join(ARCHIVE).exists() || !files_with(folder, "hzr")?.is_empty() {
                    return Err(io::Error::other(format!(
                        "{}: hzr files from an earlier attempt; remove them first",
                        folder.display()
                    )));
                }
            }
            for folder in &folders {
                progress(&folder.display().to_string());
                to_hzr(folder, &mut report)?;
            }
            fs::write(
                root.join(MARKER),
                serde_json::to_string_pretty(&Marker::default()).unwrap(),
            )?;
            for folder in &folders {
                for mca in files_with(folder, "mca")? {
                    remove_anvil(&mca)?;
                }
            }
        }
        Target::Mca => {
            let mut written = Vec::new();
            for folder in &folders {
                progress(&folder.display().to_string());
                written.push((folder.clone(), to_mca(folder, &mut report)?));
            }
            fs::remove_file(root.join(MARKER))?;
            for (folder, _) in &written {
                for hzr in files_with(folder, "hzr")? {
                    fs::remove_file(hzr)?;
                }
                let archive = folder.join(ARCHIVE);
                if archive.exists() {
                    fs::remove_file(archive)?;
                }
            }
        }
    }
    Ok(report)
}

fn to_hzr(folder: &Path, report: &mut Report) -> io::Result<()> {
    let regions = files_with(folder, "mca")?;
    if regions.is_empty() {
        return Ok(());
    }
    report.folders += 1;
    // A dictionary from the first chunks.
    let mut samples = Vec::new();
    'sampling: for mca in &regions {
        let mut source = AnvilRegion::open(mca)?;
        let present: Vec<ChunkPos> = source.chunks().map(|(c, _)| c).collect();
        for chunk in present {
            if let Some(r) = source.read(chunk)? {
                samples.push(raw_of(&r)?);
                if samples.len() >= DICT_SAMPLES {
                    break 'sampling;
                }
            }
        }
    }
    let dict = train_dictionary(&samples);
    drop(samples);
    let path = folder.join(ARCHIVE);
    let mut archive = ColdArchive::open(&path, true)?;
    if let Some(d) = &dict {
        archive.set_dictionary(d.clone());
    }
    let mut expected: BTreeMap<(i32, i32), u64> = BTreeMap::new();
    for mca in &regions {
        report.bytes_before += anvil_bytes(mca)?;
        let mut source = AnvilRegion::open(mca)?;
        let present: Vec<ChunkPos> = source.chunks().map(|(c, _)| c).collect();
        let mut batch = Vec::with_capacity(present.len());
        for chunk in present {
            let Some(r) = source.read(chunk)? else {
                continue;
            };
            let raw = raw_of(&r).map_err(|e| {
                io::Error::other(format!(
                    "{} chunk {} {}: {e}",
                    mca.display(),
                    chunk.x,
                    chunk.z
                ))
            })?;
            let hot = Compression::Zstd.encode(&raw)?;
            batch.push(Prepared::new(
                chunk,
                &raw,
                dict.as_deref(),
                r.timestamp,
                (hot.len() as u32, xxh32(&hot, 0)),
            )?);
            expected.insert((chunk.x, chunk.z), xxh3_64(&raw));
        }
        report.chunks += batch.len();
        archive.commit(&batch, &[])?;
    }
    drop(archive);
    // Read everything back.
    let mut archive = ColdArchive::open(&path, false)?;
    if archive.len() != expected.len() {
        return Err(io::Error::other(format!(
            "{}: {} chunks written, {} found",
            path.display(),
            expected.len(),
            archive.len()
        )));
    }
    for ((x, z), hash) in &expected {
        let got = archive
            .read(ChunkPos::new(*x, *z))?
            .ok_or_else(|| io::Error::other("a converted chunk is missing"))?;
        if xxh3_64(&got.compressed) != *hash {
            return Err(io::Error::other(format!(
                "{}: chunk {x} {z} differs after conversion",
                path.display()
            )));
        }
    }
    report.bytes_after += fs::metadata(&path)?.len();
    Ok(())
}

/// Every chunk of a folder in Horizon's storage, region by region: hot over cold.
fn tiered_chunks(folder: &Path) -> io::Result<BTreeMap<(i32, i32), Vec<(ChunkPos, ChunkRecord)>>> {
    let mut out: BTreeMap<(i32, i32), BTreeMap<(i32, i32), ChunkRecord>> = BTreeMap::new();
    let archive_path = folder.join(ARCHIVE);
    if archive_path.exists() {
        let mut archive = ColdArchive::open(&archive_path, false)?;
        for pos in archive.all() {
            if let Some(r) = archive.read(pos)? {
                out.entry((pos.x >> 5, pos.z >> 5))
                    .or_default()
                    .insert((pos.x, pos.z), r);
            }
        }
    }
    for hzr in files_with(folder, "hzr")? {
        let mut region = HzrRegion::open(&hzr)?;
        let pos = region.position();
        let slots = out.entry((pos.x, pos.z)).or_default();
        for chunk in region.deleted().collect::<Vec<_>>() {
            slots.remove(&(chunk.x, chunk.z));
        }
        for (chunk, _) in region.chunks().collect::<Vec<_>>() {
            if let Some(r) = region.read(chunk)? {
                slots.insert((chunk.x, chunk.z), r);
            }
        }
    }
    Ok(out
        .into_iter()
        .map(|(k, v)| {
            (
                k,
                v.into_iter()
                    .map(|((x, z), r)| (ChunkPos::new(x, z), r))
                    .collect(),
            )
        })
        .filter(|(_, v): &(_, Vec<_>)| !v.is_empty())
        .collect())
}

fn to_mca(folder: &Path, report: &mut Report) -> io::Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    let mut before = fs::metadata(folder.join(ARCHIVE)).map_or(0, |m| m.len());
    for hzr in files_with(folder, "hzr")? {
        before += fs::metadata(hzr)?.len();
    }
    let regions = tiered_chunks(folder)?;
    if regions.is_empty() {
        return Ok(written);
    }
    report.folders += 1;
    report.bytes_before += before;
    for ((rx, rz), chunks) in regions {
        let mca = folder.join(format!("r.{rx}.{rz}.mca"));
        if mca.exists() {
            remove_anvil(&mca)?;
        }
        let mut writer = RegionWriter::create(&mca)?;
        let mut expected = Vec::with_capacity(chunks.len());
        for (pos, record) in &chunks {
            let raw = raw_of(record)?;
            let zlib = ChunkRecord {
                compression: Compression::Zlib,
                compressed: Compression::Zlib.encode(&raw)?,
                timestamp: record.timestamp,
            };
            writer.write(*pos, &zlib)?;
            expected.push((*pos, xxh3_64(&raw)));
        }
        writer.finish()?;
        let mut back = AnvilRegion::open(&mca)?;
        for (pos, hash) in expected {
            let got = back
                .read(pos)?
                .ok_or_else(|| io::Error::other("a converted chunk is missing"))?;
            if xxh3_64(&raw_of(&got)?) != hash {
                return Err(io::Error::other(format!(
                    "{}: chunk {} {} differs after conversion",
                    mca.display(),
                    pos.x,
                    pos.z
                )));
            }
        }
        report.chunks += chunks.len();
        report.bytes_after += anvil_bytes(&mca)?;
        written.push(mca);
    }
    Ok(written)
}

/// What [`verify_world`] found.
#[derive(Clone, Debug, Default)]
pub struct Health {
    pub files: usize,
    pub chunks: usize,
    /// Damaged blocks found, and how many parity rebuilt (on disk with `repair`).
    pub damaged_blocks: u64,
    pub repaired_blocks: u64,
    /// Chunks that can't be read or aren't NBT: (file, chunk, why).
    pub bad: Vec<(PathBuf, ChunkPos, String)>,
}

/// Read and parse every chunk of the world at `root` (either format). With `repair`, archive
/// blocks parity can rebuild are written back, and damaged `.hzr` indexes rebuilt.
pub fn verify_world(
    root: &Path,
    repair: bool,
    mut progress: impl FnMut(&str),
) -> io::Result<Health> {
    let mut health = Health::default();
    let check = |health: &mut Health,
                 file: &Path,
                 pos: ChunkPos,
                 record: io::Result<Option<ChunkRecord>>| {
        health.chunks += 1;
        let result = record.and_then(|r| match r {
            Some(r) => r.nbt(Limits::default()).map(|_| ()),
            None => Err(io::Error::other("listed but not readable")),
        });
        if let Err(e) = result {
            health.bad.push((file.to_owned(), pos, e.to_string()));
        }
    };
    for folder in chunk_folders(root)? {
        progress(&folder.display().to_string());
        for mca in files_with(&folder, "mca")? {
            health.files += 1;
            match AnvilRegion::open(&mca) {
                Ok(mut r) => {
                    let present: Vec<ChunkPos> = r.chunks().map(|(c, _)| c).collect();
                    for pos in present {
                        let got = r.read(pos);
                        check(&mut health, &mca, pos, got);
                    }
                }
                Err(e) => health
                    .bad
                    .push((mca.clone(), ChunkPos::new(0, 0), e.to_string())),
            }
        }
        for hzr in files_with(&folder, "hzr")? {
            health.files += 1;
            let opened = if repair {
                HzrRegion::open_writable(&hzr, false)
            } else {
                HzrRegion::open(&hzr)
            };
            match opened {
                Ok(mut r) => {
                    let present: Vec<ChunkPos> = r.chunks().map(|(c, _)| c).collect();
                    for pos in present {
                        if matches!(r.slot_of(pos), Ok(Slot::Present { .. })) {
                            let got = r.read(pos);
                            check(&mut health, &hzr, pos, got);
                        }
                    }
                }
                Err(e) => health
                    .bad
                    .push((hzr.clone(), ChunkPos::new(0, 0), e.to_string())),
            }
        }
        let archive_path = folder.join(ARCHIVE);
        if archive_path.exists() {
            health.files += 1;
            match ColdArchive::open(&archive_path, repair) {
                Ok(mut a) => {
                    let v = a.verify(repair);
                    health.damaged_blocks += v.damaged_blocks;
                    health.repaired_blocks += v.repaired_blocks;
                    for pos in a.all() {
                        let got = a.read(pos);
                        check(&mut health, &archive_path, pos, got);
                    }
                }
                Err(e) => {
                    health
                        .bad
                        .push((archive_path.clone(), ChunkPos::new(0, 0), e.to_string()))
                }
            }
        }
    }
    Ok(health)
}

/// Read and parse every chunk of the world at `root` through [`crate::region::RegionFile`], as
/// the game loads them. The chunks read.
pub fn read_all(root: &Path) -> io::Result<usize> {
    let mut n = 0;
    for folder in chunk_folders(root)? {
        let mut regions = std::collections::BTreeSet::new();
        for f in files_with(&folder, "mca")?
            .into_iter()
            .chain(files_with(&folder, "hzr")?)
        {
            let name = f.file_name().unwrap().to_string_lossy().into_owned();
            let parts: Vec<&str> = name.split('.').collect();
            if let (Ok(x), Ok(z)) = (parts[1].parse::<i32>(), parts[2].parse::<i32>()) {
                regions.insert((x, z));
            }
        }
        let archive = folder.join(ARCHIVE);
        if archive.exists() {
            for pos in ColdArchive::open(&archive, false)?.all() {
                regions.insert((pos.x >> 5, pos.z >> 5));
            }
        }
        for (x, z) in regions {
            let mut region =
                crate::region::RegionFile::open(folder.join(format!("r.{x}.{z}.mca")))?;
            let present: Vec<ChunkPos> = region.chunks().map(|(c, _)| c).collect();
            for pos in present {
                if let Some(r) = region.read(pos)? {
                    r.nbt(Limits::default())?;
                    n += 1;
                }
            }
        }
    }
    Ok(n)
}

/// Write every chunk of the world at `source` into a new world at `dest` through
/// [`crate::region::RegionFile`], as the game saves them: zlib into `.mca`, or zstd into the hot
/// tier with `hzr`. Every chunk is read into memory first; the time is the writing alone, synced
/// as the game syncs each region. (chunks, seconds, bytes)
pub fn write_all(source: &Path, dest: &Path, hzr: bool) -> io::Result<(usize, f64, u64)> {
    let mut chunks: Vec<(PathBuf, ChunkPos, Vec<u8>, u32)> = Vec::new();
    for folder in chunk_folders(source)? {
        let relative = folder.strip_prefix(source).unwrap().to_owned();
        for mca in files_with(&folder, "mca")? {
            let mut r = AnvilRegion::open(&mca)?;
            let present: Vec<ChunkPos> = r.chunks().map(|(c, _)| c).collect();
            for pos in present {
                if let Some(rec) = r.read(pos)? {
                    chunks.push((relative.clone(), pos, raw_of(&rec)?, rec.timestamp));
                }
            }
        }
    }
    fs::create_dir_all(dest)?;
    fs::write(dest.join("level.dat"), b"")?;
    if hzr {
        fs::write(
            dest.join(MARKER),
            serde_json::to_string(&Marker::default()).unwrap(),
        )?;
    }
    let codec = if hzr {
        Compression::Zstd
    } else {
        Compression::Zlib
    };
    let started = std::time::Instant::now();
    let mut by_region: BTreeMap<(PathBuf, i32, i32), Vec<usize>> = BTreeMap::new();
    for (i, (rel, pos, _, _)) in chunks.iter().enumerate() {
        by_region
            .entry((rel.clone(), pos.x >> 5, pos.z >> 5))
            .or_default()
            .push(i);
    }
    for ((rel, x, z), list) in by_region {
        let dir = dest.join(&rel);
        fs::create_dir_all(&dir)?;
        let mut region =
            crate::region::RegionFile::open_writable(dir.join(format!("r.{x}.{z}.mca")), true)?;
        for i in list {
            let (_, pos, raw, ts) = &chunks[i];
            let record = ChunkRecord {
                compression: codec,
                compressed: codec.encode(raw)?,
                timestamp: *ts,
            };
            region.write(*pos, &record)?;
        }
        region.sync()?;
    }
    let secs = started.elapsed().as_secs_f64();
    let (mca, hot, cold) = world_size(dest)?;
    Ok((chunks.len(), secs, mca + hot + cold))
}

/// The chunk bytes of the world at `root`: (`.mca` with `.mcc`, hot `.hzr`, cold `.hza`).
pub fn world_size(root: &Path) -> io::Result<(u64, u64, u64)> {
    let (mut mca, mut hot, mut cold) = (0, 0, 0);
    for folder in chunk_folders(root)? {
        for f in files_with(&folder, "mca")? {
            mca += anvil_bytes(&f)?;
        }
        for f in files_with(&folder, "hzr")? {
            hot += fs::metadata(f)?.len();
        }
        if let Ok(m) = fs::metadata(folder.join(ARCHIVE)) {
            cold += m.len();
        }
    }
    Ok((mca, hot, cold))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(x: i32, z: i32) -> Vec<u8> {
        // A real NBT compound, so verify's parse passes.
        use crate::nbt::{Compound, NamedTag, Tag};
        let mut root = Compound::new();
        root.insert("xPos".into(), Tag::Int(x));
        root.insert("zPos".into(), Tag::Int(z));
        root.insert("Status".into(), Tag::String("minecraft:full".into()));
        root.insert(
            "Data".into(),
            Tag::ByteArray((0..4000).map(|i| ((i * (x + 5) + z) % 13) as u8).collect()),
        );
        let named = NamedTag {
            name: "".into(),
            tag: Tag::Compound(root),
        };
        crate::nbt::write_named(&named, Limits::default()).unwrap()
    }

    fn make_world(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("horizon-convert-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("level.dat"), b"").unwrap();
        for folder in [
            "region",
            "entities",
            "dimensions/minecraft/the_nether/region",
        ] {
            fs::create_dir_all(root.join(folder)).unwrap();
            let mut writer = RegionWriter::create(root.join(folder).join("r.0.-1.mca")).unwrap();
            for i in 0..40 {
                let pos = ChunkPos::new(i % 32, -32 + i / 32);
                let r = ChunkRecord {
                    compression: Compression::Zlib,
                    compressed: Compression::Zlib.encode(&raw(pos.x, pos.z)).unwrap(),
                    timestamp: 1000 + i as u32,
                };
                writer.write(pos, &r).unwrap();
            }
            writer.finish().unwrap();
        }
        root
    }

    #[test]
    fn a_world_goes_to_hzr_and_back_with_the_same_chunks() {
        let root = make_world("roundtrip");
        let to = convert_world(&root, Target::Hzr, |_| {}).unwrap();
        assert_eq!((to.folders, to.chunks), (3, 120));
        assert!(is_hzr_world(&root));
        let (mca, hot, cold) = world_size(&root).unwrap();
        assert_eq!((mca, hot), (0, 0));
        assert!(cold > 0);
        let health = verify_world(&root, false, |_| {}).unwrap();
        assert_eq!((health.chunks, health.bad.len()), (120, 0));
        assert!(
            convert_world(&root, Target::Hzr, |_| {}).is_err(),
            "already hzr"
        );
        let back = convert_world(&root, Target::Mca, |_| {}).unwrap();
        assert_eq!(back.chunks, 120);
        assert!(!is_hzr_world(&root));
        let mut r =
            AnvilRegion::open(root.join("dimensions/minecraft/the_nether/region/r.0.-1.mca"))
                .unwrap();
        let got = r.read(ChunkPos::new(7, -32)).unwrap().unwrap();
        assert_eq!(got.compression, Compression::Zlib);
        assert_eq!(raw_of(&got).unwrap(), raw(7, -32));
        assert_eq!(got.timestamp, 1007);
        assert!(!root.join("region").join(ARCHIVE).exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn verify_finds_and_repairs_a_damaged_archive() {
        let root = make_world("verify");
        convert_world(&root, Target::Hzr, |_| {}).unwrap();
        let archive = root.join("region").join(ARCHIVE);
        let mut bytes = fs::read(&archive).unwrap();
        let at = 3 * crate::ecc::BLOCK + 77; // a data block of the first stripe
        bytes[at] ^= 0xff;
        fs::write(&archive, &bytes).unwrap();
        let found = verify_world(&root, false, |_| {}).unwrap();
        assert_eq!(
            (found.damaged_blocks, found.bad.len()),
            (1, 0),
            "rebuilt in memory, all chunks fine"
        );
        let fixed = verify_world(&root, true, |_| {}).unwrap();
        assert_eq!(fixed.repaired_blocks, 1);
        let again = verify_world(&root, false, |_| {}).unwrap();
        assert_eq!(again.damaged_blocks, 0);
        let _ = fs::remove_dir_all(&root);
    }
}
