//! `MurmurHash3` (x86, 32-bit, seed 0), vendored so keys land on the same page as Mwmbl's
//! Python, which does `mmh3.hash(key, signed=False) % num_pages`.

const C1: u32 = 0xcc9e_2d51;
const C2: u32 = 0x1b87_3593;

fn mix_k(k: u32) -> u32 {
    k.wrapping_mul(C1).rotate_left(15).wrapping_mul(C2)
}

/// `MurmurHash3` `x86_32` with seed 0, as an unsigned integer.
#[must_use]
pub fn mmh3_32(data: &[u8]) -> u32 {
    let mut h: u32 = 0;
    let blocks = data.chunks_exact(4);
    let tail = blocks.remainder();
    for block in blocks {
        let mut bytes = [0u8; 4];
        bytes.copy_from_slice(block);
        h ^= mix_k(u32::from_le_bytes(bytes));
        h = h.rotate_left(13).wrapping_mul(5).wrapping_add(0xe654_6b64);
    }
    let mut k: u32 = 0;
    for (shift, byte) in tail.iter().enumerate() {
        k |= u32::from(*byte) << (8 * shift);
    }
    if !tail.is_empty() {
        h ^= mix_k(k);
    }
    // The length is taken modulo 2^32, as in the reference implementation.
    h ^= u32::try_from(data.len() & 0xffff_ffff).unwrap_or(0);
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^ (h >> 16)
}

/// The page a key lives on: Python's `mmh3.hash(key, signed=False) % num_pages`.
///
/// `num_pages` must be non-zero; zero maps every key to page 0 rather than dividing by zero.
#[must_use]
pub fn page_for_key(key: &str, num_pages: usize) -> usize {
    let hash = usize::try_from(mmh3_32(key.as_bytes())).unwrap_or(usize::MAX);
    hash.checked_rem(num_pages).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_values() {
        assert_eq!(mmh3_32(b""), 0);
        assert_eq!(mmh3_32(b"a"), 1_009_084_850);
        assert_eq!(mmh3_32(b"Hello, world!"), 0xc036_3e43);
    }

    #[test]
    fn zero_pages_does_not_divide_by_zero() {
        assert_eq!(page_for_key("a", 0), 0);
    }
}
