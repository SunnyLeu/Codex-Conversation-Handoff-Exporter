//! Allocation-free, inclusive JSONL physical-line boundaries.
//!
//! memchr selects its supported byte-search implementation at runtime. We keep
//! every byte and yield exactly the same slices as split_inclusive on LF,
//! including CRLF, blank lines and an unterminated final physical line.

pub(super) struct JsonlSegments<'a> {
    bytes: &'a [u8],
    ends: memchr::Memchr<'a>,
    start: usize,
}

impl<'a> JsonlSegments<'a> {
    pub(super) fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            ends: memchr::memchr_iter(b'\n', bytes),
            start: 0,
        }
    }
}

impl<'a> Iterator for JsonlSegments<'a> {
    type Item = &'a [u8];

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.start == self.bytes.len() {
            return None;
        }
        let end = self
            .ends
            .next()
            .map_or(self.bytes.len(), |position| position + 1);
        let segment = &self.bytes[self.start..end];
        self.start = end;
        Some(segment)
    }
}

impl std::iter::FusedIterator for JsonlSegments<'_> {}

#[cfg(test)]
mod tests {
    use super::*;

    fn verify(bytes: &[u8]) {
        let expected: Vec<_> = bytes.split_inclusive(|byte| *byte == b'\n').collect();
        let actual: Vec<_> = JsonlSegments::new(bytes).collect();
        assert_eq!(actual, expected);
        assert_eq!(actual.concat(), bytes);
        let mut iterator = JsonlSegments::new(bytes);
        for _ in &actual {
            assert!(iterator.next().is_some());
        }
        assert!(iterator.next().is_none());
        assert!(iterator.next().is_none());
    }

    #[test]
    fn physical_line_edge_cases_match_scalar() {
        for bytes in [
            b"".as_slice(),
            b"\n",
            b"\r",
            b"\r\n",
            b"\n\n",
            b"\r\n\n\r\n",
            b"{}",
            b"{}\n",
            b"{}\r\n",
            b"{}\n{\"partial",
            b"{}\n{\"partial\n",
            b" \t\r\n{}\n\xff",
            b"\\n\r\n",
        ] {
            verify(bytes);
        }
    }

    #[test]
    fn exhaustive_short_byte_sequences_match_scalar() {
        const ALPHABET: [u8; 6] = [b'\n', b'\r', b' ', b'"', 0, 0xff];
        for length in 0..=6 {
            for mut code in 0..6usize.pow(length) {
                let mut bytes = vec![0; length as usize];
                for byte in &mut bytes {
                    *byte = ALPHABET[code % 6];
                    code /= 6;
                }
                verify(&bytes);
            }
        }
    }

    #[test]
    fn vector_boundaries_and_long_tail_match_scalar() {
        for len in [
            15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 1023, 1024, 1025,
        ] {
            for position in 0..len {
                let mut bytes = vec![b'x'; len];
                bytes[position] = b'\n';
                verify(&bytes);
            }
        }
        let mut bytes = vec![b'x'; 4 * 1024 * 1024 + 3];
        for offset in [0, 31, 32, 63, 64, 1_048_575, 2_097_152, 4_194_305] {
            bytes[offset] = b'\n';
        }
        verify(&bytes);
    }
}
