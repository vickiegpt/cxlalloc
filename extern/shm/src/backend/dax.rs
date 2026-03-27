use core::num::NonZeroUsize;
use std::fs;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use crate::backend::Backend;

struct Device {
    file: fs::File,
    size: usize,
    /// Required alignment for mmap on this device (from sysfs).
    align: usize,
    /// Next available offset within this device.
    offset: AtomicUsize,
}

impl core::fmt::Debug for Device {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Device")
            .field("size", &self.size)
            .field("offset", &self.offset)
            .finish()
    }
}

/// DAX device backend with round-robin interleaving across multiple devices.
///
/// Each call to [`open`](crate::backend::Interface::open) selects the next
/// device in round-robin order and bump-allocates within it. This provides
/// bandwidth interleaving when used with [`Sequential`](crate::Reservation)
/// regions that lazily extend one chunk at a time.
#[derive(Debug)]
pub struct Dax {
    devices: Vec<Device>,
    /// Round-robin counter for device selection.
    next: AtomicUsize,
}

impl Dax {
    /// Open multiple DAX devices for interleaved allocation.
    ///
    /// `paths` should be device paths such as `/dev/dax0.0`, `/dev/dax1.0`, etc.
    /// Device sizes are read from sysfs.
    pub fn new(paths: &[impl AsRef<Path>]) -> std::io::Result<Self> {
        assert!(!paths.is_empty(), "At least one DAX device path is required");

        let devices = paths
            .iter()
            .map(|path| {
                let path = path.as_ref();
                let file = fs::File::options().read(true).write(true).open(path)?;
                let size = device_size(path)?;
                let align = device_align(path)?;
                Ok(Device {
                    file,
                    size,
                    align,
                    offset: AtomicUsize::new(0),
                })
            })
            .collect::<std::io::Result<Vec<_>>>()?;

        Ok(Self {
            devices,
            next: AtomicUsize::new(0),
        })
    }
}

impl Dax {
    /// The minimum alignment required by the underlying DAX device(s).
    /// All allocations from this backend are rounded up to this alignment.
    pub fn align(&self) -> usize {
        self.devices.iter().map(|d| d.align).max().unwrap_or(4096)
    }
}

impl crate::backend::Interface for Dax {
    fn name(&self) -> &'static str {
        "dax"
    }

    fn open(&self, _id: &str, size: NonZeroUsize) -> crate::Result<super::File> {
        // Round-robin device selection.
        let index = self.next.fetch_add(1, Ordering::Relaxed) % self.devices.len();
        let device = &self.devices[index];

        // Align to the device's required alignment (e.g. 2MB for CXL DAX).
        let size_aligned = size.get().next_multiple_of(device.align);

        // Bump-allocate within the selected device.
        let offset = device.offset.fetch_add(size_aligned, Ordering::Relaxed);
        assert!(
            offset + size_aligned <= device.size,
            "DAX device {index} exhausted: need {:#x} bytes at offset {:#x}, device size {:#x}",
            size_aligned,
            offset,
            device.size,
        );

        Ok(crate::backend::File::builder()
            .fd(OwnedFd::from(
                device
                    .file
                    .try_clone()
                    .expect("Failed to clone DAX device fd"),
            ))
            .size(NonZeroUsize::new(size_aligned).unwrap())
            .offset(offset as i64)
            .create(true)
            // Use MAP_SHARED instead of MAP_SHARED_VALIDATE for DAX.
            // MAP_SHARED_VALIDATE causes SIGILL from glibc's AVX-512
            // non-temporal stores on some CXL/QEMU configurations.
            .override_flags(libc::MAP_SHARED)
            .build())
    }

    fn unlink(&self, _id: &str) -> crate::Result<()> {
        Ok(())
    }
}

impl From<Dax> for Backend {
    fn from(dax: Dax) -> Self {
        Backend::Dax(dax)
    }
}

/// Read the required mmap alignment of a DAX device from sysfs.
///
/// For a device at `/dev/dax0.0`, reads `/sys/bus/dax/devices/dax0.0/align`.
/// Falls back to [`Page::SIZE`] (4 KiB) if the sysfs entry is missing.
fn device_align(path: &Path) -> std::io::Result<usize> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid DAX device path: {}", path.display()),
            )
        })?;

    let sysfs_path = format!("/sys/bus/dax/devices/{name}/align");
    match fs::read_to_string(&sysfs_path) {
        Ok(contents) => contents.trim().parse::<usize>().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("failed to parse DAX device alignment from {sysfs_path}: {e}"),
            )
        }),
        Err(_) => Ok(4096), // fallback to 4 KiB page size
    }
}

/// Read the size of a DAX device from sysfs.
///
/// For a device at `/dev/dax0.0`, reads `/sys/bus/dax/devices/dax0.0/size`.
fn device_size(path: &Path) -> std::io::Result<usize> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid DAX device path: {}", path.display()),
            )
        })?;

    let sysfs_path = format!("/sys/bus/dax/devices/{name}/size");
    let contents = fs::read_to_string(&sysfs_path).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("failed to read DAX device size from {sysfs_path}: {e}"),
        )
    })?;

    contents.trim().parse::<usize>().map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("failed to parse DAX device size from {sysfs_path}: {e}"),
        )
    })
}
