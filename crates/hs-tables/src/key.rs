//! Order-preserving tuple key encoding, in the style of FoundationDB's tuple layer: a Rust tuple
//! `(A, B, C, ...)` encodes to a byte string such that comparing two encoded keys byte-wise gives
//! the same answer as comparing the tuples component-wise — integers sort numerically, strings and
//! byte blobs sort lexicographically, and a shorter tuple that is a prefix of a longer one sorts
//! first.
//!
//! # Why fixed-width, not varint
//!
//! Integers are encoded as fixed-width big-endian (`u32` as 4 bytes, `u64` as 8, and so on), with
//! the sign bit flipped for signed types so two's-complement negatives still sort before
//! positives under unsigned byte comparison. A varint scheme (LEB128 or similar) does **not**
//! preserve order across a byte-length boundary without extra machinery (a length prefix, or
//! FoundationDB's own typed-varint scheme) — `300u32` as plain LEB128 encodes to two bytes that
//! sort *before* the one byte `1u32` produces, which breaks range scans. Fixed-width sidesteps this
//! entirely and matches `hs-model`'s short IDs, which already expose `to_be_bytes` /
//! `from_be_bytes` for exactly this reason. This was one of track 01's day-one open questions
//! (`docs/workstreams/01-storage-engine.md`); fixed-width is the decision, recorded here and in
//! `docs/status/01-storage-engine.md`.
//!
//! # Why strings escape rather than length-prefix
//!
//! A variable-length component in the middle of a tuple must be self-delimiting so the next
//! component's bytes are not mistaken for its continuation. A length prefix would do that, but
//! would not preserve order (a 10-byte string must sort after a 9-byte string with the same
//! prefix, which a leading length byte breaks). Instead, every `0x00` byte in the content is
//! escaped as `0x00 0xFF`, and the component is terminated with `0x00 0x00`: since `0xFF > 0x00`,
//! "more content follows" always sorts after "the component ended here", which is exactly the
//! prefix relationship strings need. See [`tests::string_order_matches_str_order`] for the
//! executable proof.

/// A component of a tuple key that knows how to write its order-preserving encoding.
pub trait KeyEncode {
    /// Appends this value's encoding to `out`.
    fn encode_component(&self, out: &mut Vec<u8>);
}

/// The inverse of [`KeyEncode`]: reads one component from the front of `input`, advancing it past
/// the bytes consumed.
pub trait KeyDecode: Sized {
    /// Decodes one component from the front of `input`.
    ///
    /// # Errors
    /// Returns [`KeyCodecError`] if `input` does not contain a validly encoded component.
    fn decode_component(input: &mut &[u8]) -> Result<Self, KeyCodecError>;
}

/// An error decoding a tuple key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum KeyCodecError {
    /// The input ended before a fixed-width component was fully read.
    #[error("unexpected end of key while decoding a fixed-width component")]
    UnexpectedEof,
    /// A string or blob terminator (`0x00 0x00`) was never found.
    #[error("unterminated string or blob component")]
    Unterminated,
    /// An escaped string or blob component contained a `0x00` byte not followed by `0x00` or
    /// `0xFF`.
    #[error("invalid escape sequence in a string or blob component")]
    InvalidEscape,
    /// A string component's bytes were not valid UTF-8.
    #[error("string component was not valid UTF-8")]
    InvalidUtf8,
    /// The key had bytes left over after decoding every declared component.
    #[error("trailing bytes after decoding a tuple key")]
    TrailingBytes,
}

fn encode_escaped(bytes: &[u8], out: &mut Vec<u8>) {
    out.reserve(bytes.len() + 2);
    for &byte in bytes {
        if byte == 0x00 {
            out.push(0x00);
            out.push(0xFF);
        } else {
            out.push(byte);
        }
    }
    out.push(0x00);
    out.push(0x00);
}

fn decode_escaped(input: &mut &[u8]) -> Result<Vec<u8>, KeyCodecError> {
    let mut buf = Vec::new();
    loop {
        match input.split_first() {
            None => return Err(KeyCodecError::Unterminated),
            Some((0x00, rest)) => match rest.split_first() {
                Some((0xFF, rest2)) => {
                    buf.push(0x00);
                    *input = rest2;
                }
                Some((0x00, rest2)) => {
                    *input = rest2;
                    return Ok(buf);
                }
                _ => return Err(KeyCodecError::InvalidEscape),
            },
            Some((&byte, rest)) => {
                buf.push(byte);
                *input = rest;
            }
        }
    }
}

impl KeyEncode for [u8] {
    fn encode_component(&self, out: &mut Vec<u8>) {
        encode_escaped(self, out);
    }
}

impl KeyEncode for Vec<u8> {
    fn encode_component(&self, out: &mut Vec<u8>) {
        encode_escaped(self, out);
    }
}

impl KeyDecode for Vec<u8> {
    fn decode_component(input: &mut &[u8]) -> Result<Self, KeyCodecError> {
        decode_escaped(input)
    }
}

impl KeyEncode for str {
    fn encode_component(&self, out: &mut Vec<u8>) {
        encode_escaped(self.as_bytes(), out);
    }
}

impl KeyEncode for String {
    fn encode_component(&self, out: &mut Vec<u8>) {
        self.as_str().encode_component(out);
    }
}

impl KeyDecode for String {
    fn decode_component(input: &mut &[u8]) -> Result<Self, KeyCodecError> {
        let bytes = decode_escaped(input)?;
        String::from_utf8(bytes).map_err(|_| KeyCodecError::InvalidUtf8)
    }
}

impl<T: KeyEncode + ?Sized> KeyEncode for &T {
    fn encode_component(&self, out: &mut Vec<u8>) {
        (**self).encode_component(out);
    }
}

macro_rules! impl_unsigned_fixed_width {
    ($($t:ty => $n:literal),+ $(,)?) => {
        $(
            impl KeyEncode for $t {
                fn encode_component(&self, out: &mut Vec<u8>) {
                    out.extend_from_slice(&self.to_be_bytes());
                }
            }

            impl KeyDecode for $t {
                fn decode_component(input: &mut &[u8]) -> Result<Self, KeyCodecError> {
                    if input.len() < $n {
                        return Err(KeyCodecError::UnexpectedEof);
                    }
                    let (head, rest) = input.split_at($n);
                    #[allow(clippy::unwrap_used, reason = "head is exactly $n bytes by construction")]
                    let arr: [u8; $n] = head.try_into().unwrap();
                    *input = rest;
                    Ok(<$t>::from_be_bytes(arr))
                }
            }
        )+
    };
}

impl_unsigned_fixed_width!(u8 => 1, u16 => 2, u32 => 4, u64 => 8);

macro_rules! impl_signed_fixed_width {
    ($($t:ty, $u:ty => $n:literal),+ $(,)?) => {
        $(
            impl KeyEncode for $t {
                fn encode_component(&self, out: &mut Vec<u8>) {
                    let flipped = (*self as $u) ^ (1 << ($n * 8 - 1));
                    out.extend_from_slice(&flipped.to_be_bytes());
                }
            }

            impl KeyDecode for $t {
                fn decode_component(input: &mut &[u8]) -> Result<Self, KeyCodecError> {
                    if input.len() < $n {
                        return Err(KeyCodecError::UnexpectedEof);
                    }
                    let (head, rest) = input.split_at($n);
                    #[allow(clippy::unwrap_used, reason = "head is exactly $n bytes by construction")]
                    let arr: [u8; $n] = head.try_into().unwrap();
                    *input = rest;
                    let flipped = <$u>::from_be_bytes(arr);
                    Ok((flipped ^ (1 << ($n * 8 - 1))) as $t)
                }
            }
        )+
    };
}

impl_signed_fixed_width!(i16, u16 => 2, i32, u32 => 4, i64, u64 => 8);

macro_rules! impl_short_id {
    ($($t:ty => $n:literal),+ $(,)?) => {
        $(
            impl KeyEncode for $t {
                fn encode_component(&self, out: &mut Vec<u8>) {
                    out.extend_from_slice(&self.to_be_bytes());
                }
            }

            impl KeyDecode for $t {
                fn decode_component(input: &mut &[u8]) -> Result<Self, KeyCodecError> {
                    if input.len() < $n {
                        return Err(KeyCodecError::UnexpectedEof);
                    }
                    let (head, rest) = input.split_at($n);
                    #[allow(clippy::unwrap_used, reason = "head is exactly $n bytes by construction")]
                    let arr: [u8; $n] = head.try_into().unwrap();
                    *input = rest;
                    Ok(<$t>::from_be_bytes(arr))
                }
            }
        )+
    };
}

impl_short_id!(
    hs_model::RoomSn => 4,
    hs_model::UserSn => 4,
    hs_model::ServerSn => 4,
    hs_model::StateKeyId => 4,
    hs_model::TypeId => 4,
    hs_model::EventSn => 8,
);

// Tuples implement `KeyEncode`/`KeyDecode` themselves, component by component, which makes them
// composable: a tuple can be a component of another tuple (`(IK, PK)` where `IK` and `PK` are
// themselves tuples is exactly `hs-tables`'s composite index key, below).
macro_rules! impl_tuple_codec {
    ($($idx:tt : $t:ident),+) => {
        impl<$($t: KeyEncode),+> KeyEncode for ($($t,)+) {
            fn encode_component(&self, out: &mut Vec<u8>) {
                $( self.$idx.encode_component(out); )+
            }
        }

        impl<$($t: KeyDecode),+> KeyDecode for ($($t,)+) {
            fn decode_component(input: &mut &[u8]) -> Result<Self, KeyCodecError> {
                Ok(( $( <$t as KeyDecode>::decode_component(input)?, )+ ))
            }
        }
    };
}

impl_tuple_codec!(0: A);
impl_tuple_codec!(0: A, 1: B);
impl_tuple_codec!(0: A, 1: B, 2: C);
impl_tuple_codec!(0: A, 1: B, 2: C, 3: D);
impl_tuple_codec!(0: A, 1: B, 2: C, 3: D, 4: E);
impl_tuple_codec!(0: A, 1: B, 2: C, 3: D, 4: E, 5: F);

/// A type that stands for one whole `hs-kv` key: a [`KeyEncode`] + [`KeyDecode`] type (in
/// practice, always a tuple, arity 1 through 6, possibly nesting another tuple as one of its
/// components) with convenience `encode` / `decode` methods that encode or decode the *entire*
/// key, rejecting leftover bytes `decode` did not consume — unlike [`KeyDecode::decode_component`],
/// which is meant to be followed by more components.
pub trait TupleKey: KeyEncode + KeyDecode {
    /// Encodes the whole key to bytes suitable for use as an `hs-kv` key.
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_component(&mut out);
        out
    }

    /// Decodes a whole key previously produced by [`TupleKey::encode`].
    ///
    /// # Errors
    /// Returns [`KeyCodecError`] if `bytes` is not a validly encoded key of this shape, or has
    /// bytes left over after decoding it.
    fn decode(bytes: &[u8]) -> Result<Self, KeyCodecError>
    where
        Self: Sized,
    {
        let mut input = bytes;
        let value = Self::decode_component(&mut input)?;
        if !input.is_empty() {
            return Err(KeyCodecError::TrailingBytes);
        }
        Ok(value)
    }
}

impl<T: KeyEncode + KeyDecode> TupleKey for T {}

/// Encodes any [`TupleKey`] in one call, for call sites that would otherwise write
/// `key.encode()` on a value constructed inline.
pub fn encode<K: TupleKey>(key: &K) -> Vec<u8> {
    key.encode()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn encode_str(s: &str) -> Vec<u8> {
        let mut out = Vec::new();
        s.encode_component(&mut out);
        out
    }

    #[test]
    fn u32_sorts_numerically_across_byte_length_boundaries() {
        // The whole point of fixed-width over varint: 255 and 256 must sort in numeric order
        // even though a naive varint would put the shorter encoding of 255 before 256's longer one
        // (or, depending on the scheme, do the opposite) rather than reflecting magnitude.
        let a = (255u32,).encode();
        let b = (256u32,).encode();
        assert!(a < b);
        assert_eq!((300u32,).encode() < (1000u32,).encode(), 300u32 < 1000u32);
    }

    #[test]
    fn i32_negatives_sort_before_positives() {
        let neg = (-1i32,).encode();
        let zero = (0i32,).encode();
        let pos = (1i32,).encode();
        assert!(neg < zero);
        assert!(zero < pos);
        assert!((i32::MIN,).encode() < (i32::MAX,).encode());
    }

    #[test]
    fn string_terminator_sorts_before_continuation() {
        // "ab" must sort before "abc": at the byte where "ab" terminates (0x00 0x00), "abc"
        // instead has 'c' (0x63), and 0x00 < 0x63.
        assert!(encode_str("ab") < encode_str("abc"));
        assert!(encode_str("") < encode_str("a"));
    }

    #[test]
    fn embedded_nul_round_trips_and_sorts_after_its_prefix() {
        let with_nul = "a\0b";
        let encoded = encode_str(with_nul);
        let mut input = encoded.as_slice();
        let decoded = String::decode_component(&mut input).unwrap();
        assert_eq!(decoded, with_nul);
        assert!(input.is_empty());

        // "a" is a proper prefix of "a\0b", so it must sort first.
        assert!(encode_str("a") < encode_str(with_nul));
    }

    #[test]
    fn tuple_round_trips() {
        let original = (7u32, "room".to_string(), -42i64);
        let bytes = original.encode();
        let decoded = <(u32, String, i64)>::decode(&bytes).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn tuple_decode_rejects_trailing_bytes() {
        let mut bytes = (1u32,).encode();
        bytes.push(0xAB);
        assert_eq!(<(u32,)>::decode(&bytes), Err(KeyCodecError::TrailingBytes));
    }

    #[test]
    fn short_id_encoding_matches_its_own_be_bytes() {
        let sn = hs_model::EventSn::new(0x0102_0304_0506_0708);
        assert_eq!((sn,).encode(), sn.to_be_bytes().to_vec());
    }

    #[test]
    fn a_shorter_tuple_prefix_sorts_before_a_longer_one() {
        // (1,) vs (1, 2): the first component is identical, and the shorter tuple has nothing
        // more, so it must sort first -- this is what lets a prefix scan over the first N
        // components of a composite key work.
        assert!((1u32,).encode() < (1u32, 2u32).encode());
    }

    proptest! {
        #[test]
        fn string_order_matches_str_order(a in ".*", b in ".*") {
            let encoded_order = encode_str(&a).cmp(&encode_str(&b));
            let str_order = a.cmp(&b);
            prop_assert_eq!(encoded_order, str_order);
        }

        #[test]
        fn u64_order_matches_numeric_order(a: u64, b: u64) {
            let encoded_order = (a,).encode().cmp(&(b,).encode());
            prop_assert_eq!(encoded_order, a.cmp(&b));
        }

        #[test]
        fn i64_order_matches_numeric_order(a: i64, b: i64) {
            let encoded_order = (a,).encode().cmp(&(b,).encode());
            prop_assert_eq!(encoded_order, a.cmp(&b));
        }

        #[test]
        fn tuple_round_trip_holds_for_arbitrary_values(a: u32, b: i64, s in ".*") {
            let original = (a, s.clone(), b);
            let bytes = original.encode();
            let decoded = <(u32, String, i64)>::decode(&bytes).unwrap();
            prop_assert_eq!(decoded, (a, s, b));
        }
    }
}
