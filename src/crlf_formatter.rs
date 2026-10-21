//! Pretty JSON whitespace emitted as CRLF at its source.
//!
//! String escaping, number rendering and key order stay in serde_json. Unlike
//! the legacy CRLF writer, this formatter never rescans already escaped string
//! fragments. Indentation is copied in bounded chunks instead of two bytes per
//! nesting level. It keeps the PrettyFormatter empty-container conventions.

use serde::Serialize;
use serde_json::{Value, ser::Formatter};
use std::io::{self, Write};

#[derive(Default)]
struct CrlfPrettyFormatter {
    depth: usize,
    has_value: bool,
}

impl CrlfPrettyFormatter {
    #[inline]
    fn indentation<W: ?Sized + Write>(&self, writer: &mut W) -> io::Result<()> {
        const SPACES: [u8; 128] = [b' '; 128];
        let mut remaining = self.depth * 2;
        while remaining > SPACES.len() {
            writer.write_all(&SPACES)?;
            remaining -= SPACES.len();
        }
        writer.write_all(&SPACES[..remaining])
    }

    #[inline]
    fn begin_container<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        delimiter: &[u8],
    ) -> io::Result<()> {
        self.depth += 1;
        self.has_value = false;
        writer.write_all(delimiter)
    }

    #[inline]
    fn end_container<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        delimiter: &[u8],
    ) -> io::Result<()> {
        self.depth -= 1;
        if self.has_value {
            writer.write_all(b"\r\n")?;
            self.indentation(writer)?;
        }
        writer.write_all(delimiter)
    }

    #[inline]
    fn begin_entry<W: ?Sized + Write>(&self, writer: &mut W, first: bool) -> io::Result<()> {
        writer.write_all(if first { b"\r\n" } else { b",\r\n" })?;
        self.indentation(writer)
    }
}

impl Formatter for CrlfPrettyFormatter {
    #[inline]
    fn begin_array<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.begin_container(writer, b"[")
    }

    #[inline]
    fn end_array<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.end_container(writer, b"]")
    }

    #[inline]
    fn begin_array_value<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.begin_entry(writer, first)
    }

    #[inline]
    fn end_array_value<W: ?Sized + Write>(&mut self, _writer: &mut W) -> io::Result<()> {
        self.has_value = true;
        Ok(())
    }

    #[inline]
    fn begin_object<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.begin_container(writer, b"{")
    }

    #[inline]
    fn end_object<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.end_container(writer, b"}")
    }

    #[inline]
    fn begin_object_key<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.begin_entry(writer, first)
    }

    #[inline]
    fn begin_object_value<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        writer.write_all(b": ")
    }

    #[inline]
    fn end_object_value<W: ?Sized + Write>(&mut self, _writer: &mut W) -> io::Result<()> {
        self.has_value = true;
        Ok(())
    }
}

/// Same bytes as serde_json pretty JSON with two-space indentation, structural
/// LF converted to CRLF, and exactly one terminal CRLF. Only an owned JSON Value
/// is accepted; no unvalidated raw JSON fragments are injected into the stream.
pub(super) fn to_vec_pretty(handoff: &Value) -> serde_json::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let formatter = CrlfPrettyFormatter::default();
    handoff.serialize(&mut serde_json::Serializer::with_formatter(
        &mut bytes, formatter,
    ))?;
    bytes.extend_from_slice(b"\r\n");
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Map, Number, json};

    fn verify(value: &Value) {
        let pretty = serde_json::to_vec_pretty(value).unwrap();
        let mut expected = Vec::new();
        for b in pretty {
            if b == b'\n' {
                expected.push(b'\r');
            }
            expected.push(b);
        }
        expected.extend_from_slice(b"\r\n");
        let actual = to_vec_pretty(value).unwrap();
        assert_eq!(actual, expected);
        let parsed: Value = serde_json::from_slice(&actual).unwrap();
        // serde_json's float reader has its own rounding policy; exact serializer
        // bytes are the primary assertion. Values without floats also roundtrip.
        assert_eq!(
            serde_json::to_vec_pretty(&parsed).unwrap(),
            serde_json::to_vec_pretty(&serde_json::from_slice::<Value>(&expected).unwrap())
                .unwrap()
        );
        assert!(actual.ends_with(b"\r\n"));
        assert!(!actual.ends_with(b"\r\n\r\n"));
        for (i, b) in actual.iter().enumerate() {
            if *b == b'\n' {
                assert!(i > 0 && actual[i - 1] == b'\r');
            }
        }
    }

    #[test]
    fn scalar_types_and_numeric_edges_match_serde_pretty() {
        for value in [
            Value::Null,
            json!(true),
            json!(false),
            json!(""),
            json!(i64::MIN),
            json!(u64::MAX),
            json!(0),
            Value::Number(Number::from_f64(-0.0).unwrap()),
            Value::Number(Number::from_f64(1e-300).unwrap()),
            Value::Number(Number::from_f64(1e300).unwrap()),
        ] {
            verify(&value);
        }
    }

    #[test]
    fn all_controls_quotes_backslashes_and_unicode_match() {
        let controls: String = (0..=0x1f).map(char::from).collect();
        verify(
            &json!({"\"\\\r\n\t臺灣📝": [controls, "a\r\nb\nc\rd", "\\n is literal", "\u{2028}\u{2029}"]}),
        );
    }

    #[test]
    fn nested_empty_containers_do_not_leak_has_value_state() {
        verify(&json!([[], {}, [[], {}], {"a":[], "b":{}, "c":[1,[],{}]}, [], {"d":[]} ]));
        verify(&json!({"a":{"b":{}}, "c":[], "d":[{},[],{"e":[[]]}]}));
    }

    #[test]
    fn insertion_order_is_preserved() {
        let mut object = Map::new();
        for name in ["z", "a", "臺灣", "\n", "0", "a/b~c"] {
            object.insert(name.into(), json!({"second":2,"first":1}));
        }
        verify(&Value::Object(object));
    }

    #[test]
    fn long_escaped_and_unescaped_strings_match() {
        verify(
            &json!({"text": "繁體中文/normal/📝".repeat(32768), "escaped": "\n\r\t\"\\".repeat(16384)}),
        );
    }

    #[test]
    fn indentation_larger_than_one_chunk_matches() {
        let mut value = json!({"leaf":[1, {}, []]});
        for _ in 0..100 {
            value = json!({"nested":value});
        }
        // The serializer has no reader recursion limit; compare bytes directly.
        let mut expected = serde_json::to_string_pretty(&value)
            .unwrap()
            .replace('\n', "\r\n")
            .into_bytes();
        expected.extend_from_slice(b"\r\n");
        assert_eq!(to_vec_pretty(&value).unwrap(), expected);
    }

    #[test]
    fn deterministic_mixed_trees_match_reference() {
        fn value(seed: u64, depth: u32) -> Value {
            if depth == 0 {
                return json!(format!("{seed}:\n臺灣\"\\"));
            }
            match seed % 5 {
                0 => json!([]),
                1 => json!({}),
                2 => json!([
                    value(
                        seed.wrapping_mul(6364136223846793005).wrapping_add(1),
                        depth - 1
                    ),
                    Value::Null,
                    value(seed + 11, depth - 1)
                ]),
                3 => json!({"z":value(seed+7,depth-1), "a":value(seed+2,depth-1)}),
                _ => json!([seed, seed % 2 == 0, "\t\r\n"]),
            }
        }
        for seed in 0..1000 {
            verify(&value(seed, 5));
        }
    }

    #[test]
    fn formatter_propagates_writer_failure() {
        struct Fails;
        impl Write for Fails {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("fixture failure"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let error = json!({"a":[1]})
            .serialize(&mut serde_json::Serializer::with_formatter(
                Fails,
                CrlfPrettyFormatter::default(),
            ))
            .unwrap_err();
        assert!(error.to_string().contains("fixture failure"));
    }
}
