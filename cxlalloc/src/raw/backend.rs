#[cfg(feature = "backend-dax")]
pub use shm::backend::Dax;
#[cfg(feature = "backend-dax")]
pub use shm::backend::DaxMmap;
pub use shm::backend::Mmap;
pub use shm::backend::Shm;
pub use shm::Numa;
pub use shm::Populate;

use core::ops::Deref;

use bon::Builder;

#[derive(Builder, Debug, Default)]
pub struct Backend {
    numa: Option<::shm::Numa>,
    populate: Option<::shm::Populate>,
    #[builder(into)]
    backend: shm::backend::Backend,
}

impl Backend {
    pub(super) fn numa(&self) -> Option<&::shm::Numa> {
        self.numa.as_ref()
    }

    pub(super) fn populate(&self) -> Option<::shm::Populate> {
        self.populate
    }
}

impl Deref for Backend {
    type Target = shm::Backend;
    fn deref(&self) -> &Self::Target {
        &self.backend
    }
}
