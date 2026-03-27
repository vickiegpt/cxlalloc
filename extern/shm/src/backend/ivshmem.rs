use core::num::NonZeroUsize;
use std::fs::File;
use std::os::fd::OwnedFd;
use std::path::Path;

use crate::backend::Backend;

#[derive(Debug)]
pub struct Ivshmem {
    device: File,
}

impl Ivshmem {
    #[allow(clippy::new_without_default)]
    pub fn new() -> std::io::Result<Self> {
        File::options()
            .read(true)
            .write(true)
            .open("/dev/cxl_ivpci0")
            .map(|device| Self { device })
    }

    pub fn open(path: &Path) -> std::io::Result<Self> {
        File::options()
            .read(true)
            .write(true)
            .open(path)
            .map(|device| Self { device })
    }
}

impl crate::backend::Interface for Ivshmem {
    fn name(&self) -> &'static str {
        "ivshmem"
    }

    fn open(&self, id: &str, size: NonZeroUsize) -> crate::Result<super::File> {
        let allocation = driver::find_cxl_alloc_nomap(&self.device, &id, size.get())
            .expect("Failed to allocate from ivshmem device");

        log::debug!(
            "{} allocation with id={}, size={:#x?}, offset={:#x?}",
            if allocation.existing == 0 {
                "Created"
            } else {
                "Found"
            },
            id,
            allocation.desc.length,
            allocation.desc.offset,
        );

        Ok(crate::backend::File::builder()
            .fd(OwnedFd::from(
                self.device
                    .try_clone()
                    .expect("Failed to clone ivshmem device"),
            ))
            .size(size)
            .offset(allocation.desc.offset as i64)
            .create(allocation.existing == 0)
            .build())
    }

    fn unlink(&self, _id: &str) -> crate::Result<()> {
        // FIXME: call `driver::cxl_free`
        Ok(())
    }
}

impl From<Ivshmem> for Backend {
    fn from(ivshmem: Ivshmem) -> Self {
        Backend::Ivshmem(ivshmem)
    }
}

#[allow(dead_code, non_camel_case_types)]
mod driver {
    use core::ffi;
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd as _;

    use ribbit::private::u14;

    // https://sites.uclouvain.be/SystInfo/usr/include/asm-generic/ioctl.h.html
    #[ribbit::pack(size = 32, debug)]
    #[repr(C)]
    struct Ioctl {
        function: u8,
        driver: u8,
        size: u14,
        #[ribbit(size = 2)]
        dir: Dir,
    }

    #[ribbit::pack(size = 2, debug)]
    enum Dir {
        None,
        W,
        R,
        RW,
    }

    #[repr(C)]
    #[derive(Default)]
    pub(super) struct region_desc {
        pub(super) offset: u64,
        pub(super) length: u64,
        prog_id: [u8; 28],
    }

    #[repr(C)]
    #[derive(Default)]
    pub(super) struct vcxl_find_alloc {
        pub(super) desc: region_desc,
        pub(super) existing: ffi::c_int,
    }

    const IOCTL_MAGIC: u8 = b'f';

    pub(super) fn find_cxl_alloc_nomap(
        file: &File,
        id: &str,
        size: usize,
    ) -> io::Result<vcxl_find_alloc> {
        const IOCTL_FIND_ALLOC: Ioctl = Ioctl::new(
            8,
            IOCTL_MAGIC,
            u14::new(size_of::<vcxl_find_alloc>() as u16),
            Dir::new(DirUnpacked::RW),
        );

        let mut find = vcxl_find_alloc::default();
        find.desc.length = size as u64;

        assert!(
            id.len() < 28,
            "Ivshmem driver only supports IDs up to length 28 (including null byte), got {id:?}"
        );

        // Note: `to_bytes` does not include null terminator. We check above
        // that `id` length + 1 fits, and array is 0-initialized.
        find.desc.prog_id[..id.len()].copy_from_slice(id.as_bytes());

        match unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                ribbit::convert::packed_to_loose(IOCTL_FIND_ALLOC) as u64,
                &mut find,
            )
        } {
            0 => Ok(find),
            _ => Err(io::Error::last_os_error()),
        }
    }

    #[expect(clippy::field_reassign_with_default)]
    pub(super) fn cxl_free(file: &File, id: &str, offset: i64, size: usize) -> io::Result<()> {
        const IOCTL_FREE: Ioctl = Ioctl::new(
            7,
            IOCTL_MAGIC,
            u14::new(size_of::<region_desc>() as u16),
            // Possibly an issue with the driver interface? Should at least be `R`.
            Dir::new(DirUnpacked::W),
        );

        let mut free = region_desc {
            offset: offset as u64,
            length: size as u64,
            prog_id: [0u8; 28],
        };

        free.prog_id[..id.as_bytes().len()].copy_from_slice(id.as_bytes());

        match unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                ribbit::convert::packed_to_loose(IOCTL_FREE) as u64,
                &mut free,
            )
        } {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }
}
