use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};

pub struct Reader {
    file: BufReader<File>,
    len: u64,
}

impl Reader {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let len = file.metadata()?.len();
        Ok(Self {
            file: BufReader::with_capacity(256 * 1024, file),
            len,
        })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn seek(&mut self, offset: u64) -> Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        Ok(())
    }

    pub fn bytes(&mut self, count: usize, context: &str) -> Result<Vec<u8>> {
        let mut data = vec![0; count];
        self.file
            .read_exact(&mut data)
            .with_context(|| format!("read {context}"))?;
        Ok(data)
    }

    pub fn range(&mut self, offset: u64, length: u64, context: &str) -> Result<Vec<u8>> {
        if offset == 0 || length == 0 || offset >= self.len || length > self.len - offset {
            bail!("invalid byte range for {context}: offset={offset}, length={length}");
        }
        self.seek(offset)?;
        self.bytes(
            usize::try_from(length).context("byte range is too large for this platform")?,
            context,
        )
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>("u8")?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array("u16")?))
    }

    pub fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.array("i32")?))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array("u32")?))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array("u64")?))
    }

    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.array("f32")?))
    }

    pub fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_le_bytes(self.array("f64")?))
    }

    fn array<const N: usize>(&mut self, context: &str) -> Result<[u8; N]> {
        let mut data = [0; N];
        self.file
            .read_exact(&mut data)
            .with_context(|| format!("read {context}"))?;
        Ok(data)
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
