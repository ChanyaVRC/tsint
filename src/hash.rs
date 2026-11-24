//! Zero-dependency fast non-cryptographic hasher (FxHash) for internal VM tables.
//!
//! `tsint` maintains zero external crate dependencies (`std` only). Standard
//! `HashMap`/`HashSet` use SipHash-1-3, which has noticeable per-lookup overhead
//! for short JS property identifiers and raw pointer keys (`usize`).
//! `FxHasher` uses a single multiply-xor-rotate mixing step per word.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

const FX_SEED: u64 = 0x517c_c1b7_2722_0a95;

#[derive(Default, Clone)]
pub(crate) struct FxHasher {
    hash: u64,
}

impl FxHasher {
    #[inline]
    fn add_to_hash(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(FX_SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }

    #[inline]
    fn write(&mut self, mut bytes: &[u8]) {
        while bytes.len() >= 8 {
            let chunk: [u8; 8] = bytes[..8].try_into().unwrap();
            self.add_to_hash(u64::from_ne_bytes(chunk));
            bytes = &bytes[8..];
        }
        if bytes.len() >= 4 {
            let chunk: [u8; 4] = bytes[..4].try_into().unwrap();
            self.add_to_hash(u32::from_ne_bytes(chunk) as u64);
            bytes = &bytes[4..];
        }
        if bytes.len() >= 2 {
            let chunk: [u8; 2] = bytes[..2].try_into().unwrap();
            self.add_to_hash(u16::from_ne_bytes(chunk) as u64);
            bytes = &bytes[2..];
        }
        if let Some(&b) = bytes.first() {
            self.add_to_hash(b as u64);
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add_to_hash(i as u64);
    }

    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.add_to_hash(i as u64);
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add_to_hash(i as u64);
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add_to_hash(i);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add_to_hash(i as u64);
    }
}

pub(crate) type FastBuildHasher = BuildHasherDefault<FxHasher>;
pub(crate) type FastHashMap<K, V> = HashMap<K, V, FastBuildHasher>;
pub(crate) type FastHashSet<T> = HashSet<T, FastBuildHasher>;
