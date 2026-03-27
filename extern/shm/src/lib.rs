use core::marker::PhantomData;
use core::mem;
use core::num::NonZeroUsize;
use core::ptr::NonNull;

pub mod backend;
mod barrier;
mod error;
mod numa;
mod raw;
mod reservation;

pub use backend::Backend;
pub use barrier::Barrier;
pub use error::Error;
pub use numa::Numa;
pub use raw::Raw;
pub use reservation::Reservation;

pub type Result<T> = std::result::Result<T, Error>;

use bon::bon;

#[repr(C, align(4096))]
pub struct Page([u8; 4096]);

impl Page {
    pub const SIZE: usize = mem::size_of::<Self>();
}

#[derive(Copy, Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Populate {
    PageTable,
    Physical,
}

pub struct Shm<T> {
    inner: Raw,
    r#type: PhantomData<T>,
}

#[bon]
impl<T> Shm<T> {
    #[builder]
    pub fn new(
        numa: Option<Numa>,
        name: String,
        #[builder(default)] create: bool,
        populate: Option<Populate>,
    ) -> crate::Result<Self> {
        let inner = Raw::builder()
            .maybe_numa(numa)
            .name(name)
            .size(Self::SIZE)
            .create(create)
            .maybe_populate(populate)
            .build()?;

        Ok(Self {
            inner,
            r#type: PhantomData,
        })
    }
}

impl<T> Shm<T> {
    const SIZE: usize = mem::size_of::<T>().next_multiple_of(Page::SIZE);

    pub fn address(&self) -> NonNull<T> {
        self.inner.address.cast()
    }

    pub fn size(&self) -> NonZeroUsize {
        self.inner.size
    }

    pub fn unlink(&mut self) -> crate::Result<()> {
        self.inner.unlink()
    }
}

macro_rules! try_libc {
    // mmap64 returns a pointer instead of a status code
    (libc::mmap64( $($arg:expr),* $(,)? )) => {
        match libc::mmap64 ( $($arg),* ) {
            libc::MAP_FAILED => Err(crate::Error::Libc {
                name: "mmap64",
                source: ::std::io::Error::last_os_error()
            }),
            value => Ok(value),
        }
    };

    (libc:: $function:ident ( $($arg:expr),* $(,)? )) => {
        {
            use libc::$function;
            crate::try_libc!($function ( $($arg),* ))
        }
    };
    // Only needed for `mbind_syscall` in raw.rs
    ($function:ident ( $($arg:expr),* $(,)? )) => {
        match $function ( $($arg),* ) {
            -1 => Err(crate::Error::Libc {
                name: stringify!($function),
                source: ::std::io::Error::last_os_error()
            }),
            value => Ok(value),
        }
    };
}

macro_rules! try_pthread {
    (libc:: $function:ident ( $($arg:expr),* $(,)? )) => {
        match ::libc::$function ( $($arg),* ) {
            0 => Ok(()),
            error => Err(crate::Error::Libc {
                name: stringify!($function),
                source: std::io::Error::from_raw_os_error(error),
            }),
        }
    };
}

pub(crate) use try_libc;
pub(crate) use try_pthread;
