use core::ffi;
use core::num::NonZeroUsize;
use core::ptr;
use core::ptr::NonNull;

use crate::Page;
use crate::try_libc;

pub struct Reservation<const SIZE: usize> {
    address: NonNull<Page>,
}

impl<const SIZE: usize> Reservation<SIZE> {
    pub const SIZE: NonZeroUsize = NonZeroUsize::new(SIZE).unwrap();

    // In order to keep heap regions contiguous when extending, we need
    // to reserve an unbacked region of virtual address space,
    // and then overwrite it later via `mmap` with `MMAP_FIXED`.
    pub fn new() -> crate::Result<Self> {
        let address = Self::mmap(Self::SIZE)?;
        Ok(Self { address })
    }

    pub fn new_contiguous<const COUNT: usize>() -> crate::Result<[Self; COUNT]> {
        let total = const { NonZeroUsize::new(SIZE * COUNT).unwrap() };
        let address = Self::mmap(total)?;
        Ok(std::array::from_fn(|i| Self {
            address: unsafe { address.byte_add(SIZE * i) },
        }))
    }

    fn mmap(size: NonZeroUsize) -> crate::Result<NonNull<Page>> {
        unsafe {
            try_libc!(libc::mmap64(
                ptr::null_mut(),
                size.get(),
                libc::PROT_NONE,
                libc::MAP_ANONYMOUS | libc::MAP_PRIVATE,
                -1,
                0,
            ))
        }
        .map(NonNull::new)
        .map(Option::unwrap)
        .map(|address| address.cast::<Page>())
    }

    pub fn unmap(&self) -> crate::Result<()> {
        unsafe {
            crate::try_libc!(libc::munmap(
                self.address.as_ptr().cast::<ffi::c_void>(),
                SIZE,
            ))?;
        }
        Ok(())
    }

    pub fn start(&self) -> NonNull<Page> {
        self.address
    }

    pub fn end(&self) -> NonNull<Page> {
        unsafe { self.address.byte_add(SIZE) }
    }
}

impl<const SIZE: usize> core::fmt::Debug for Reservation<SIZE> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let lo = self.address;
        let hi = self.address.as_ptr().wrapping_byte_add(SIZE);
        lo.fmt(f)?;
        write!(f, "..")?;
        hi.fmt(f)
    }
}
