//! Self-checking blocks in Reed–Solomon stripes, for `.hza` archives ([`crate::archive`]).
//!
//! A block is [`BLOCK`] bytes: [`PAYLOAD`] bytes of data, then the CRC32 of its file block number
//! and its data, so a damaged or misplaced block tells on itself. Every [`DATA`] data blocks get
//! [`PARITY`] parity blocks (the same code QR codes use): any [`PARITY`] blocks of a stripe that
//! are damaged or can't be read are rebuilt from the others.
use reed_solomon_erasure::galois_8::ReedSolomon;
use std::sync::OnceLock;

pub const BLOCK: usize = 4096;
pub const PAYLOAD: usize = BLOCK - 4;
pub const DATA: usize = 16;
pub const PARITY: usize = 2;
pub const STRIPE: usize = DATA + PARITY;
/// The data bytes of one stripe.
pub const STRIPE_PAYLOAD: usize = DATA * PAYLOAD;

fn codec() -> &'static ReedSolomon {
    static CODEC: OnceLock<ReedSolomon> = OnceLock::new();
    CODEC.get_or_init(|| ReedSolomon::new(DATA, PARITY).expect("Reed-Solomon parameters"))
}

fn crc(number: u64, payload: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(&number.to_le_bytes());
    h.update(payload);
    h.finalize()
}

/// `payload` as file block `number`.
pub fn seal(number: u64, payload: &[u8]) -> [u8; BLOCK] {
    debug_assert!(payload.len() <= PAYLOAD);
    let mut block = [0; BLOCK];
    block[..payload.len()].copy_from_slice(payload);
    let sum = crc(number, &block[..PAYLOAD]);
    block[PAYLOAD..].copy_from_slice(&sum.to_le_bytes());
    block
}

/// The payload of file block `number`, if the block is whole.
pub fn open(number: u64, block: &[u8]) -> Option<&[u8]> {
    if block.len() != BLOCK {
        return None;
    }
    let stored = u32::from_le_bytes(block[PAYLOAD..].try_into().unwrap());
    (crc(number, &block[..PAYLOAD]) == stored).then(|| &block[..PAYLOAD])
}

/// The [`STRIPE`] blocks of a stripe whose first block is file block `first`, from up to
/// [`STRIPE_PAYLOAD`] bytes of data (the rest zero).
pub fn encode_stripe(first: u64, data: &[u8]) -> Vec<[u8; BLOCK]> {
    debug_assert!(data.len() <= STRIPE_PAYLOAD);
    let mut shards: Vec<Vec<u8>> = (0..STRIPE).map(|_| vec![0u8; PAYLOAD]).collect();
    for (i, chunk) in data.chunks(PAYLOAD).enumerate() {
        shards[i][..chunk.len()].copy_from_slice(chunk);
    }
    codec().encode(&mut shards).expect("Reed-Solomon encoding");
    shards
        .iter()
        .enumerate()
        .map(|(i, payload)| seal(first + i as u64, payload))
        .collect()
}

/// What decoding a stripe found.
#[derive(Debug, PartialEq, Eq)]
pub struct Decoded {
    /// The stripe's [`STRIPE_PAYLOAD`] data bytes.
    pub data: Vec<u8>,
    /// The stripe positions (0 to `STRIPE - 1`) that were damaged and rebuilt.
    pub repaired: Vec<usize>,
}

/// The data of a stripe whose first block is file block `first`, from its blocks (`None` for one
/// that couldn't be read). None when more than [`PARITY`] blocks are damaged.
pub fn decode_stripe(first: u64, blocks: &[Option<Vec<u8>>]) -> Option<Decoded> {
    let mut shards: Vec<Option<Vec<u8>>> = (0..STRIPE)
        .map(|i| {
            blocks
                .get(i)
                .and_then(|b| b.as_deref())
                .and_then(|b| open(first + i as u64, b))
                .map(<[u8]>::to_vec)
        })
        .collect();
    let repaired: Vec<usize> = (0..STRIPE).filter(|&i| shards[i].is_none()).collect();
    if repaired.len() > PARITY {
        return None;
    }
    if !repaired.is_empty() {
        codec().reconstruct(&mut shards).ok()?;
    }
    let mut data = Vec::with_capacity(STRIPE_PAYLOAD);
    for shard in shards.iter().take(DATA) {
        data.extend_from_slice(shard.as_ref()?);
    }
    Some(Decoded { data, repaired })
}

/// The block at stripe position `i` of the stripe starting at `first`, rebuilt (for writing back).
pub fn rebuilt_block(first: u64, decoded: &Decoded, i: usize) -> [u8; BLOCK] {
    encode_stripe(first, &decoded.data)[i]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> Vec<u8> {
        (0..STRIPE_PAYLOAD - 1000)
            .map(|i| (i * 31 % 251) as u8)
            .collect()
    }

    #[test]
    fn two_damaged_blocks_of_a_stripe_are_rebuilt() {
        let first = 2 + 18 * 5;
        let blocks = encode_stripe(first, &data());
        let mut read: Vec<Option<Vec<u8>>> = blocks.iter().map(|b| Some(b.to_vec())).collect();
        read[3].as_mut().unwrap()[100] ^= 1; // a flipped bit
        read[17] = None; // a parity block that can't be read
        let decoded = decode_stripe(first, &read).unwrap();
        assert_eq!(decoded.repaired, [3, 17]);
        assert_eq!(&decoded.data[..data().len()], &data()[..]);
        assert_eq!(rebuilt_block(first, &decoded, 3), blocks[3]);
    }

    #[test]
    fn three_damaged_blocks_are_too_many_and_blocks_know_their_place() {
        let blocks = encode_stripe(2, &data());
        let mut read: Vec<Option<Vec<u8>>> = blocks.iter().map(|b| Some(b.to_vec())).collect();
        read[0] = None;
        read[1] = None;
        read[2] = None;
        assert!(decode_stripe(2, &read).is_none());
        // A block written at the wrong place fails its check there.
        assert!(open(2, &blocks[0]).is_some());
        assert!(open(3, &blocks[0]).is_none());
    }
}
