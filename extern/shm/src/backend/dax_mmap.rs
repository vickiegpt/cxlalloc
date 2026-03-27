use core::num::NonZeroUsize;
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use crate::Page;
use crate::backend::Backend;

use super::Dax;

/// Hybrid backend that interleaves allocations between DAX devices (CXL memory)
/// and anonymous mmap (main DRAM).
///
/// Each call to [`open`](crate::backend::Interface::open) alternates between
/// the DAX device and anonymous mmap in round-robin order. This provides
/// bandwidth and capacity interleaving across CXL-attached memory and local DRAM.
///
/// Both DAX and mmap allocations are aligned to the DAX device's required
/// alignment (e.g. 2 MiB for CXL) so that region sizes stay consistent
/// across the interleaved sequence.
#[derive(Debug)]
pub struct DaxMmap {
    dax: Dax,
    /// DAX device alignment — mmap allocations are rounded up to match.
    align: usize,
    /// Round-robin counter: even = DAX, odd = mmap.
    next: AtomicUsize,
}

impl DaxMmap {
    /// Create a hybrid backend that interleaves between the given DAX devices
    /// and anonymous mmap (main memory).
    ///
    /// `paths` should be device paths such as `/dev/dax0.0`.
    pub fn new(paths: &[impl AsRef<Path>]) -> std::io::Result<Self> {
        let dax = Dax::new(paths)?;
        let align = dax.align();
        Ok(Self {
            dax,
            align,
            next: AtomicUsize::new(0),
        })
    }
}

impl crate::backend::Interface for DaxMmap {
    fn name(&self) -> &'static str {
        "dax-mmap"
    }

    fn open(&self, id: &str, size: NonZeroUsize) -> crate::Result<super::File> {
        let index = self.next.fetch_add(1, Ordering::Relaxed);
        if index % 2 == 0 {
            // CXL memory via DAX device (alignment handled inside Dax::open)
            self.dax.open(id, size)
        } else {
            // Main DRAM via anonymous mmap, aligned to DAX device alignment
            let size_aligned = size.get().next_multiple_of(self.align);
            let size = NonZeroUsize::new(size_aligned.next_multiple_of(Page::SIZE)).unwrap();
            Ok(super::File::builder()
                .size(size)
                .offset(0)
                .create(true)
                .build())
        }
    }

    fn unlink(&self, _id: &str) -> crate::Result<()> {
        Ok(())
    }
}

impl From<DaxMmap> for Backend {
    fn from(dax_mmap: DaxMmap) -> Self {
        Backend::DaxMmap(dax_mmap)
    }
}
