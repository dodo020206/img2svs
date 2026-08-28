use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

pub struct Reader {
    file: File,
    len: u64,
}

impl Reader {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let len = file.metadata()?.len();
        Ok(Self { file, len })
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
        Ok(self.bytes(1, "u8")?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(
            self.bytes(2, "u16")?.try_into().unwrap(),
        ))
    }

    pub fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(
            self.bytes(4, "i32")?.try_into().unwrap(),
        ))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.bytes(4, "u32")?.try_into().unwrap(),
        ))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(
            self.bytes(8, "u64")?.try_into().unwrap(),
        ))
    }

    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(
            self.bytes(4, "f32")?.try_into().unwrap(),
        ))
    }

    pub fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_le_bytes(
            self.bytes(8, "f64")?.try_into().unwrap(),
        ))
    }
}
