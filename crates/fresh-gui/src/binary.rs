//! Sniff files that must not be opened as text.
//!
//! A binary snapshot sent to the GPUI editor becomes an accessibility text
//! buffer. On Windows that buffer is too small for the system call UI
//! Automation makes (`0x8007007A`, `ERROR_INSUFFICIENT_BUFFER`), and the
//! launching shell fills with errors. Callers show a placeholder instead.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};

const SAMPLE: usize = 8192;

/// A path the editor refused because the bytes are not text.
#[derive(Debug)]
pub struct BinaryFile {
    pub path: std::path::PathBuf,
}

impl std::fmt::Display for BinaryFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "binary file: {}", self.path.display())
    }
}

impl std::error::Error for BinaryFile {}

/// True when `sample` should not be decoded as editor text.
///
/// NUL (UTF-16, executables, images) is enough. A run of other C0 controls
/// is too, so a file of NULs that were stripped still counts.
pub fn looks_binary(sample: &[u8]) -> bool {
    if sample.is_empty() {
        return false;
    }
    if sample.contains(&0) {
        return true;
    }
    let bad = sample
        .iter()
        .filter(|&&byte| matches!(byte, 0x01..=0x08 | 0x0b | 0x0c | 0x0e..=0x1f))
        .count();
    bad.saturating_mul(20) > sample.len()
}

/// Read the start of `path` and sniff it. Missing files are not binary.
pub fn is_binary_file(path: &Path) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut buf = vec![0u8; SAMPLE];
    let n = file
        .read(&mut buf)
        .with_context(|| format!("read {}", path.display()))?;
    Ok(looks_binary(&buf[..n]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_and_empty_are_not_binary() {
        assert!(!looks_binary(b""));
        assert!(!looks_binary(b"fn main() {}\n"));
        assert!(!looks_binary("héllo\n".as_bytes()));
        assert!(!looks_binary(b"\tline\r\n"));
    }

    #[test]
    fn nul_and_control_runs_are_binary() {
        assert!(looks_binary(b"MZ\x00\x01"));
        assert!(looks_binary(&[0xff, 0xfe, 0x41, 0x00]));
        let mut noisy = vec![0x01u8; 40];
        noisy.extend(std::iter::repeat_n(b'a', 40));
        assert!(looks_binary(&noisy));
    }
}
