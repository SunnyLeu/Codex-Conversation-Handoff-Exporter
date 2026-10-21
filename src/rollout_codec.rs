//! Zstandard rollout decoding support (format only). Never mutates Codex's source files.
//!
//! Bundled upstream Zstandard v1.5.7 decoder is statically linked into the EXE.
use anyhow::{Context, Result, bail};
use std::ffi::{CStr, c_char, c_void};
use std::fs::File;
use std::io::Read;
use std::path::Path;

const MAX_ROLLOUT_UNCOMPRESSED: usize = 2 * 1024 * 1024 * 1024;
const DECODE_BUFFER_SIZE: usize = 128 * 1024;

#[repr(C)]
struct ZstdInBuffer {
    src: *const c_void,
    size: usize,
    pos: usize,
}
#[repr(C)]
struct ZstdOutBuffer {
    dst: *mut c_void,
    size: usize,
    pos: usize,
}

// Statically link the vetted vendored upstream decoder on all platforms.
#[link(name = "zstd", kind = "static")]
unsafe extern "C" {
    fn ZSTD_createDStream() -> *mut c_void;
    fn ZSTD_freeDStream(stream: *mut c_void) -> usize;
    fn ZSTD_initDStream(stream: *mut c_void) -> usize;
    fn ZSTD_decompressStream(
        stream: *mut c_void,
        output: *mut ZstdOutBuffer,
        input: *mut ZstdInBuffer,
    ) -> usize;
    fn ZSTD_isError(code: usize) -> u32;
    fn ZSTD_getErrorName(code: usize) -> *const c_char;
}

struct Decoder(*mut c_void);
impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: the pointer was obtained from ZSTD_createDStream and is freed once.
        unsafe { ZSTD_freeDStream(self.0) };
    }
}

fn check_code(code: usize, path: &Path) -> Result<usize> {
    // SAFETY: these libzstd APIs are pure queries over their scalar return code.
    if unsafe { ZSTD_isError(code) } != 0 {
        let detail = unsafe {
            let p = ZSTD_getErrorName(code);
            if p.is_null() {
                "unknown Zstandard error".to_owned()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        bail!("Zstandard decode error in {}: {detail}", path.display());
    }
    Ok(code)
}

pub(crate) fn is_compressed_rollout(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".jsonl.zst"))
}

fn decode_zstd(compressed: &[u8], path: &Path) -> Result<Vec<u8>> {
    if compressed.is_empty() {
        bail!("Empty Zstandard rollout: {}", path.display());
    }
    // SAFETY: the libzstd constructor requires no parameters; a null result
    // is handled before the pointer is used.
    let ptr = unsafe { ZSTD_createDStream() };
    if ptr.is_null() {
        bail!(
            "Unable to initialize Zstandard decoder for {}",
            path.display()
        );
    }
    let decoder = Decoder(ptr);
    check_code(unsafe { ZSTD_initDStream(decoder.0) }, path)?;
    let mut input = ZstdInBuffer {
        src: compressed.as_ptr().cast(),
        size: compressed.len(),
        pos: 0,
    };
    let mut scratch = vec![0u8; DECODE_BUFFER_SIZE];
    let mut result = Vec::new();
    loop {
        let before_input = input.pos;
        let mut output = ZstdOutBuffer {
            dst: scratch.as_mut_ptr().cast(),
            size: scratch.len(),
            pos: 0,
        };
        // SAFETY: the decoder is valid and exclusively owned, and the in/out
        // buffers point to live, non-overlapping slices for this entire call.
        let remaining = check_code(
            unsafe { ZSTD_decompressStream(decoder.0, &mut output, &mut input) },
            path,
        )?;
        if output.pos > MAX_ROLLOUT_UNCOMPRESSED.saturating_sub(result.len()) {
            bail!(
                "Zstandard rollout exceeds safety limit of {} bytes: {}",
                MAX_ROLLOUT_UNCOMPRESSED,
                path.display()
            );
        }
        result.extend_from_slice(&scratch[..output.pos]);
        if remaining == 0 && input.pos == input.size {
            return Ok(result);
        }
        if input.pos == before_input && output.pos == 0 {
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
}
