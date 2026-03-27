use core::ffi::CStr;
use core::num::NonZeroUsize;

use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd as _;
use std::os::fd::OwnedFd;

use crate::Page;
use crate::backend;

#[derive(Debug)]
pub struct Shm;

impl backend::Interface for Shm {
    fn name(&self) -> &'static str {
        "shm"
    }

    fn open(&self, id: &str, size: NonZeroUsize) -> crate::Result<backend::File> {
        let size = size.get().next_multiple_of(Page::SIZE);

        let (create, fd) = Self::with_path(id, |path| {
            let (create, fd) = match unsafe {
                crate::try_libc!(libc::shm_open(
                    path.as_ptr(),
                    libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                    0o666,
                ))
            } {
                Err(error) if error.is_already_exists() => unsafe {
                    let fd = crate::try_libc!(libc::shm_open(path.as_ptr(), libc::O_RDWR, 0o666))
                        .map(|fd| OwnedFd::from_raw_fd(fd))?;
                    Ok((false, fd))
                },
                Err(error) => Err(error),
                Ok(fd) => Ok((true, unsafe { OwnedFd::from_raw_fd(fd) })),
            }?;

            if create {
                unsafe {
                    crate::try_libc!(libc::ftruncate64(fd.as_raw_fd(), size as i64))?;
                }
            }

            Ok((create, fd))
        })?;

        Ok(backend::File::builder()
            .fd(fd)
            .size(NonZeroUsize::new(size).unwrap())
            .create(create)
            .offset(0)
            .build())
    }

    fn unlink(&self, id: &str) -> crate::Result<()> {
        Self::with_path(id, shm_unlink)
    }
}

impl From<Shm> for backend::Backend {
    fn from(shm: Shm) -> Self {
        backend::Backend::Shm(shm)
    }
}

pub type Path = [u8; Shm::MAX_LEN + 1];

impl Shm {
    pub const MAX_LEN: usize = 62;

    fn with_path<T, F: FnOnce(&CStr) -> crate::Result<T>>(id: &str, apply: F) -> crate::Result<T> {
        if id.len() > Self::MAX_LEN {
            return Err(crate::Error::ShmName);
        }

        let mut path = [0u8; Self::MAX_LEN + 1];
        path[0] = b'/';
        path[1..][..id.len()].copy_from_slice(id.as_bytes());
        apply(CStr::from_bytes_until_nul(&path).unwrap()).map_err(|error| error.with_path(path))
    }
}

fn shm_unlink(name: &CStr) -> crate::Result<()> {
    unsafe { crate::try_libc!(libc::shm_unlink(name.as_ptr())) }?;
    Ok(())
}
