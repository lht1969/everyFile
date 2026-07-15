use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

const DEFAULT_BUF_SIZE: usize = 4 * 1024 * 1024;

pub struct AlignedReader {
    inner: File,
    sector_size: usize,
    buffer: Vec<u8>,
    buf_valid: usize,
    pos: u64,
    buf_start: u64,
    file_size: u64,
}

impl AlignedReader {
    pub fn new(file: File, sector_size: usize) -> io::Result<Self> {
        assert!(sector_size > 0 && sector_size.is_power_of_two());
        let file_size = file.metadata().map(|m| m.len()).unwrap_or(0);
        let buf_size = (DEFAULT_BUF_SIZE / sector_size) * sector_size;
        Ok(Self {
            inner: file,
            sector_size,
            buffer: vec![0u8; buf_size],
            buf_valid: 0,
            pos: 0,
            buf_start: 0,
            file_size,
        })
    }

    fn buf_end(&self) -> u64 {
        self.buf_start + self.buf_valid as u64
    }

    fn fill_buffer(&mut self, aligned_start: u64) -> io::Result<()> {
        self.inner.seek(SeekFrom::Start(aligned_start))?;
        let cap = self.buffer.len();
        let mut total = 0usize;
        while total < cap {
            match self.inner.read(&mut self.buffer[total..]) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        self.buf_start = aligned_start;
        self.buf_valid = total;
        Ok(())
    }

    fn ensure_readable(&mut self, len: usize) -> io::Result<()> {
        if self.buf_valid > 0
            && self.pos >= self.buf_start
            && self.pos + len as u64 <= self.buf_end()
        {
            return Ok(());
        }
        let sector_mask = !(self.sector_size as u64 - 1);
        let aligned = self.pos & sector_mask;
        self.fill_buffer(aligned)
    }
}

impl Read for AlignedReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.ensure_readable(buf.len())?;
        if self.pos >= self.buf_end() {
            return Ok(0);
        }
        let offset = (self.pos - self.buf_start) as usize;
        let available = self.buf_valid - offset;
        let to_copy = std::cmp::min(buf.len(), available);
        buf[..to_copy].copy_from_slice(&self.buffer[offset..offset + to_copy]);
        self.pos += to_copy as u64;
        Ok(to_copy)
    }
}

impl Seek for AlignedReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = match pos {
            SeekFrom::Start(offset) => offset,
            SeekFrom::End(offset) => (self.file_size as i64 + offset) as u64,
            SeekFrom::Current(offset) => (self.pos as i64 + offset) as u64,
        };
        Ok(self.pos)
    }
}
