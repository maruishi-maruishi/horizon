//! Horizon's own region format, `.hzr`: the same 32x32 chunks as an Anvil `.mca` file, packed
//! tighter and made to survive a crash.
//!
//! Against Anvil:
//!
//! - **Space.** A chunk takes 256-byte units, not 4096-byte sectors: Anvil pads every chunk to
//!   its next sector, about 2 KiB a chunk on average, a third or more of a small chunk's size.
//!   Chunks are zstd (level [`crate::compression::ZSTD_LEVEL`]): smaller than zlib and several times faster to
//!   decode. There are no `.mcc` side files: a chunk of any size stays in its region.
//! - **Crash safety.** A write lands in free space only, then the index points to it, then the
//!   old copy's space is freed: a crash at any point leaves the old or the new chunk, never a
//!   torn one. Every chunk carries its coordinates, a generation number and an xxh32 checksum.
//!   An index entry that points at anything else (a torn index write, a damaged sector) makes the
//!   file rebuild its index from the chunks themselves, newest generation first.
//!
//! Layout (little endian):
//!
//! ```text
//! 0      "HZR1"  version u32  generation u64
//! 16     1024 x { unit u32, length u32, timestamp u32, xxh32 u32 }
//!        (unit 0: no chunk; length 0: deleted, which hides the cold tier's copy)
//! 16400  padding to the first unit boundary (16640, unit 65)
//! ...    records, each at a unit boundary:
//!        "HZC1"  x i32  z i32  length u32  xxh32 u32  codec u8  0 0 0  generation u64  timestamp u32
//!        then `length` bytes of the chunk's compressed NBT
//! ```
use crate::compression::{Compression, invalid};
use crate::region::ChunkRecord;
use horizon_core::position::ChunkPos;
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
use xxhash_rust::xxh32::xxh32;

/// The allocation unit.
pub const UNIT: u64 = 256;
const MAGIC: &[u8; 4] = b"HZR1";
const RECORD_MAGIC: &[u8; 4] = b"HZC1";
const VERSION: u32 = 1;
const ENTRY: usize = 16;
/// The codec byte of a deletion record.
const DELETED: u8 = 0xFE;
const INDEX_START: usize = 16;
const HEADER_BYTES: usize = INDEX_START + 1024 * ENTRY;
/// The first unit a record may use.
const FIRST_UNIT: u32 = (HEADER_BYTES as u64).div_ceil(UNIT) as u32;
const RECORD_HEADER: usize = 36;
/// A chunk's compressed size limit, as Anvil's.
const MAX_COMPRESSED: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Entry {
    unit: u32,
    length: u32,
    timestamp: u32,
    checksum: u32,
}

/// What a region says of one of its chunks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    /// Nothing: ask the cold tier.
    Absent,
    /// Here, with its compressed length and xxh32.
    Present { length: u32, checksum: u32 },
    /// Deleted here, whatever the cold tier holds.
    Deleted,
}

impl Entry {
    fn slot(&self) -> Slot {
        match (self.unit, self.length) {
            (0, _) => Slot::Absent,
            (_, 0) => Slot::Deleted,
            (_, length) => Slot::Present {
                length,
                checksum: self.checksum,
            },
        }
    }

    fn units(&self) -> u32 {
        (RECORD_HEADER as u64 + u64::from(self.length)).div_ceil(UNIT) as u32
    }
}

/// An open `.hzr` region.
pub struct HzrRegion {
    file: File,
    path: PathBuf,
    position: ChunkPos,
    generation: u64,
    entries: [Entry; 1024],
    /// The units in use: the header's and every chunk's.
    used: Vec<bool>,
    first_free: usize,
    writable: bool,
}

fn slot(region: ChunkPos, chunk: ChunkPos) -> io::Result<usize> {
    if chunk.region() != region {
        return Err(invalid("chunk does not belong to this region"));
    }
    let (x, z) = chunk.region_local();
    Ok(usize::from(x) + usize::from(z) * 32)
}

fn position(region: ChunkPos, index: usize) -> ChunkPos {
    ChunkPos::new(
        region.x * 32 + (index % 32) as i32,
        region.z * 32 + (index / 32) as i32,
    )
}

/// `r.<x>.<z>.hzr`'s region coordinates.
pub fn position_from_path(path: &Path) -> io::Result<ChunkPos> {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| invalid("invalid region filename"))?;
    let parts: Vec<_> = name.split('.').collect();
    if parts.len() != 4 || parts[0] != "r" || parts[3] != "hzr" {
        return Err(invalid("expected r.<x>.<z>.hzr"));
    }
    let x: i32 = parts[1].parse().map_err(|_| invalid("invalid region x"))?;
    let z: i32 = parts[2].parse().map_err(|_| invalid("invalid region z"))?;
    Ok(ChunkPos::new(x, z))
}

struct RecordHeader {
    x: i32,
    z: i32,
    length: u32,
    checksum: u32,
    codec: u8,
    generation: u64,
    timestamp: u32,
}

impl RecordHeader {
    fn encode(&self) -> [u8; RECORD_HEADER] {
        let mut b = [0; RECORD_HEADER];
        b[0..4].copy_from_slice(RECORD_MAGIC);
        b[4..8].copy_from_slice(&self.x.to_le_bytes());
        b[8..12].copy_from_slice(&self.z.to_le_bytes());
        b[12..16].copy_from_slice(&self.length.to_le_bytes());
        b[16..20].copy_from_slice(&self.checksum.to_le_bytes());
        b[20] = self.codec;
        b[24..32].copy_from_slice(&self.generation.to_le_bytes());
        b[32..36].copy_from_slice(&self.timestamp.to_le_bytes());
        b
    }
    fn decode(b: &[u8; RECORD_HEADER]) -> Option<Self> {
        if &b[0..4] != RECORD_MAGIC {
            return None;
        }
        let u = |r: std::ops::Range<usize>| u32::from_le_bytes(b[r].try_into().unwrap());
        Some(Self {
            x: u(4..8) as i32,
            z: u(8..12) as i32,
            length: u(12..16),
            checksum: u(16..20),
            codec: b[20],
            generation: u64::from_le_bytes(b[24..32].try_into().unwrap()),
            timestamp: u(32..36),
        })
    }
}

impl HzrRegion {
    pub fn position(&self) -> ChunkPos {
        self.position
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Open an existing `.hzr` file for reading.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_owned();
        let file = File::open(&path)?;
        Self::from_file(path, file, false)
    }

    /// Open for reading and writing, creating an empty region with `create`.
    pub fn open_writable(path: impl AsRef<Path>, create: bool) -> io::Result<Self> {
        let path = path.as_ref().to_owned();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .truncate(false)
            .open(&path)?;
        Self::from_file(path, file, true)
    }

    fn from_file(path: PathBuf, file: File, writable: bool) -> io::Result<Self> {
        let position = position_from_path(&path)?;
        let len = file.metadata()?.len();
        let mut region = Self {
            file,
            path,
            position,
            generation: 0,
            entries: [Entry::default(); 1024],
            used: Vec::new(),
            first_free: 0,
            writable,
        };
        if len == 0 {
            if writable {
                region.write_whole_header()?;
            }
            region.rebuild_bitmap();
            return Ok(region);
        }
        let mut header = vec![0; HEADER_BYTES];
        region.file.seek(SeekFrom::Start(0))?;
        let read = read_up_to(&mut region.file, &mut header)?;
        if read < HEADER_BYTES || &header[0..4] != MAGIC {
            log::warn!(
                "{}: damaged header, rebuilding the index from its chunks",
                region.path.display()
            );
            region.recover()?;
            return Ok(region);
        }
        let version = u32::from_le_bytes(header[4..8].try_into().unwrap());
        if version > VERSION {
            return Err(invalid(format!(
                "{}: .hzr version {version} is newer than this game",
                region.path.display()
            )));
        }
        region.generation = u64::from_le_bytes(header[8..16].try_into().unwrap());
        for (i, entry) in region.entries.iter_mut().enumerate() {
            let at = INDEX_START + i * ENTRY;
            let u = |o: usize| u32::from_le_bytes(header[at + o..at + o + 4].try_into().unwrap());
            *entry = Entry {
                unit: u(0),
                length: u(4),
                timestamp: u(8),
                checksum: u(12),
            };
            let end = u64::from(entry.unit) * UNIT + RECORD_HEADER as u64 + u64::from(entry.length);
            if entry.unit != 0 && (entry.unit < FIRST_UNIT || end > len) {
                *entry = Entry::default();
            }
        }
        region.rebuild_bitmap();
        Ok(region)
    }

    fn rebuild_bitmap(&mut self) {
        self.used = vec![true; FIRST_UNIT as usize];
        let entries = self.entries;
        for entry in entries.iter().filter(|e| e.unit != 0) {
            self.force(entry.unit as usize, entry.units() as usize);
        }
        self.first_free = self.used.iter().position(|u| !u).unwrap_or(self.used.len());
    }

    fn force(&mut self, start: usize, count: usize) {
        if self.used.len() < start + count {
            self.used.resize(start + count, false);
        }
        self.used[start..start + count].fill(true);
        if self.first_free >= start && self.first_free < start + count {
            self.first_free = self.used[start + count..]
                .iter()
                .position(|&u| !u)
                .map_or(self.used.len(), |i| start + count + i);
        }
    }

    fn free(&mut self, start: usize, count: usize) {
        let end = (start + count).min(self.used.len());
        if start < end {
            self.used[start..end].fill(false);
            self.first_free = self.first_free.min(start);
        }
        // A free tail is given back to the file system on the next sync.
        while self.used.last() == Some(&false) && self.used.len() > FIRST_UNIT as usize {
            self.used.pop();
        }
    }

    fn allocate(&mut self, count: usize) -> usize {
        let mut current = self.first_free;
        loop {
            let start = (current..self.used.len())
                .find(|&s| !self.used[s])
                .unwrap_or(self.used.len());
            let end = (start..self.used.len()).find(|&s| self.used[s]);
            if end.is_none_or(|end| end - start >= count) {
                self.force(start, count);
                return start;
            }
            current = end.unwrap_or(start);
        }
    }

    fn write_whole_header(&mut self) -> io::Result<()> {
        let mut header = vec![0; FIRST_UNIT as usize * UNIT as usize];
        header[0..4].copy_from_slice(MAGIC);
        header[4..8].copy_from_slice(&VERSION.to_le_bytes());
        header[8..16].copy_from_slice(&self.generation.to_le_bytes());
        for (i, e) in self.entries.iter().enumerate() {
            let at = INDEX_START + i * ENTRY;
            header[at..at + 4].copy_from_slice(&e.unit.to_le_bytes());
            header[at + 4..at + 8].copy_from_slice(&e.length.to_le_bytes());
            header[at + 8..at + 12].copy_from_slice(&e.timestamp.to_le_bytes());
            header[at + 12..at + 16].copy_from_slice(&e.checksum.to_le_bytes());
        }
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header)
    }

    fn write_entry(&mut self, i: usize) -> io::Result<()> {
        let e = self.entries[i];
        let mut b = [0; ENTRY];
        b[0..4].copy_from_slice(&e.unit.to_le_bytes());
        b[4..8].copy_from_slice(&e.length.to_le_bytes());
        b[8..12].copy_from_slice(&e.timestamp.to_le_bytes());
        b[12..16].copy_from_slice(&e.checksum.to_le_bytes());
        self.file.seek(SeekFrom::Start(8))?;
        self.file.write_all(&self.generation.to_le_bytes())?;
        self.file
            .seek(SeekFrom::Start((INDEX_START + i * ENTRY) as u64))?;
        self.file.write_all(&b)
    }

    /// Rebuild the index from the records in the file: for each chunk, the valid record with the
    /// highest generation. Writes the rebuilt header back when the file is writable.
    pub fn recover(&mut self) -> io::Result<usize> {
        let len = self.file.metadata()?.len();
        let mut best: [Option<(u64, Entry)>; 1024] = [None; 1024];
        let mut unit = u64::from(FIRST_UNIT);
        let mut header = [0; RECORD_HEADER];
        let mut found = 0;
        while unit * UNIT + RECORD_HEADER as u64 <= len {
            self.file.seek(SeekFrom::Start(unit * UNIT))?;
            self.file.read_exact(&mut header)?;
            let Some(record) = RecordHeader::decode(&header) else {
                unit += 1;
                continue;
            };
            let chunk = ChunkPos::new(record.x, record.z);
            let end = unit * UNIT + RECORD_HEADER as u64 + u64::from(record.length);
            let fits = chunk.region() == self.position
                && end <= len
                && (record.length as usize) <= MAX_COMPRESSED;
            if fits {
                let mut data = vec![0; record.length as usize];
                self.file.read_exact(&mut data)?;
                if xxh32(&data, 0) == record.checksum {
                    let i = slot(self.position, chunk)?;
                    if best[i].is_none_or(|(g, _)| record.generation > g) {
                        best[i] = Some((
                            record.generation,
                            Entry {
                                unit: unit as u32,
                                // A deletion record's entry has length 0.
                                length: if record.codec == DELETED {
                                    0
                                } else {
                                    record.length
                                },
                                timestamp: record.timestamp,
                                checksum: record.checksum,
                            },
                        ));
                    }
                    found += 1;
                    unit = end.div_ceil(UNIT);
                    continue;
                }
            }
            unit += 1;
        }
        self.entries = [Entry::default(); 1024];
        for (i, b) in best.iter().enumerate() {
            if let Some((g, e)) = b {
                self.entries[i] = *e;
                self.generation = self.generation.max(*g);
            }
        }
        self.rebuild_bitmap();
        if self.writable {
            self.write_whole_header()?;
            self.file.sync_data()?;
        }
        Ok(found)
    }

    /// What this region says of `chunk`.
    pub fn slot_of(&self, chunk: ChunkPos) -> io::Result<Slot> {
        Ok(self.entries[slot(self.position, chunk)?].slot())
    }

    /// The highest generation written.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Every chunk this region deleted (hiding the cold tier's copies).
    pub fn deleted(&self) -> impl Iterator<Item = ChunkPos> + '_ {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.slot() == Slot::Deleted)
            .map(|(i, _)| position(self.position, i))
    }

    /// Write a chunk: into free units, then the index, then free its old units. A chunk whose
    /// bytes are the ones already here is not written again; false then.
    pub fn write(&mut self, chunk: ChunkPos, record: &ChunkRecord) -> io::Result<bool> {
        let i = slot(self.position, chunk)?;
        let record = to_zstd(record)?;
        if record.compressed.len() > MAX_COMPRESSED {
            return Err(invalid("compressed chunk limit exceeded"));
        }
        let checksum = xxh32(&record.compressed, 0);
        if self.entries[i].slot()
            == (Slot::Present {
                length: record.compressed.len() as u32,
                checksum,
            })
        {
            return Ok(false);
        }
        self.put(
            i,
            chunk,
            &record.compressed,
            record.compression as u8,
            record.timestamp,
            checksum,
        )?;
        Ok(true)
    }

    /// Delete a chunk with a deletion record, which hides the cold tier's copy.
    pub fn delete(&mut self, chunk: ChunkPos, timestamp: u32) -> io::Result<()> {
        let i = slot(self.position, chunk)?;
        if self.entries[i].slot() == Slot::Deleted {
            return Ok(());
        }
        self.put(i, chunk, &[], DELETED, timestamp, xxh32(&[], 0))
    }

    fn put(
        &mut self,
        i: usize,
        chunk: ChunkPos,
        payload: &[u8],
        codec: u8,
        timestamp: u32,
        checksum: u32,
    ) -> io::Result<()> {
        self.generation += 1;
        let header = RecordHeader {
            x: chunk.x,
            z: chunk.z,
            length: payload.len() as u32,
            checksum,
            codec,
            generation: self.generation,
            timestamp,
        };
        let entry = Entry {
            unit: 0,
            length: header.length,
            timestamp,
            checksum,
        };
        let units = entry.units() as usize;
        let start = self.allocate(units);
        if start as u64 > u64::from(u32::MAX) {
            self.free(start, units);
            return Err(invalid("region file limit exceeded"));
        }
        let mut data = Vec::with_capacity(units * UNIT as usize);
        data.extend_from_slice(&header.encode());
        data.extend_from_slice(payload);
        data.resize(units * UNIT as usize, 0);
        let written = self
            .file
            .seek(SeekFrom::Start(start as u64 * UNIT))
            .and_then(|_| self.file.write_all(&data));
        if let Err(e) = written {
            self.free(start, units);
            return Err(e);
        }
        let old = self.entries[i];
        self.entries[i] = Entry {
            unit: start as u32,
            ..entry
        };
        self.write_entry(i)?;
        if old.unit != 0 {
            // The old copy must not come back in a recovery (after a later clear, say).
            self.kill(old.unit)?;
            self.free(old.unit as usize, old.units() as usize);
        }
        Ok(())
    }

    /// Remove a chunk. Returns whether the region held it.
    pub fn clear(&mut self, chunk: ChunkPos, timestamp: u32) -> io::Result<bool> {
        let i = slot(self.position, chunk)?;
        let old = self.entries[i];
        if old.unit == 0 {
            return Ok(false);
        }
        self.entries[i] = Entry {
            timestamp,
            ..Entry::default()
        };
        self.write_entry(i)?;
        // A record left in freed space would come back in a recovery: mark it dead.
        self.kill(old.unit)?;
        self.free(old.unit as usize, old.units() as usize);
        Ok(true)
    }

    /// Overwrite a freed record's magic, so a recovery never takes it.
    fn kill(&mut self, unit: u32) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(u64::from(unit) * UNIT))?;
        self.file.write_all(&[0; 4])
    }

    /// Flush the writes to the disk, giving back a free tail.
    pub fn sync(&self) -> io::Result<()> {
        if self.writable {
            let end = (self.used.len() as u64).max(u64::from(FIRST_UNIT)) * UNIT;
            if self.file.metadata()?.len() > end {
                self.file.set_len(end)?;
            }
        }
        self.file.sync_data()
    }

    pub fn chunks(&self) -> impl Iterator<Item = (ChunkPos, u32)> + '_ {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e.slot(), Slot::Present { .. }))
            .map(|(i, e)| (position(self.position, i), e.timestamp))
    }

    /// Bytes the region's chunks take, and the file's length.
    pub fn usage(&self) -> io::Result<(u64, u64)> {
        let live = self
            .entries
            .iter()
            .filter(|e| e.unit != 0)
            .map(|e| RECORD_HEADER as u64 + u64::from(e.length))
            .sum();
        Ok((live, self.file.metadata()?.len()))
    }

    pub fn read(&mut self, chunk: ChunkPos) -> io::Result<Option<ChunkRecord>> {
        match self.read_once(chunk) {
            Ok(r) => Ok(r),
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                // Another handle may have moved the chunk since this one read the index, or the
                // index is damaged: read the index again, then rebuild it from the records.
                log::warn!(
                    "{} chunk {} {}: {e}; reading the index again",
                    self.path.display(),
                    chunk.x,
                    chunk.z
                );
                self.reload()?;
                match self.read_once(chunk) {
                    Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                        self.recover()?;
                        self.read_once(chunk)
                    }
                    other => other,
                }
            }
            Err(e) => Err(e),
        }
    }

    fn reload(&mut self) -> io::Result<()> {
        let file = self.file.try_clone()?;
        let fresh = Self::from_file(self.path.clone(), file, self.writable)?;
        self.generation = self.generation.max(fresh.generation);
        self.entries = fresh.entries;
        self.used = fresh.used;
        self.first_free = fresh.first_free;
        Ok(())
    }

    fn read_once(&mut self, chunk: ChunkPos) -> io::Result<Option<ChunkRecord>> {
        let i = slot(self.position, chunk)?;
        let e = self.entries[i];
        if e.slot()
            != (Slot::Present {
                length: e.length,
                checksum: e.checksum,
            })
        {
            return Ok(None);
        }
        let mut header = [0; RECORD_HEADER];
        self.file.seek(SeekFrom::Start(u64::from(e.unit) * UNIT))?;
        self.file.read_exact(&mut header)?;
        let record = RecordHeader::decode(&header)
            .ok_or_else(|| invalid("no chunk record at its index entry"))?;
        if record.x != chunk.x || record.z != chunk.z || record.length != e.length {
            return Err(invalid("index entry points at another chunk"));
        }
        let mut data = vec![0; record.length as usize];
        self.file.read_exact(&mut data)?;
        if xxh32(&data, 0) != record.checksum {
            return Err(invalid("chunk checksum mismatch"));
        }
        Ok(Some(ChunkRecord {
            compression: Compression::try_from(record.codec)?,
            compressed: data,
            timestamp: e.timestamp,
        }))
    }

    /// Write a whole region to `destination` (which must not exist) from `chunks`: a temporary
    /// sibling, synced, then renamed into place.
    pub fn create_from(
        destination: &Path,
        chunks: impl IntoIterator<Item = io::Result<(ChunkPos, ChunkRecord)>>,
    ) -> io::Result<()> {
        let temporary = destination.with_extension("hzr.tmp");
        let _ = std::fs::remove_file(&temporary);
        let result = (|| {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            let mut region = Self {
                file,
                path: destination.to_owned(),
                position: position_from_path(destination)?,
                generation: 0,
                entries: [Entry::default(); 1024],
                used: Vec::new(),
                first_free: 0,
                writable: true,
            };
            region.rebuild_bitmap();
            region.write_whole_header()?;
            for chunk in chunks {
                let (pos, record) = chunk?;
                region.write(pos, &record)?;
            }
            region.write_whole_header()?;
            region.sync()?;
            Ok(())
        })();
        if let Err(e) = result {
            let _ = std::fs::remove_file(&temporary);
            return Err(e);
        }
        if destination.try_exists()? {
            let _ = std::fs::remove_file(&temporary);
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "destination region already exists",
            ));
        }
        std::fs::rename(&temporary, destination)
    }
}

fn read_up_to(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut read = 0;
    while read < buf.len() {
        match file.read(&mut buf[read..])? {
            0 => break,
            n => read += n,
        }
    }
    Ok(read)
}

/// The record recompressed with zstd, unless it already is.
/// See [`recompress`].
pub fn to_zstd(record: &ChunkRecord) -> io::Result<std::borrow::Cow<'_, ChunkRecord>> {
    recompress(record, Compression::Zstd)
}

/// The record in `codec`, recompressed when it is in another one.
pub fn recompress(
    record: &ChunkRecord,
    codec: Compression,
) -> io::Result<std::borrow::Cow<'_, ChunkRecord>> {
    if record.compression == codec {
        return Ok(std::borrow::Cow::Borrowed(record));
    }
    let raw = record
        .compression
        .decode(&record.compressed, MAX_COMPRESSED * 16)?;
    Ok(std::borrow::Cow::Owned(ChunkRecord {
        compression: codec,
        compressed: codec.encode(&raw)?,
        timestamp: record.timestamp,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("horizon-hzr-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn record(seed: u8, len: usize) -> ChunkRecord {
        let raw: Vec<u8> = (0..len)
            .map(|i| (i as u8).wrapping_mul(seed) ^ (i / 97) as u8)
            .collect();
        ChunkRecord {
            compression: Compression::Zstd,
            compressed: Compression::Zstd.encode(&raw).unwrap(),
            timestamp: u32::from(seed),
        }
    }

    #[test]
    fn writes_read_back_and_survive_reopen() {
        let d = dir("roundtrip");
        let path = d.join("r.-1.2.hzr");
        let mut region = HzrRegion::open_writable(&path, true).unwrap();
        let chunks: Vec<_> = (0..40)
            .map(|i| ChunkPos::new(-32 + i % 32, 64 + i / 32))
            .collect();
        for (n, c) in chunks.iter().enumerate() {
            region
                .write(*c, &record(n as u8 + 1, 3000 + n * 500))
                .unwrap();
        }
        // Rewrite some bigger, clear some.
        for c in chunks.iter().step_by(3) {
            region.write(*c, &record(200, 20_000)).unwrap();
        }
        assert!(region.clear(chunks[1], 9).unwrap());
        region.sync().unwrap();
        drop(region);
        let mut region = HzrRegion::open(&path).unwrap();
        for (n, c) in chunks.iter().enumerate() {
            let got = region.read(*c).unwrap();
            if n == 1 {
                assert!(got.is_none());
            } else if n % 3 == 0 {
                assert_eq!(got.unwrap().compressed, record(200, 20_000).compressed);
            } else {
                assert_eq!(
                    got.unwrap().compressed,
                    record(n as u8 + 1, 3000 + n * 500).compressed
                );
            }
        }
        assert_eq!(region.chunks().count(), 39);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_damaged_index_is_rebuilt_from_the_records() {
        let d = dir("recover");
        let path = d.join("r.0.0.hzr");
        let mut region = HzrRegion::open_writable(&path, true).unwrap();
        for i in 0..10 {
            region
                .write(ChunkPos::new(i, 0), &record(i as u8 + 3, 5000))
                .unwrap();
        }
        region
            .write(ChunkPos::new(4, 0), &record(99, 7000))
            .unwrap();
        region.clear(ChunkPos::new(7, 0), 1).unwrap();
        region.sync().unwrap();
        drop(region);
        // Wipe the whole index.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[..HEADER_BYTES].fill(0);
        std::fs::write(&path, &bytes).unwrap();
        let mut region = HzrRegion::open_writable(&path, false).unwrap();
        assert_eq!(region.chunks().count(), 9);
        assert_eq!(
            region
                .read(ChunkPos::new(4, 0))
                .unwrap()
                .unwrap()
                .compressed,
            record(99, 7000).compressed
        );
        assert!(region.read(ChunkPos::new(7, 0)).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_corrupted_chunk_is_reported_not_returned() {
        let d = dir("corrupt");
        let path = d.join("r.0.0.hzr");
        let mut region = HzrRegion::open_writable(&path, true).unwrap();
        region.write(ChunkPos::new(1, 1), &record(5, 9000)).unwrap();
        region.sync().unwrap();
        drop(region);
        let mut bytes = std::fs::read(&path).unwrap();
        let at = FIRST_UNIT as usize * UNIT as usize + RECORD_HEADER + 100;
        bytes[at] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();
        let mut region = HzrRegion::open(&path).unwrap();
        // The only copy is damaged: after recovery there is no valid record for it.
        assert!(region.read(ChunkPos::new(1, 1)).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn packs_tighter_than_sectors() {
        let d = dir("pack");
        let path = d.join("r.0.0.hzr");
        let mut region = HzrRegion::open_writable(&path, true).unwrap();
        let mut payload = 0;
        for i in 0..64 {
            let r = record(i as u8 + 1, 1500);
            payload += r.compressed.len() as u64;
            region.write(ChunkPos::new(i % 32, i / 32), &r).unwrap();
        }
        region.sync().unwrap();
        let (_, len) = region.usage().unwrap();
        let header = u64::from(FIRST_UNIT) * UNIT;
        assert!(len - header <= payload + 64 * (RECORD_HEADER as u64 + UNIT));
        let _ = std::fs::remove_dir_all(&d);
    }
}
