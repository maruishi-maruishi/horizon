//! `.hza`: the cold tier of Horizon's storage, one archive per folder (`region`, `entities`,
//! `poi`) of a dimension (docs/horizon-storage.md).
//!
//! ```text
//! block 0, 1   superblocks (the valid one with the higher generation wins; written in turn)
//! block 2...   stripes of 16 data blocks and 2 Reed-Solomon parity blocks ([`crate::ecc`])
//! ```
//!
//! The data blocks of the stripes, end to end, make the *stream*: records packed one after the
//! other (chunks, the zstd dictionary, index deltas). A commit appends whole new stripes (a
//! commit's last stripe is padded), syncs them, then writes the next superblock, which points to
//! the commit's index delta: a crash anywhere leaves the last committed state. Blocks once
//! written are never written again, except to put back a block a stripe's parity rebuilt.
//!
//! Each commit's index delta lists the chunks it changed; every [`FULL_EVERY`]th is a full index.
//! Opening reads the deltas back to the last full one.
use crate::compression::{Compression, invalid};
use crate::ecc::{self, BLOCK, PAYLOAD, STRIPE, STRIPE_PAYLOAD};
use crate::region::ChunkRecord;
use horizon_core::position::ChunkPos;
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
use xxhash_rust::xxh32::xxh32;

const MAGIC: &[u8; 4] = b"HZA1";
const RECORD_MAGIC: &[u8; 4] = b"HZRC";
const VERSION: u32 = 1;
const SUPERBLOCKS: u64 = 2;
/// A full index after this many deltas.
pub const FULL_EVERY: u32 = 64;
const RECORD_HEADER: usize = 52;
const KIND_CHUNK: u8 = 1;
const KIND_DICT: u8 = 2;
const KIND_INDEX: u8 = 3;
/// zstd with the archive's dictionary.
pub const CODEC_DICT: u8 = 101;
/// The zstd level cold chunks are written with.
pub const COLD_LEVEL: i32 = 7;
/// The dictionary's size.
const DICT_SIZE: usize = 64 * 1024;
/// A chunk's decompressed size limit.
const MAX_RAW: usize = 64 * 1024 * 1024;
/// One entry of an index delta: key (8), deleted (1), offset, len, raw_len, generation, timestamp,
/// codec, hot_len, hot_sum (37).
const DELTA_ITEM: usize = 8 + 1 + 37;

/// Where a chunk is in the stream, and what the hot tier knew of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub offset: u64,
    pub len: u32,
    pub raw_len: u32,
    pub generation: u64,
    pub timestamp: u32,
    pub codec: u8,
    /// The length and xxh32 of the chunk as the hot tier stores it (zstd level 3, no
    /// dictionary): a write of the same bytes changes nothing.
    pub hot_len: u32,
    pub hot_sum: u32,
}

/// A chunk compressed for [`ColdArchive::commit`] (outside the archive's lock).
#[derive(Clone, Debug)]
pub struct Prepared {
    pub pos: ChunkPos,
    pub payload: Vec<u8>,
    pub codec: u8,
    pub raw_len: u32,
    pub timestamp: u32,
    pub hot_len: u32,
    pub hot_sum: u32,
}

impl Prepared {
    /// `raw` (a chunk's uncompressed NBT) compressed with `dict` (or plain zstd).
    pub fn new(
        pos: ChunkPos,
        raw: &[u8],
        dict: Option<&[u8]>,
        timestamp: u32,
        hot: (u32, u32),
    ) -> io::Result<Self> {
        let (payload, codec) = match dict {
            Some(d) => (
                zstd::bulk::Compressor::with_dictionary(COLD_LEVEL, d)?.compress(raw)?,
                CODEC_DICT,
            ),
            None => (
                zstd::bulk::compress(raw, COLD_LEVEL)?,
                Compression::Zstd as u8,
            ),
        };
        Ok(Self {
            pos,
            payload,
            codec,
            raw_len: raw.len() as u32,
            timestamp,
            hot_len: hot.0,
            hot_sum: hot.1,
        })
    }
}

/// A dictionary trained on `samples` (chunks' uncompressed NBT); None for too few.
pub fn train_dictionary(samples: &[Vec<u8>]) -> Option<Vec<u8>> {
    if samples.len() < 32 {
        return None;
    }
    zstd::dict::from_samples(samples, DICT_SIZE).ok()
}

/// What [`ColdArchive::verify`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Verified {
    pub stripes: u64,
    pub damaged_blocks: u64,
    pub repaired_blocks: u64,
    pub lost_stripes: u64,
    pub chunks: u64,
    pub bad_chunks: Vec<ChunkPos>,
}

pub struct ColdArchive {
    file: File,
    path: PathBuf,
    writable: bool,
    generation: u64,
    stripes: u64,
    index: HashMap<(i32, i32), Entry>,
    regions: HashMap<(i32, i32), u32>,
    dict: Option<Arc<Vec<u8>>>,
    dict_at: (u64, u32),
    delta_at: (u64, u32),
    deltas_since_full: u32,
    live: u64,
    dead: u64,
    /// Blocks rebuilt from parity while reading.
    pub repaired: u64,
    /// Of those, the ones written back to the file.
    pub healed: u64,
    /// A decompression context with the dictionary loaded, kept between chunks.
    decompressor: Option<zstd::bulk::Decompressor<'static>>,
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

struct RecordHeader {
    kind: u8,
    x: i32,
    z: i32,
    generation: u64,
    timestamp: u32,
    raw_len: u32,
    len: u32,
    codec: u8,
    hot_len: u32,
    hot_sum: u32,
    checksum: u32,
}

impl RecordHeader {
    fn encode(&self) -> [u8; RECORD_HEADER] {
        let mut b = [0; RECORD_HEADER];
        b[0..4].copy_from_slice(RECORD_MAGIC);
        b[4] = self.kind;
        b[8..12].copy_from_slice(&self.x.to_le_bytes());
        b[12..16].copy_from_slice(&self.z.to_le_bytes());
        b[16..24].copy_from_slice(&self.generation.to_le_bytes());
        b[24..28].copy_from_slice(&self.timestamp.to_le_bytes());
        b[28..32].copy_from_slice(&self.raw_len.to_le_bytes());
        b[32..36].copy_from_slice(&self.len.to_le_bytes());
        b[36] = self.codec;
        b[40..44].copy_from_slice(&self.hot_len.to_le_bytes());
        b[44..48].copy_from_slice(&self.hot_sum.to_le_bytes());
        b[48..52].copy_from_slice(&self.checksum.to_le_bytes());
        b
    }
    fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < RECORD_HEADER || &b[0..4] != RECORD_MAGIC {
            return None;
        }
        Some(Self {
            kind: b[4],
            x: le32(b, 8) as i32,
            z: le32(b, 12) as i32,
            generation: le64(b, 16),
            timestamp: le32(b, 24),
            raw_len: le32(b, 28),
            len: le32(b, 32),
            codec: b[36],
            hot_len: le32(b, 40),
            hot_sum: le32(b, 44),
            checksum: le32(b, 48),
        })
    }
}

fn data_block(offset: u64) -> (u64, u64, usize) {
    // (stripe, file block, offset in its payload)
    let d = offset / PAYLOAD as u64;
    let stripe = d / ecc::DATA as u64;
    let within = d % ecc::DATA as u64;
    (
        stripe,
        SUPERBLOCKS + stripe * STRIPE as u64 + within,
        (offset % PAYLOAD as u64) as usize,
    )
}

fn stripe_first(stripe: u64) -> u64 {
    SUPERBLOCKS + stripe * STRIPE as u64
}

impl ColdArchive {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn dictionary(&self) -> Option<Arc<Vec<u8>>> {
        self.dict.clone()
    }
    pub fn get(&self, pos: ChunkPos) -> Option<Entry> {
        self.index.get(&(pos.x, pos.z)).copied()
    }
    pub fn len(&self) -> usize {
        self.index.len()
    }
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }
    /// Whether any chunk of region `region` is here.
    pub fn has_region(&self, region: ChunkPos) -> bool {
        self.regions
            .get(&(region.x, region.z))
            .is_some_and(|n| *n > 0)
    }
    /// The chunks of region `region`, with their timestamps.
    pub fn chunks_in(&self, region: ChunkPos) -> Vec<(ChunkPos, u32)> {
        if !self.has_region(region) {
            return Vec::new();
        }
        let mut out: Vec<_> = self
            .index
            .iter()
            .filter(|((x, z), _)| x >> 5 == region.x && z >> 5 == region.z)
            .map(|((x, z), e)| (ChunkPos::new(*x, *z), e.timestamp))
            .collect();
        out.sort_by_key(|(p, _)| (p.z, p.x));
        out
    }
    /// Every chunk here.
    pub fn all(&self) -> Vec<ChunkPos> {
        let mut out: Vec<_> = self
            .index
            .keys()
            .map(|(x, z)| ChunkPos::new(*x, *z))
            .collect();
        out.sort_by_key(|p| (p.x, p.z));
        out
    }
    /// The bytes of live records and of replaced or deleted ones.
    pub fn usage(&self) -> (u64, u64) {
        (self.live, self.dead)
    }

    /// Open `path`, creating an empty archive with `writable` when there is none.
    pub fn open(path: impl AsRef<Path>, writable: bool) -> io::Result<Self> {
        let path = path.as_ref().to_owned();
        let file = if writable {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)?
        } else {
            File::open(&path)?
        };
        let mut archive = Self {
            file,
            path,
            writable,
            generation: 0,
            stripes: 0,
            index: HashMap::new(),
            regions: HashMap::new(),
            dict: None,
            dict_at: (0, 0),
            delta_at: (0, 0),
            deltas_since_full: 0,
            live: 0,
            dead: 0,
            repaired: 0,
            healed: 0,
            decompressor: None,
        };
        let len = archive.file.metadata()?.len();
        if len == 0 {
            return Ok(archive);
        }
        let mut best: Option<Vec<u8>> = None;
        for slot in 0..SUPERBLOCKS {
            if let Some(block) = archive.read_block(slot)
                && let Some(payload) = ecc::open(slot, &block)
                && &payload[0..4] == MAGIC
                && best.as_ref().is_none_or(|b| le64(payload, 8) > le64(b, 8))
            {
                best = Some(payload.to_vec());
            }
        }
        let Some(sb) = best else {
            return Err(invalid(format!(
                "{}: no valid superblock",
                archive.path.display()
            )));
        };
        if le32(&sb, 4) > VERSION {
            return Err(invalid(format!(
                "{}: archive version is newer than this game",
                archive.path.display()
            )));
        }
        archive.generation = le64(&sb, 8);
        archive.stripes = le64(&sb, 16);
        archive.delta_at = (le64(&sb, 24), le32(&sb, 32));
        archive.deltas_since_full = le32(&sb, 36);
        archive.dict_at = (le64(&sb, 40), le32(&sb, 48));
        archive.live = le64(&sb, 56);
        archive.dead = le64(&sb, 64);
        let committed = stripe_first(archive.stripes) * BLOCK as u64;
        if writable && len > committed {
            // A commit that never got its superblock.
            archive.file.set_len(committed)?;
        }
        if archive.dict_at.1 > 0 {
            let (header, payload) = archive.read_record(archive.dict_at.0)?;
            if header.kind != KIND_DICT {
                return Err(invalid("the superblock's dictionary is no dictionary"));
            }
            archive.dict = Some(Arc::new(payload));
        }
        archive.load_index()?;
        Ok(archive)
    }

    fn load_index(&mut self) -> io::Result<()> {
        let mut deltas = Vec::new();
        let mut at = self.delta_at;
        while at.1 > 0 {
            let (header, payload) = self.read_record(at.0)?;
            if header.kind != KIND_INDEX {
                return Err(invalid("an index delta that is no index"));
            }
            let raw = zstd::bulk::decompress(&payload, header.raw_len as usize)?;
            let full = raw.first() == Some(&1);
            let prev = (le64(&raw, 1), le32(&raw, 9));
            deltas.push(raw);
            if full {
                break;
            }
            at = prev;
        }
        for raw in deltas.iter().rev() {
            let count = le32(raw, 13) as usize;
            let mut p = 17;
            for _ in 0..count {
                if p + DELTA_ITEM > raw.len() {
                    return Err(invalid("a short index delta"));
                }
                let key = (le32(raw, p) as i32, le32(raw, p + 4) as i32);
                let deleted = raw[p + 8] == 1;
                let e = &raw[p + 9..];
                let entry = Entry {
                    offset: le64(e, 0),
                    len: le32(e, 8),
                    raw_len: le32(e, 12),
                    generation: le64(e, 16),
                    timestamp: le32(e, 24),
                    codec: e[28],
                    hot_len: le32(e, 29),
                    hot_sum: le32(e, 33),
                };
                p += DELTA_ITEM;
                if deleted {
                    if self.index.remove(&key).is_some() {
                        *self.regions.entry((key.0 >> 5, key.1 >> 5)).or_default() -= 1;
                    }
                } else if self.index.insert(key, entry).is_none() {
                    *self.regions.entry((key.0 >> 5, key.1 >> 5)).or_default() += 1;
                }
            }
        }
        Ok(())
    }

    fn read_block(&mut self, number: u64) -> Option<Vec<u8>> {
        let mut block = vec![0; BLOCK];
        self.file
            .seek(SeekFrom::Start(number * BLOCK as u64))
            .ok()?;
        self.file.read_exact(&mut block).ok()?;
        Some(block)
    }

    /// A stripe's data, rebuilt from parity where blocks are damaged (and written back when the
    /// archive is writable).
    fn stripe_data(&mut self, stripe: u64, repair: bool) -> io::Result<(Vec<u8>, usize)> {
        let first = stripe_first(stripe);
        let blocks: Vec<Option<Vec<u8>>> = (0..STRIPE as u64)
            .map(|i| self.read_block(first + i))
            .collect();
        let decoded = ecc::decode_stripe(first, &blocks).ok_or_else(|| {
            invalid(format!(
                "{}: stripe {stripe} has more damaged blocks than its parity can rebuild",
                self.path.display()
            ))
        })?;
        let damaged = decoded.repaired.len();
        if damaged > 0 {
            log::warn!(
                "{}: stripe {stripe}: rebuilt {damaged} damaged block(s) from parity",
                self.path.display()
            );
            self.repaired += damaged as u64;
            if repair && self.writable {
                let rebuilt = ecc::encode_stripe(first, &decoded.data);
                for &i in &decoded.repaired {
                    self.file
                        .seek(SeekFrom::Start((first + i as u64) * BLOCK as u64))?;
                    self.file.write_all(&rebuilt[i])?;
                }
                self.file.sync_data()?;
                self.healed += damaged as u64;
            }
        }
        Ok((decoded.data, damaged))
    }

    /// `len` bytes of the stream at `offset`.
    fn read_stream(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(len);
        let mut at = offset;
        let mut stripe_cache: Option<(u64, Vec<u8>)> = None;
        // The data blocks the range covers within its first stripe sit next to each other on
        // disk: read them in one go.
        let mut run: Vec<u8> = Vec::new();
        let mut run_first = u64::MAX;
        {
            let (stripe, block, within) = data_block(at);
            let last_in_stripe = stripe_first(stripe) + ecc::DATA as u64 - 1;
            let blocks_needed = (within + len).div_ceil(PAYLOAD) as u64;
            let count = blocks_needed.min(last_in_stripe - block + 1);
            if stripe < self.stripes && count > 1 {
                let mut buf = vec![0u8; count as usize * BLOCK];
                if self
                    .file
                    .seek(SeekFrom::Start(block * BLOCK as u64))
                    .is_ok()
                    && self.file.read_exact(&mut buf).is_ok()
                {
                    run = buf;
                    run_first = block;
                }
            }
        }
        while out.len() < len {
            let (stripe, block, within) = data_block(at);
            if stripe >= self.stripes {
                return Err(invalid("a record past the end of the archive"));
            }
            let take = (PAYLOAD - within).min(len - out.len());
            let from_run = (run_first != u64::MAX
                && block >= run_first
                && ((block - run_first) as usize) < run.len() / BLOCK)
                .then(|| {
                    let i = (block - run_first) as usize;
                    run[i * BLOCK..(i + 1) * BLOCK].to_vec()
                });
            let whole = from_run
                .or_else(|| self.read_block(block))
                .filter(|b| ecc::open(block, b).is_some());
            match whole {
                Some(b) => out.extend_from_slice(&b[within..within + take]),
                None => {
                    if stripe_cache.as_ref().is_none_or(|(s, _)| *s != stripe) {
                        let writable = self.writable;
                        stripe_cache = Some((stripe, self.stripe_data(stripe, writable)?.0));
                    }
                    let data = &stripe_cache.as_ref().unwrap().1;
                    let start = (at - stripe * STRIPE_PAYLOAD as u64) as usize;
                    out.extend_from_slice(&data[start..start + take]);
                }
            }
            at += take as u64;
        }
        Ok(out)
    }

    fn read_record(&mut self, offset: u64) -> io::Result<(RecordHeader, Vec<u8>)> {
        let head = self.read_stream(offset, RECORD_HEADER)?;
        let header = RecordHeader::decode(&head)
            .ok_or_else(|| invalid("no record where the index points"))?;
        let payload = self.read_stream(offset + RECORD_HEADER as u64, header.len as usize)?;
        if xxh32(&payload, 0) != header.checksum {
            return Err(invalid("a record's checksum doesn't match"));
        }
        Ok((header, payload))
    }

    /// A chunk's compressed payload as stored, with its entry (for copying without recompressing).
    pub fn raw_payload(&mut self, pos: ChunkPos) -> io::Result<Option<(Entry, Vec<u8>)>> {
        let Some(entry) = self.get(pos) else {
            return Ok(None);
        };
        let (header, payload) = self.read_record(entry.offset)?;
        if header.kind != KIND_CHUNK || header.x != pos.x || header.z != pos.z {
            return Err(invalid("the index points at another record"));
        }
        Ok(Some((entry, payload)))
    }

    /// A chunk, decompressed (`Compression::None`).
    pub fn read(&mut self, pos: ChunkPos) -> io::Result<Option<ChunkRecord>> {
        let Some((entry, payload)) = self.raw_payload(pos)? else {
            return Ok(None);
        };
        let raw = self.decompress(entry.codec, &payload, entry.raw_len)?;
        Ok(Some(ChunkRecord {
            compression: Compression::None,
            compressed: raw,
            timestamp: entry.timestamp,
        }))
    }

    fn decompress(&mut self, codec: u8, payload: &[u8], raw_len: u32) -> io::Result<Vec<u8>> {
        let capacity = (raw_len as usize).min(MAX_RAW);
        match codec {
            CODEC_DICT => {
                if self.decompressor.is_none() {
                    let dict = self.dict.as_ref().ok_or_else(|| {
                        invalid("a chunk compressed with a dictionary the archive lacks")
                    })?;
                    self.decompressor = Some(zstd::bulk::Decompressor::with_dictionary(dict)?);
                }
                Ok(self
                    .decompressor
                    .as_mut()
                    .unwrap()
                    .decompress(payload, capacity)?)
            }
            c if c == Compression::Zstd as u8 => Ok(zstd::bulk::decompress(payload, capacity)?),
            other => Err(invalid(format!("unknown archive codec {other}"))),
        }
    }

    /// Install a dictionary for chunks prepared from now on (written with the next commit).
    pub fn set_dictionary(&mut self, dict: Vec<u8>) {
        self.decompressor = None;
        self.dict = Some(Arc::new(dict));
        self.dict_at = (u64::MAX, 0);
    }

    /// Append `chunks` and remove `deleted`, as one commit.
    pub fn commit(&mut self, chunks: &[Prepared], deleted: &[ChunkPos]) -> io::Result<()> {
        self.commit_with(chunks, deleted, false)
    }

    /// [`commit`](Self::commit); `full` writes a full index (and commits even with no change).
    fn commit_with(
        &mut self,
        chunks: &[Prepared],
        deleted: &[ChunkPos],
        full: bool,
    ) -> io::Result<()> {
        if !self.writable {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the archive is read-only",
            ));
        }
        if chunks.is_empty() && deleted.is_empty() && self.dict_at.0 != u64::MAX && !full {
            return Ok(());
        }
        let generation = self.generation + 1;
        let start = self.stripes * STRIPE_PAYLOAD as u64;
        let mut stream = Vec::new();
        let mut dict_at = self.dict_at;
        if self.dict_at.0 == u64::MAX {
            let dict = self.dict.clone().unwrap();
            dict_at = (start, dict.len() as u32);
            push_record(
                &mut stream,
                KIND_DICT,
                0,
                0,
                generation,
                0,
                dict.len() as u32,
                0,
                (0, 0),
                &dict,
            );
        }
        let mut changes: Vec<((i32, i32), Option<Entry>)> = Vec::new();
        for c in chunks {
            let offset = start + stream.len() as u64;
            push_record(
                &mut stream,
                KIND_CHUNK,
                c.pos.x,
                c.pos.z,
                generation,
                c.timestamp,
                c.raw_len,
                c.codec,
                (c.hot_len, c.hot_sum),
                &c.payload,
            );
            changes.push((
                (c.pos.x, c.pos.z),
                Some(Entry {
                    offset,
                    len: c.payload.len() as u32,
                    raw_len: c.raw_len,
                    generation,
                    timestamp: c.timestamp,
                    codec: c.codec,
                    hot_len: c.hot_len,
                    hot_sum: c.hot_sum,
                }),
            ));
        }
        for d in deleted {
            if self.index.contains_key(&(d.x, d.z)) {
                changes.push(((d.x, d.z), None));
            }
        }
        // The new index, to account and to write.
        let mut index = self.index.clone();
        let mut regions = self.regions.clone();
        let (mut live, mut dead) = (self.live, self.dead);
        for (key, entry) in &changes {
            if let Some(old) = index.get(key) {
                dead += RECORD_HEADER as u64 + u64::from(old.len);
                live = live.saturating_sub(RECORD_HEADER as u64 + u64::from(old.len));
            }
            match entry {
                Some(e) => {
                    live += RECORD_HEADER as u64 + u64::from(e.len);
                    if index.insert(*key, *e).is_none() {
                        *regions.entry((key.0 >> 5, key.1 >> 5)).or_default() += 1;
                    }
                }
                None => {
                    if index.remove(key).is_some() {
                        *regions.entry((key.0 >> 5, key.1 >> 5)).or_default() -= 1;
                    }
                }
            }
        }
        let full = full || self.deltas_since_full + 1 >= FULL_EVERY || self.delta_at.1 == 0;
        let mut delta = Vec::new();
        delta.push(u8::from(full));
        delta.extend_from_slice(&self.delta_at.0.to_le_bytes());
        delta.extend_from_slice(&self.delta_at.1.to_le_bytes());
        let listed: Vec<((i32, i32), Option<Entry>)> = if full {
            let mut all: Vec<_> = index.iter().map(|(k, e)| (*k, Some(*e))).collect();
            all.sort_by_key(|(k, _)| *k);
            all
        } else {
            changes.clone()
        };
        delta.extend_from_slice(&(listed.len() as u32).to_le_bytes());
        for (key, entry) in &listed {
            delta.extend_from_slice(&key.0.to_le_bytes());
            delta.extend_from_slice(&key.1.to_le_bytes());
            delta.push(u8::from(entry.is_none()));
            let e = entry.unwrap_or(Entry {
                offset: 0,
                len: 0,
                raw_len: 0,
                generation: 0,
                timestamp: 0,
                codec: 0,
                hot_len: 0,
                hot_sum: 0,
            });
            delta.extend_from_slice(&e.offset.to_le_bytes());
            delta.extend_from_slice(&e.len.to_le_bytes());
            delta.extend_from_slice(&e.raw_len.to_le_bytes());
            delta.extend_from_slice(&e.generation.to_le_bytes());
            delta.extend_from_slice(&e.timestamp.to_le_bytes());
            delta.push(e.codec);
            delta.extend_from_slice(&e.hot_len.to_le_bytes());
            delta.extend_from_slice(&e.hot_sum.to_le_bytes());
        }
        let packed = zstd::bulk::compress(&delta, 3)?;
        let delta_at = (start + stream.len() as u64, packed.len() as u32);
        push_record(
            &mut stream,
            KIND_INDEX,
            0,
            0,
            generation,
            0,
            delta.len() as u32,
            Compression::Zstd as u8,
            (0, 0),
            &packed,
        );
        // Whole stripes, then the superblock.
        let stripes = stream.len().div_ceil(STRIPE_PAYLOAD) as u64;
        let mut out = Vec::with_capacity(stripes as usize * STRIPE * BLOCK);
        for (i, data) in stream.chunks(STRIPE_PAYLOAD).enumerate() {
            for block in ecc::encode_stripe(stripe_first(self.stripes + i as u64), data) {
                out.extend_from_slice(&block);
            }
        }
        self.file
            .seek(SeekFrom::Start(stripe_first(self.stripes) * BLOCK as u64))?;
        self.file.write_all(&out)?;
        self.file.sync_data()?;
        let next = Superblock {
            generation,
            stripes: self.stripes + stripes,
            delta_at,
            deltas_since_full: if full { 0 } else { self.deltas_since_full + 1 },
            dict_at,
            live,
            dead,
        };
        self.write_superblock(&next)?;
        self.file.sync_data()?;
        self.generation = generation;
        self.stripes = next.stripes;
        self.delta_at = delta_at;
        self.deltas_since_full = next.deltas_since_full;
        self.dict_at = dict_at;
        self.index = index;
        self.regions = regions;
        self.live = live;
        self.dead = dead;
        Ok(())
    }

    fn write_superblock(&mut self, s: &Superblock) -> io::Result<()> {
        let mut p = vec![0u8; PAYLOAD];
        p[0..4].copy_from_slice(MAGIC);
        p[4..8].copy_from_slice(&VERSION.to_le_bytes());
        p[8..16].copy_from_slice(&s.generation.to_le_bytes());
        p[16..24].copy_from_slice(&s.stripes.to_le_bytes());
        p[24..32].copy_from_slice(&s.delta_at.0.to_le_bytes());
        p[32..36].copy_from_slice(&s.delta_at.1.to_le_bytes());
        p[36..40].copy_from_slice(&s.deltas_since_full.to_le_bytes());
        p[40..48].copy_from_slice(&s.dict_at.0.to_le_bytes());
        p[48..52].copy_from_slice(&s.dict_at.1.to_le_bytes());
        p[56..64].copy_from_slice(&s.live.to_le_bytes());
        p[64..72].copy_from_slice(&s.dead.to_le_bytes());
        let slot = s.generation % SUPERBLOCKS;
        self.file.seek(SeekFrom::Start(slot * BLOCK as u64))?;
        self.file.write_all(&ecc::seal(slot, &p))
    }

    /// Whether replaced and deleted records take more room than the live ones (and enough to
    /// matter): time to [`compact`](Self::compact).
    pub fn wants_compaction(&self) -> bool {
        self.dead > self.live && self.dead > 32 * 1024 * 1024
    }

    /// Rewrite the archive with only its live records (a new file, then renamed over this one).
    pub fn compact(&mut self) -> io::Result<()> {
        let temporary = self.path.with_extension("hza.compact");
        let _ = std::fs::remove_file(&temporary);
        {
            let mut fresh = ColdArchive::open(&temporary, true)?;
            if let Some(d) = &self.dict {
                fresh.set_dictionary(d.as_ref().clone());
            }
            let mut batch = Vec::new();
            for pos in self.all() {
                let (entry, payload) = self.raw_payload(pos)?.unwrap();
                batch.push(Prepared {
                    pos,
                    payload,
                    codec: entry.codec,
                    raw_len: entry.raw_len,
                    timestamp: entry.timestamp,
                    hot_len: entry.hot_len,
                    hot_sum: entry.hot_sum,
                });
                if batch.len() >= 1024 {
                    fresh.commit(&batch, &[])?;
                    batch.clear();
                }
            }
            // The last commit carries a full index.
            fresh.commit_with(&batch, &[], true)?;
        }
        let path = self.path.clone();
        let writable = self.writable;
        // Close this handle before the rename replaces the file.
        self.file = File::open(&temporary)?;
        std::fs::rename(&temporary, &path)?;
        *self = ColdArchive::open(&path, writable)?;
        Ok(())
    }

    /// Check every block and every chunk; with `repair`, write back the blocks parity rebuilt.
    pub fn verify(&mut self, repair: bool) -> Verified {
        // Blocks already rebuilt and written back since opening (reading the index) count too.
        let mut v = Verified {
            stripes: self.stripes,
            damaged_blocks: self.healed,
            repaired_blocks: self.healed,
            ..Verified::default()
        };
        for stripe in 0..self.stripes {
            let first = stripe_first(stripe);
            let blocks: Vec<Option<Vec<u8>>> = (0..STRIPE as u64)
                .map(|i| self.read_block(first + i))
                .collect();
            let bad = blocks
                .iter()
                .enumerate()
                .filter(|(i, b)| {
                    b.as_ref()
                        .and_then(|b| ecc::open(first + *i as u64, b))
                        .is_none()
                })
                .count() as u64;
            if bad == 0 {
                continue;
            }
            v.damaged_blocks += bad;
            match self.stripe_data(stripe, repair) {
                Ok(_) if repair => v.repaired_blocks += bad,
                Ok(_) => {}
                Err(_) => v.lost_stripes += 1,
            }
        }
        self.healed = 0;
        for pos in self.all() {
            v.chunks += 1;
            if self.read(pos).is_err() {
                v.bad_chunks.push(pos);
            }
        }
        v
    }
}

struct Superblock {
    generation: u64,
    stripes: u64,
    delta_at: (u64, u32),
    deltas_since_full: u32,
    dict_at: (u64, u32),
    live: u64,
    dead: u64,
}

#[allow(clippy::too_many_arguments)]
fn push_record(
    stream: &mut Vec<u8>,
    kind: u8,
    x: i32,
    z: i32,
    generation: u64,
    timestamp: u32,
    raw_len: u32,
    codec: u8,
    hot: (u32, u32),
    payload: &[u8],
) {
    let header = RecordHeader {
        kind,
        x,
        z,
        generation,
        timestamp,
        raw_len,
        len: payload.len() as u32,
        codec,
        hot_len: hot.0,
        hot_sum: hot.1,
        checksum: xxh32(payload, 0),
    };
    stream.extend_from_slice(&header.encode());
    stream.extend_from_slice(payload);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("horizon-hza-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Chunk-like data: repetitive, a little different for each chunk.
    fn raw(x: i32, z: i32, len: usize) -> Vec<u8> {
        let words: [&[u8]; 5] = [
            b"minecraft:stone",
            b"minecraft:dirt",
            b"Palette",
            b"block_states",
            b"minecraft:deepslate",
        ];
        let mut out = Vec::new();
        let mut i = 0usize;
        while out.len() < len {
            out.extend_from_slice(words[(i + (x as usize) * 3 + (z as usize)) % 5]);
            out.push((i as i32 ^ x ^ z) as u8);
            i += 1;
        }
        out.truncate(len);
        out
    }

    fn put(pos: ChunkPos, len: usize, dict: Option<&[u8]>) -> Prepared {
        Prepared::new(pos, &raw(pos.x, pos.z, len), dict, 7, (0, 0)).unwrap()
    }

    #[test]
    fn commits_survive_reopening_and_deltas_chain_to_full_indexes() {
        let d = dir("commits");
        let path = d.join("archive.hza");
        let mut a = ColdArchive::open(&path, true).unwrap();
        // More commits than FULL_EVERY, so a full index sits in the middle of the chain.
        for round in 0..(FULL_EVERY as i32 + 6) {
            let chunks: Vec<_> = (0..3)
                .map(|i| put(ChunkPos::new(round, i), 3000 + (round as usize) * 10, None))
                .collect();
            let deleted = if round % 7 == 6 {
                vec![ChunkPos::new(round - 1, 0)]
            } else {
                vec![]
            };
            a.commit(&chunks, &deleted).unwrap();
        }
        drop(a);
        let mut a = ColdArchive::open(&path, false).unwrap();
        for round in 0..(FULL_EVERY as i32 + 6) {
            for i in 0..3 {
                let pos = ChunkPos::new(round, i);
                let got = a.read(pos).unwrap();
                if i == 0 && (round + 1) % 7 == 6 {
                    assert!(got.is_none(), "{pos:?} was deleted");
                } else {
                    assert_eq!(
                        got.unwrap().compressed,
                        raw(round, i, 3000 + round as usize * 10)
                    );
                }
            }
        }
        assert!(a.has_region(ChunkPos::new(0, 0)));
        assert!(!a.has_region(ChunkPos::new(9, 9)));
    }

    #[test]
    fn a_commit_cut_short_leaves_the_one_before() {
        let d = dir("crash");
        let path = d.join("archive.hza");
        let mut a = ColdArchive::open(&path, true).unwrap();
        a.commit(&[put(ChunkPos::new(1, 1), 5000, None)], &[])
            .unwrap();
        a.commit(&[put(ChunkPos::new(2, 2), 9000, None)], &[])
            .unwrap();
        drop(a);
        // The second commit's stripes are there, its superblock is not: put back the first one's.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[..2 * BLOCK].fill(0);
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            ColdArchive::open(&path, true).is_err(),
            "no superblock at all is an error"
        );
        // Now a real torn commit: stripes written, superblock not.
        let path2 = d.join("torn.hza");
        let mut a = ColdArchive::open(&path2, true).unwrap();
        a.commit(&[put(ChunkPos::new(1, 1), 5000, None)], &[])
            .unwrap();
        drop(a);
        let before = std::fs::read(&path2).unwrap();
        let mut a = ColdArchive::open(&path2, true).unwrap();
        a.commit(&[put(ChunkPos::new(2, 2), 9000, None)], &[])
            .unwrap();
        drop(a);
        let mut after = std::fs::read(&path2).unwrap();
        // Restore the superblocks as they were before the second commit.
        after[..2 * BLOCK].copy_from_slice(&before[..2 * BLOCK]);
        std::fs::write(&path2, &after).unwrap();
        let mut a = ColdArchive::open(&path2, true).unwrap();
        assert!(a.read(ChunkPos::new(1, 1)).unwrap().is_some());
        assert!(a.read(ChunkPos::new(2, 2)).unwrap().is_none());
        assert_eq!(
            std::fs::metadata(&path2).unwrap().len(),
            before.len() as u64,
            "the torn tail is cut off"
        );
    }

    #[test]
    fn damaged_blocks_are_rebuilt_on_read_and_by_verify() {
        let d = dir("damage");
        let path = d.join("archive.hza");
        let mut a = ColdArchive::open(&path, true).unwrap();
        let chunks: Vec<_> = (0..20)
            .map(|i| put(ChunkPos::new(i, 0), 20_000, None))
            .collect();
        a.commit(&chunks, &[]).unwrap();
        drop(a);
        let mut bytes = std::fs::read(&path).unwrap();
        // Two blocks of the first stripe: a flipped bit and a zeroed block.
        bytes[(SUPERBLOCKS as usize + 1) * BLOCK + 10] ^= 0x40;
        let z = (SUPERBLOCKS as usize + 5) * BLOCK;
        bytes[z..z + BLOCK].fill(0);
        std::fs::write(&path, &bytes).unwrap();
        let mut a = ColdArchive::open(&path, false).unwrap();
        for i in 0..20 {
            assert_eq!(
                a.read(ChunkPos::new(i, 0)).unwrap().unwrap().compressed,
                raw(i, 0, 20_000)
            );
        }
        assert!(a.repaired >= 2);
        drop(a);
        let mut a = ColdArchive::open(&path, true).unwrap();
        let v = a.verify(true);
        assert_eq!(
            (
                v.damaged_blocks,
                v.repaired_blocks,
                v.lost_stripes,
                v.bad_chunks.len()
            ),
            (2, 2, 0, 0)
        );
        let v = a.verify(false);
        assert_eq!(v.damaged_blocks, 0, "repaired on disk");
    }

    #[test]
    fn a_dictionary_shrinks_small_chunks_and_compaction_keeps_everything() {
        let d = dir("dict");
        let samples: Vec<Vec<u8>> = (0..200).map(|i| raw(i, i * 3, 2500)).collect();
        let dict = train_dictionary(&samples).expect("a dictionary");
        let plain: usize = (0..200)
            .map(|i| put(ChunkPos::new(i, 0), 2500, None).payload.len())
            .sum();
        let with: usize = (0..200)
            .map(|i| put(ChunkPos::new(i, 0), 2500, Some(&dict)).payload.len())
            .sum();
        assert!(with < plain, "dictionary {with} vs plain {plain}");
        let path = d.join("archive.hza");
        let mut a = ColdArchive::open(&path, true).unwrap();
        a.set_dictionary(dict.clone());
        let chunks: Vec<_> = (0..50)
            .map(|i| put(ChunkPos::new(i, 1), 2500, Some(&dict)))
            .collect();
        a.commit(&chunks, &[]).unwrap();
        // Replace them all, then compact.
        let chunks: Vec<_> = (0..50)
            .map(|i| put(ChunkPos::new(i, 1), 2600, Some(&dict)))
            .collect();
        a.commit(&chunks, &[]).unwrap();
        let before = std::fs::metadata(&path).unwrap().len();
        a.compact().unwrap();
        let after = std::fs::metadata(&path).unwrap().len();
        assert!(after < before, "{after} < {before}");
        drop(a);
        let mut a = ColdArchive::open(&path, false).unwrap();
        assert_eq!(a.len(), 50);
        assert_eq!(
            a.read(ChunkPos::new(7, 1)).unwrap().unwrap().compressed,
            raw(7, 1, 2600)
        );
        assert_eq!(a.usage().1, 0, "no dead records after compaction");
    }
}
