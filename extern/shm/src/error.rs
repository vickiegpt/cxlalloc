use core::fmt::Display;
use std::io;

use crate::backend;

#[derive(Debug)]
pub enum Error {
    ShmName,
    Shm {
        path: backend::shm::Path,
        name: &'static str,
        source: io::Error,
    },
    Libc {
        name: &'static str,
        source: io::Error,
    },
}

impl Error {
    pub(crate) fn with_path(self, path: backend::shm::Path) -> Self {
        match self {
            Error::ShmName | Error::Shm { .. } => unreachable!(),
            Error::Libc { name, source } => Self::Shm { path, name, source },
        }
    }

    pub(crate) fn is_not_found(&self) -> bool {
        match self {
            Error::Shm { source, .. } | Error::Libc { source, .. } => {
                matches!(source.kind(), io::ErrorKind::NotFound)
            }
            _ => false,
        }
    }

    pub(crate) fn is_already_exists(&self) -> bool {
        match self {
            Error::Libc { name: _, source } => {
                matches!(source.kind(), io::ErrorKind::AlreadyExists)
            }
            _ => false,
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ShmName => write!(f, "shm name can be at most {} bytes", backend::Shm::MAX_LEN),
            Self::Shm {
                path,
                name,
                source: _,
            } => write!(
                f,
                "{name} error ({})",
                std::str::from_utf8(path).unwrap_or("")
            ),
            Self::Libc { name, source: _ } => write!(f, "{name} error"),
        }
    }
}

impl core::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ShmName => None,
            Self::Shm { source, .. } | Self::Libc { source, .. } => Some(source),
        }
    }
}
