//! Forward error correction for video frames.
//!
//! A frame is cut into fixed-size shards, grouped into blocks of at most `MAX_K` data shards,
//! and each block gets Reed-Solomon parity shards. Any `k` of the `k + m` shards of a block
//! rebuild it, so lost Wi-Fi packets are repaired without a round trip.

use crate::proto::VideoHdr;
use reed_solomon_erasure::galois_8::ReedSolomon;
use std::collections::{BTreeMap, HashMap};

pub const SHARD: usize = 1180;
const MAX_K: usize = 64;
const FEC_PERCENT: usize = 20;

/// (total data shards, blocks, data shards per full block)
pub fn layout(len: usize) -> (usize, usize, usize) {
    let shards = len.div_ceil(SHARD).max(1);
    let blocks = shards.div_ceil(MAX_K);
    (shards, blocks, shards.div_ceil(blocks))
}

pub fn parity_for(k: usize) -> usize {
    (k * FEC_PERCENT).div_ceil(100).max(1)
}

#[derive(Default)]
pub struct RsCache(HashMap<(usize, usize), ReedSolomon>);

impl RsCache {
    fn get(&mut self, k: usize, m: usize) -> &ReedSolomon {
        self.0.entry((k, m)).or_insert_with(|| ReedSolomon::new(k, m).expect("rs params"))
    }
}

/// Splits a frame into shards. `emit(block, nblocks, shard_index, k, m, shard_bytes)`.
pub fn encode(rs: &mut RsCache, frame: &[u8], mut emit: impl FnMut(u16, u16, u16, u16, u16, &[u8])) {
    let (total, nblocks, k_full) = layout(frame.len());
    let mut shards: Vec<Vec<u8>> = Vec::new();
    for b in 0..nblocks {
        let first = b * k_full;
        let k = k_full.min(total - first);
        let m = parity_for(k);
        shards.clear();
        for i in 0..k + m {
            let mut s = vec![0u8; SHARD];
            if i < k {
                let off = (first + i) * SHARD;
                let end = (off + SHARD).min(frame.len());
                if off < end {
                    s[..end - off].copy_from_slice(&frame[off..end]);
                }
            }
            shards.push(s);
        }
        rs.get(k, m).encode(&mut shards).expect("rs encode");
        for (i, s) in shards.iter().enumerate() {
            emit(b as u16, nblocks as u16, i as u16, k as u16, m as u16, s);
        }
    }
}

struct Block {
    shards: Vec<Option<Vec<u8>>>,
    k: usize,
    got: usize,
    done: bool,
}

struct Partial {
    first_seen: std::time::Instant,
    data: Vec<u8>,
    blocks: Vec<Option<Block>>,
    done_blocks: usize,
    ts: u64,
    flags: u8,
}

pub struct Complete {
    pub id: u32,
    pub data: Vec<u8>,
    pub ts: u64,
    pub key: bool,
}

#[derive(Default)]
pub struct Assembler {
    frames: BTreeMap<u32, Partial>,
    rs: RsCache,
    pub recovered_shards: u64,
}

impl Assembler {
    pub fn push(&mut self, h: &VideoHdr, payload: &[u8]) -> Option<Complete> {
        let len = h.frame_len as usize;
        if payload.len() != SHARD || len == 0 || len > 64 << 20 {
            return None;
        }
        let (total, nblocks, k_full) = layout(len);
        let (b, k, m, idx) = (h.block as usize, h.k as usize, h.m as usize, h.shard as usize);
        if nblocks != h.nblocks as usize || b >= nblocks || k != k_full.min(total - b * k_full) || idx >= k + m {
            return None;
        }
        let p = self.frames.entry(h.frame).or_insert_with(|| Partial {
            first_seen: std::time::Instant::now(),
            data: vec![0u8; total * SHARD],
            blocks: (0..nblocks).map(|_| None).collect(),
            done_blocks: 0,
            ts: h.ts,
            flags: h.flags,
        });
        if p.blocks.len() != nblocks {
            return None;
        }
        let blk = p.blocks[b].get_or_insert_with(|| Block { shards: vec![None; k + m], k, got: 0, done: false });
        if blk.done || blk.shards.len() != k + m || blk.shards[idx].is_some() {
            return None;
        }
        blk.shards[idx] = Some(payload.to_vec());
        blk.got += 1;
        if blk.got < blk.k {
            return None;
        }
        // Block complete: rebuild missing data shards if needed, then copy into the frame buffer.
        let missing = blk.shards[..k].iter().filter(|s| s.is_none()).count();
        if missing > 0 {
            if self.rs.get(k, m).reconstruct_data(&mut blk.shards).is_err() {
                return None;
            }
            self.recovered_shards += missing as u64;
        }
        let base = b * k_full * SHARD;
        for (i, s) in blk.shards[..k].iter().enumerate() {
            let s = s.as_ref().unwrap();
            p.data[base + i * SHARD..base + (i + 1) * SHARD].copy_from_slice(s);
        }
        blk.done = true;
        blk.shards = Vec::new();
        p.done_blocks += 1;
        if p.done_blocks < nblocks {
            return None;
        }
        let mut p = self.frames.remove(&h.frame)?;
        p.data.truncate(len);
        Some(Complete { id: h.frame, data: p.data, ts: p.ts, key: p.flags & crate::proto::FLAG_KEY != 0 })
    }

    /// Drops partial frames with id <= `id`.
    pub fn discard_upto(&mut self, id: u32) {
        self.frames = self.frames.split_off(&(id.wrapping_add(1)));
    }

    /// Id of the newest partial frame that has been incomplete for longer than `age`.
    pub fn stale(&self, age: std::time::Duration) -> Option<u32> {
        self.frames.iter().filter(|(_, p)| p.first_seen.elapsed() > age).map(|(id, _)| *id).last()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(len: usize, drop_every: usize) -> Option<Vec<u8>> {
        let frame: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
        let mut rs = RsCache::default();
        let mut pkts = Vec::new();
        encode(&mut rs, &frame, |block, nblocks, shard, k, m, s| {
            pkts.push((VideoHdr { frame: 1, block, nblocks, shard, k, m, frame_len: len as u32, ..Default::default() }, s.to_vec()))
        });
        let mut asm = Assembler::default();
        let mut out = None;
        for (i, (h, s)) in pkts.iter().enumerate() {
            if drop_every > 0 && i % drop_every == 0 {
                continue;
            }
            if let Some(c) = asm.push(h, s) {
                out = Some(c.data);
            }
        }
        assert!(out.as_ref().map_or(true, |o| *o == frame));
        out
    }

    #[test]
    fn small_and_large_frames() {
        assert!(roundtrip(10, 0).is_some());
        assert!(roundtrip(SHARD * 300 + 17, 0).is_some());
    }

    #[test]
    fn recovers_from_loss() {
        // Dropping 1 in 8 packets stays below the 20% parity budget of every block.
        assert!(roundtrip(SHARD * 200 + 5, 8).is_some());
        // Dropping half is unrecoverable.
        assert!(roundtrip(SHARD * 50, 2).is_none());
    }
}
