#[cfg(feature = "dax")]
mod dax;
#[cfg(feature = "dax")]
mod dax_mmap;
#[cfg(feature = "ivshmem")]
mod ivshmem;
mod mmap;
pub(crate) mod shm;

#[cfg(feature = "dax")]
pub use dax::Dax;
#[cfg(feature = "dax")]
pub use dax_mmap::DaxMmap;
#[cfg(feature = "ivshmem")]
pub use ivshmem::Ivshmem;
pub use mmap::Mmap;
pub use shm::Shm;

use core::ffi;
use core::num::NonZeroUsize;
use core::ptr;
use core::ptr::NonNull;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::prelude::RawFd;

use crate::Numa;
use crate::Page;
use crate::Populate;
use crate::try_libc;

/// Shared memory backend.
// Note: we use an enum here to avoid dynamic allocation
// of a `Box<dyn backend::Interface>` trait object. This is fine
// because the set of backends should not be extensible
// by downstream consumers.
#[derive(Debug)]
pub enum Backend {
    Mmap(Mmap),
    Shm(Shm),
    #[cfg(feature = "dax")]
    Dax(Dax),
    #[cfg(feature = "dax")]
    DaxMmap(DaxMmap),
    #[cfg(feature = "ivshmem")]
    Ivshmem(Ivshmem),
}

impl Backend {
    pub fn open(&self, id: &str, size: NonZeroUsize) -> crate::Result<File> {
        self.as_backend().open(id, size)
    }

    /// Human-readable name of backend, for debugging purposes.
    pub fn name(&self) -> &str {
        self.as_backend().name()
    }

    pub fn unlink(&self, id: &str) -> crate::Result<()> {
        self.as_backend().unlink(id)
    }

    fn as_backend(&self) -> &dyn Interface {
        match self {
            Backend::Mmap(mmap) => mmap,
            Backend::Shm(shm) => shm,
            #[cfg(feature = "dax")]
            Backend::Dax(dax) => dax,
            #[cfg(feature = "dax")]
            Backend::DaxMmap(dax_mmap) => dax_mmap,
            #[cfg(feature = "ivshmem")]
            Backend::Ivshmem(ivshmem) => ivshmem,
        }
    }
}

impl Default for Backend {
    fn default() -> Self {
        Backend::Mmap(Mmap)
    }
}

// This trait is an implementation detail for requiring
// our backend implementations to expose the same interface.
pub(crate) trait Interface: Send + Sync {
    fn name(&self) -> &'static str;

    fn open(&self, id: &str, size: NonZeroUsize) -> crate::Result<File>;

    fn unlink(&self, id: &str) -> crate::Result<()>;
}

pub struct File {
    fd: Option<OwnedFd>,
    size: NonZeroUsize,
    offset: i64,
    create: bool,
    extra_flags: libc::c_int,
    /// If set, replaces the automatically-selected base flags entirely.
    override_flags: Option<libc::c_int>,
}

impl AsRawFd for File {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_ref().map(|fd| fd.as_raw_fd()).unwrap_or(-1)
    }
}

impl File {
    /// Whether this file is newly created or already existed.
    pub fn is_create(&self) -> bool {
        self.create
    }

    pub(crate) fn flags(&self) -> libc::c_int {
        let base = match self.override_flags {
            Some(flags) => flags,
            None => match self.fd {
                Some(_) => libc::MAP_SHARED_VALIDATE,
                None => libc::MAP_ANONYMOUS | libc::MAP_PRIVATE,
            },
        };
        base | self.extra_flags
    }
}

#[bon::bon]
impl File {
    #[builder]
    pub(crate) fn new(
        fd: Option<OwnedFd>,
        size: NonZeroUsize,
        offset: i64,
        create: bool,
        #[builder(default)] extra_flags: libc::c_int,
        override_flags: Option<libc::c_int>,
    ) -> Self {
        Self {
            fd,
            size,
            offset,
            create,
            extra_flags,
            override_flags,
        }
    }

    /// SAFETY: caller must ensure `address` does not overlap an existing memory region.
    #[builder]
    pub unsafe fn map(
        self,
        address: Option<NonNull<Page>>,
        numa: Option<Numa>,
        populate: Option<Populate>,
    ) -> crate::Result<NonNull<Page>> {
        let actual = unsafe {
            try_libc!(libc::mmap64(
                address
                    .map(NonNull::as_ptr)
                    .unwrap_or_else(ptr::null_mut)
                    .cast(),
                self.size.get(),
                libc::PROT_READ | libc::PROT_WRITE,
                self.flags()
                    | address.map(|_| libc::MAP_FIXED).unwrap_or(0)
                    | if matches!(populate, Some(Populate::PageTable)) {
                        libc::MAP_POPULATE
                    } else {
                        0
                    },
                self.as_raw_fd(),
                self.offset,
            ))
        }
        .map(NonNull::new)
        .map(Option::unwrap)
        .map(|address| address.cast::<Page>())?;

        if let Some(expected) = address {
            assert_eq!(expected, actual);
        }

        if let Some(numa) = numa {
            numa.mbind(actual.as_ptr().cast(), self.size.get())?;
        }

        if matches!(populate, Some(Populate::Physical)) {
            madvise(actual.as_ptr().cast(), self.size.get())?;
        }

        Ok(actual)
    }
}

// SAFETY: `libc::madvise` will not dereference invalid address.
#[expect(clippy::not_unsafe_ptr_arg_deref)]
fn madvise(address: *mut ffi::c_void, size: usize) -> crate::Result<()> {
    unsafe { try_libc!(libc::madvise(address, size, libc::MADV_POPULATE_WRITE)) }?;
    Ok(())
}
