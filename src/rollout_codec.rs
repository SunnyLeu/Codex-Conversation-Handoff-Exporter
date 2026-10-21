//! Zstandard rollout decoding support (format only). Never mutates Codex's source files.
//!
//! The `zstd` Cargo dependency delegates streaming decompression to zstd-safe,
//! with the native Zstandard library built statically from zstd-sys sources.
use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use zstd::zstd_safe::{self, DCtx, InBuffer, OutBuffer};

const MAX_ROLLOUT_UNCOMPRESSED: usize = 2 * 1024 * 1024 * 1024;
const DECODE_BUFFER_SIZE: usize = 128 * 1024;

fn decode_error(code: usize, path: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "Zstandard decode error in {}: {}",
        path.display(),
        zstd_safe::get_error_name(code)
    )
}

pub(crate) fn is_compressed_rollout(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".jsonl.zst"))
}

fn decode_zstd(compressed: &[u8], path: &Path) -> Result<Vec<u8>> {
    decode_zstd_with_limit(compressed, path, MAX_ROLLOUT_UNCOMPRESSED)
}

fn decode_zstd_with_limit(compressed: &[u8], path: &Path, max_output: usize) -> Result<Vec<u8>> {
    if compressed.is_empty() {
        bail!("Empty Zstandard rollout: {}", path.display());
    }

    let mut decoder = DCtx::try_create().ok_or_else(|| {
        anyhow::anyhow!(
            "Unable to initialize Zstandard decoder for {}",
            path.display()
        )
    })?;
    decoder.init().map_err(|code| decode_error(code, path))?;

    let mut input = InBuffer::around(compressed);
    let mut scratch = vec![0u8; DECODE_BUFFER_SIZE];
    let mut result = Vec::new();
    loop {
        let before_input = input.pos();
        let mut output = OutBuffer::around(&mut scratch[..]);
        let remaining = decoder
            .decompress_stream(&mut output, &mut input)
            .map_err(|code| decode_error(code, path))?;
        let written = output.pos();
        // Enforce the same 2 GiB maximum as the original decoder before append.
        if written > max_output.saturating_sub(result.len()) {
            bail!(
                "Zstandard rollout exceeds safety limit of {} bytes: {}",
                max_output,
                path.display()
            );
        }
        result.extend_from_slice(&scratch[..written]);
        if remaining == 0 && input.pos() == compressed.len() {
            return Ok(result);
        }
        if input.pos() == before_input && written == 0 {
            bail!("Truncated or stalled Zstandard rollout: {}", path.display());
        }
    }
}

/// A read-only fixed snapshot; compressed content is validated to the end of
/// its frame(s), so late corruption cannot be silently treated as valid JSON.
pub(crate) fn read_rollout_snapshot(path: &Path) -> Result<Vec<u8>> {
    let file = File::open(path).with_context(|| format!("open rollout {}", path.display()))?;
    let snapshot_len = file.metadata()?.len();
    let mut stored = Vec::with_capacity(
        usize::try_from(snapshot_len)
            .unwrap_or(16 * 1024 * 1024)
            .min(16 * 1024 * 1024),
    );
    file.take(snapshot_len)
        .read_to_end(&mut stored)
        .with_context(|| format!("read rollout snapshot {}", path.display()))?;
    if is_compressed_rollout(path) {
        decode_zstd(&stored, path)
    } else {
        Ok(stored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zstd_valid_and_corrupt_frame() {
        let sample = include_bytes!("../fixtures/sample-rollout.jsonl.zst");
        let name = Path::new("rollout-sample.jsonl.zst");
        assert_eq!(
            decode_zstd(sample, name).unwrap(),
            b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"test\"}}\n"
        );
        assert!(decode_zstd(&sample[..sample.len() - 2], name).is_err());
        assert!(decode_zstd(b"invalid", name).is_err());
        assert!(decode_zstd(b"", name).is_err());
    }

    #[test]
    fn zstd_concatenated_frames_are_fully_decoded() {
        let sample = include_bytes!("../fixtures/sample-rollout.jsonl.zst");
        let combined = [sample.as_slice(), sample.as_slice()].concat();
        let name = Path::new("rollout-double.jsonl.zst");
        let expected = b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"test\"}}\n";
        assert_eq!(decode_zstd(&combined, name).unwrap(), expected.repeat(2));

        // A truncated second frame must not be hidden by a valid first frame.
        assert!(decode_zstd(&combined[..combined.len() - 2], name).is_err());
        assert!(decode_zstd(&[sample.as_slice(), b"garbage"].concat(), name).is_err());
    }

    #[test]
    fn zstd_limit_is_enforced_before_returning_output() {
        let sample = include_bytes!("../fixtures/sample-rollout.jsonl.zst");
        let name = Path::new("rollout-limit.jsonl.zst");
        let expected = b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"test\"}}\n";
        assert_eq!(
            decode_zstd_with_limit(sample, name, expected.len()).unwrap(),
            expected
        );
        assert!(decode_zstd_with_limit(sample, name, expected.len() - 1).is_err());
    }
}
