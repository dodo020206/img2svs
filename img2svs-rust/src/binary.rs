//! Buffered little-endian reader shared by every vendor container parser.
//!
//! Container layouts are mostly fixed offsets, so the readers combine streaming
//! scalar reads with explicit [`Reader::seek`] / [`Reader::range`] access. Every
//! read takes a short `context` label that ends up in the error message, which
//! is what makes a truncated or corrupt container diagnosable.

use crate::model::ByteRange;
use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};

/// Size of the read-ahead buffer kept in front of the file handle.
const READ_BUFFER_SIZE: usize = 256 * 1024;

/// A scalar that can be read from a fixed-width little-endian slice.
///
/// Implemented by the integer widths the container indexes use, so a reader
/// can ask for `le_at::<u32>` without spelling out the width.
pub trait LittleEndian: Sized {
    /// Width in bytes.
    const WIDTH: usize;
    /// Decodes `slice`, which is guaranteed to be exactly [`Self::WIDTH`] long.
    fn from_le_bytes(slice: &[u8]) -> Self;
}

macro_rules! impl_little_endian {
    ($($type:ty),* $(,)?) => {
        $(impl LittleEndian for $type {
            const WIDTH: usize = std::mem::size_of::<$type>();
            fn from_le_bytes(slice: &[u8]) -> Self {
                let bytes: [u8; std::mem::size_of::<$type>()] =
                    slice.try_into().expect("slice length is checked by le_at");
                <$type>::from_le_bytes(bytes)
            }
        })*
    };
}
impl_little_endian!(u16, u32, u64, i32, i64);

/// Reads a little-endian `T` at `offset` of an in-memory buffer.
///
/// Readers that keep a whole index table in memory use this where [`Reader`]
/// is used for a file. `context` names the structure, so a truncated table
/// reports which one ran out.
pub fn le_at<T: LittleEndian>(data: &[u8], offset: usize, context: &str) -> Result<T> {
    let slice = data
        .get(offset..offset + T::WIDTH)
        .with_context(|| format!("truncated {context}"))?;
    Ok(T::from_le_bytes(slice))
}

/// A buffered file handle with the scalar and random-access helpers the
/// container parsers need.
pub struct Reader {
    file: BufReader<File>,
    len: u64,
}

impl Reader {
    /// Opens `path` and remembers its size for range validation.
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let len = file.metadata()?.len();
        Ok(Self {
            file: BufReader::with_capacity(READ_BUFFER_SIZE, file),
            len,
        })
    }

    /// Total size of the underlying file in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Positions the cursor at an absolute byte `offset`.
    pub fn seek(&mut self, offset: u64) -> Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        Ok(())
    }

    /// Reads exactly `count` bytes at the cursor.
    pub fn bytes(&mut self, count: usize, context: &str) -> Result<Vec<u8>> {
        let mut data = vec![0; count];
        self.read_exact(&mut data, context)?;
        Ok(data)
    }

    /// Advances the cursor over `count` bytes that carry no usable data.
    ///
    /// Container headers interleave length-reserved fields we ignore; skipping
    /// them by name keeps the surrounding offsets readable. Reads through the
    /// buffer on purpose so that a truncated file still fails immediately.
    pub fn skip(&mut self, count: usize, context: &str) -> Result<()> {
        self.bytes(count, context).map(|_| ())
    }

    /// Reads `length` bytes located at absolute `offset`.
    ///
    /// Unlike [`Reader::bytes`] this rejects ranges that fall outside the file,
    /// so callers can use it directly on offsets taken from an index.
    pub fn range(&mut self, offset: u64, length: u64, context: &str) -> Result<Vec<u8>> {
        // Reuses the model's range test so a byte range means the same thing
        // whether it is read here or validated before being stored.
        let range = ByteRange { offset, length };
        if !range.fits(self.len) {
            bail!("invalid byte range for {context}: offset={offset}, length={length}");
        }
        self.seek(offset)?;
        self.bytes(
            usize::try_from(length).context("byte range is too large for this platform")?,
            context,
        )
    }

    /// Reads a little-endian `i64` at the cursor.
    pub fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.array("i64")?))
    }

    /// Reads one unsigned byte at the cursor.
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>("u8")?[0])
    }

    /// Reads a little-endian `u16` at the cursor.
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array("u16")?))
    }

    /// Reads a little-endian `i32` at the cursor.
    pub fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.array("i32")?))
    }

    /// Reads a little-endian `u32` at the cursor.
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array("u32")?))
    }

    /// Reads a little-endian `u64` at the cursor.
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array("u64")?))
    }

    /// Reads a little-endian `f32` at the cursor.
    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.array("f32")?))
    }

    /// Reads a little-endian `f64` at the cursor.
    pub fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_le_bytes(self.array("f64")?))
    }

    /// Reads a fixed-size array without allocating.
    fn array<const N: usize>(&mut self, context: &str) -> Result<[u8; N]> {
        let mut data = [0; N];
        self.read_exact(&mut data, context)?;
        Ok(data)
    }

    /// Fills `data`, labelling the read with `context` on failure.
    fn read_exact(&mut self, data: &mut [u8], context: &str) -> Result<()> {
        self.file
            .read_exact(data)
            .with_context(|| format!("read {context}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn buffered_reader_preserves_scalar_and_random_access_reads() -> Result<()> {
        let path =
            std::env::temp_dir().join(format!("img2svs-binary-reader-{}.bin", std::process::id()));
        let mut source = vec![0u8; 256 * 1024 + 32];
        source[0] = 7;
        source[1..3].copy_from_slice(&0x1234u16.to_le_bytes());
        source[3..7].copy_from_slice(&0x89abcdefu32.to_le_bytes());
        source[256 * 1024 + 8..256 * 1024 + 16]
            .copy_from_slice(&0x0123456789abcdefu64.to_le_bytes());
        fs::write(&path, &source)?;

        let mut reader = Reader::open(&path)?;
        assert_eq!(reader.u8()?, 7);
        assert_eq!(reader.u16()?, 0x1234);
        assert_eq!(reader.u32()?, 0x89abcdef);
        reader.seek(256 * 1024 + 8)?;
        assert_eq!(reader.u64()?, 0x0123456789abcdef);
        assert_eq!(reader.range(1, 2, "test range")?, [0x34, 0x12]);

        drop(reader);
        fs::remove_file(path)?;
        Ok(())
    }
}
