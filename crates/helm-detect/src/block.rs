//! Random access access to block devices and partitions.
//!
//! Everything in the detection module works on raw block devices: the OS
//! "list my drives" APIs are deliberately not used, because they only ever
//! report the drives of the running system. Helm has to see *every* disk,
//! including the ones holding an operating system it is not currently running.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::Arc;

/// A device that can be read at arbitrary offsets.
pub trait BlockDevice: Send + Sync {
    /// Total size of the device in bytes.
    fn size(&self) -> io::Result<u64>;

    /// Read into `buf` starting at `offset`. Returns the number of bytes
    /// read, which is shorter than `buf.len()` at the end of the device.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;

    /// Read exactly `buf.len()` bytes, failing when the device ends early.
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut read = 0usize;
        while read < buf.len() {
            match self.read_at(offset + read as u64, &mut buf[read..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "unexpected end of device",
                    ))
                }
                Ok(n) => read += n,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    /// Read `count` bytes at `offset`.
    fn read_vec_at(&self, offset: u64, count: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; count];
        self.read_exact_at(offset, &mut buf)?;
        Ok(buf)
    }
}

/// A block device backed by a file (a raw disk or a disk image).
#[derive(Debug)]
pub struct FileDevice {
    file: File,
    len: u64,
}

impl FileDevice {
    /// Open a device or image file.
    pub fn open<P: AsRef<std::path::Path>>(path: P) -> io::Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        Ok(Self { file, len })
    }

    /// Wrap an already opened file.
    pub fn from_file(file: File) -> io::Result<Self> {
        let len = file.metadata()?.len();
        Ok(Self { file, len })
    }
}

#[cfg(unix)]
fn read_file_at(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, offset)
}

#[cfg(windows)]
fn read_file_at(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buf, offset)
}

#[cfg(not(any(unix, windows)))]
fn read_file_at(_file: &File, _offset: u64, _buf: &mut [u8]) -> io::Result<usize> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "positional reads are not supported on this platform",
    ))
}

impl BlockDevice for FileDevice {
    fn size(&self) -> io::Result<u64> {
        Ok(self.len)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        read_file_at(&self.file, offset, buf)
    }
}

/// A block device backed by an in-memory buffer. Used by the tests.
#[derive(Debug, Clone)]
pub struct SliceDevice {
    data: Vec<u8>,
}

impl SliceDevice {
    pub fn new(data: Vec<u8>) -> Self {
        Self { data }
    }
}

impl BlockDevice for SliceDevice {
    fn size(&self) -> io::Result<u64> {
        Ok(self.data.len() as u64)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let start = offset as usize;
        if start >= self.data.len() {
            return Ok(0);
        }
        let end = (start + buf.len()).min(self.data.len());
        buf[..end - start].copy_from_slice(&self.data[start..end]);
        Ok(end - start)
    }
}

/// A view on a partition of a block device.
#[derive(Clone)]
pub struct PartitionDevice {
    inner: Arc<dyn BlockDevice>,
    start: u64,
    len: u64,
}

impl std::fmt::Debug for PartitionDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PartitionDevice")
            .field("start", &self.start)
            .field("len", &self.len)
            .finish()
    }
}

impl PartitionDevice {
    /// Create a view on `start .. start + len` of `inner`.
    pub fn new(inner: Arc<dyn BlockDevice>, start: u64, len: u64) -> Self {
        Self { inner, start, len }
    }
}

impl BlockDevice for PartitionDevice {
    fn size(&self) -> io::Result<u64> {
        Ok(self.len)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if offset >= self.len {
            return Ok(0);
        }
        let max = ((self.len - offset) as usize).min(buf.len());
        self.inner.read_at(self.start + offset, &mut buf[..max])
    }
}

/// Adapter that lets the `ext4` crate read from any [`BlockDevice`].
pub struct SharedDevice(pub Arc<dyn BlockDevice>);

impl positioned_io::ReadAt for SharedDevice {
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read_at(pos, buf)
    }
}

/// A `Read + Seek` cursor over a block device.
///
/// The NTFS crate needs a seekable reader; the FAT crate additionally needs
/// a `Write` implementation, which is stubbed out because Helm never modifies
/// a filesystem.
pub struct DeviceCursor {
    device: Arc<dyn BlockDevice>,
    position: u64,
}

impl DeviceCursor {
    pub fn new(device: Arc<dyn BlockDevice>) -> Self {
        Self {
            device,
            position: 0,
        }
    }
}

impl Read for DeviceCursor {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.device.read_at(self.position, buf)?;
        self.position += read as u64;
        Ok(read)
    }
}

impl Seek for DeviceCursor {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let len = self.device.size().unwrap_or(0);
        let target = match pos {
            SeekFrom::Start(offset) => offset as i128,
            SeekFrom::End(offset) => len as i128 + offset as i128,
            SeekFrom::Current(offset) => self.position as i128 + offset as i128,
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before the start of the device",
            ));
        }
        self.position = target as u64;
        Ok(self.position)
    }
}

impl Write for DeviceCursor {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Helm only reads filesystems",
        ))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_device_reads() {
        let device = SliceDevice::new((0u8..64).collect());
        assert_eq!(device.size().unwrap(), 64);
        let mut buf = [0u8; 8];
        assert_eq!(device.read_at(16, &mut buf).unwrap(), 8);
        assert_eq!(buf, [16, 17, 18, 19, 20, 21, 22, 23]);
        assert_eq!(device.read_at(60, &mut buf).unwrap(), 4);
    }

    #[test]
    fn partition_view_offsets_reads() {
        let device = Arc::new(SliceDevice::new((0u8..128).collect()));
        let partition = PartitionDevice::new(device, 64, 32);
        let mut buf = [0u8; 4];
        partition.read_exact_at(0, &mut buf).unwrap();
        assert_eq!(buf, [64, 65, 66, 67]);
        assert_eq!(partition.size().unwrap(), 32);
        // Reads past the end of the partition are clipped.
        assert_eq!(partition.read_at(30, &mut buf).unwrap(), 2);
    }

    #[test]
    fn cursor_seeks_and_reads() {
        let device = Arc::new(SliceDevice::new((0u8..64).collect()));
        let mut cursor = DeviceCursor::new(device);
        let mut buf = [0u8; 4];
        cursor.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [0, 1, 2, 3]);
        cursor.seek(SeekFrom::Start(60)).unwrap();
        assert_eq!(cursor.read(&mut buf).unwrap(), 4);
        assert_eq!(buf, [60, 61, 62, 63]);
        assert!(cursor.write(&buf).is_err());
    }
}
