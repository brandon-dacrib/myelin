//! Minimal unsigned LEB128 varint encode/decode, used by the production state representation
//! (`crate::frames`, formerly bake-off candidate B) for its "delta-varint compressed"
//! appended/disposed lists (`PLAN.md` section 6.3).

/// Appends `v`'s LEB128 encoding to `out`.
pub fn write_uvarint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
}

/// Reads one LEB128-encoded value from the front of `input`, advancing it.
///
/// # Errors
/// Returns `Err(())` if `input` is exhausted before a terminating byte is found, or the value
/// would overflow a `u64`.
pub fn read_uvarint(input: &mut &[u8]) -> Result<u64, ()> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        let (&byte, rest) = input.split_first().ok_or(())?;
        *input = rest;
        if shift >= 64 {
            return Err(());
        }
        result |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_across_the_range() {
        for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            let mut buf = Vec::new();
            write_uvarint(&mut buf, v);
            let mut slice = buf.as_slice();
            assert_eq!(read_uvarint(&mut slice).unwrap(), v);
            assert!(slice.is_empty());
        }
    }

    #[test]
    fn small_values_are_more_compact_than_fixed_width() {
        let mut buf = Vec::new();
        write_uvarint(&mut buf, 5);
        assert_eq!(buf.len(), 1);
    }
}
