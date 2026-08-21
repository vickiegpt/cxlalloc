use core::ffi::c_void;
use core::mem;
use core::ptr;
use core::ptr::NonNull;
use std::ffi::CStr;
use std::fs;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;

const MAX_EXTENTS: usize = 128;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ArenaError {
    InvalidAlignment,
    InvalidDevice,
    InvalidPointer,
    OutOfMemory,
    Overflow,
    System(i32),
}

#[derive(Clone, Copy)]
#[repr(C)]
struct Extent {
    offset: usize,
    rounded_len: usize,
    requested_len: usize,
    active: bool,
}

impl Extent {
    const EMPTY: Self = Self {
        offset: 0,
        rounded_len: 0,
        requested_len: 0,
        active: false,
    };
}

#[repr(C)]
struct SharedState {
    mutex: libc::pthread_mutex_t,
    extents: [Extent; MAX_EXTENTS],
}

pub(crate) struct DaxArena {
    mapping: NonNull<c_void>,
    mapping_len: usize,
    base: NonNull<u8>,
    size: usize,
    alignment: usize,
    state: NonNull<SharedState>,
    _fd: Option<OwnedFd>,
}

unsafe impl Send for DaxArena {}
unsafe impl Sync for DaxArena {}

impl DaxArena {
    pub(crate) fn open(path: &CStr) -> Result<Self, ArenaError> {
        let descriptor = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if descriptor < 0 {
            return Err(last_errno());
        }
        let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
        lock_exclusive(descriptor.as_raw_fd())?;

        let name = device_name(path)?
            .to_str()
            .map_err(|_| ArenaError::InvalidDevice)?;
        let size = read_sysfs_usize(&format!("/sys/bus/dax/devices/{name}/size"))?;
        let alignment = read_sysfs_usize(&format!("/sys/bus/dax/devices/{name}/align"))?;
        if !alignment.is_power_of_two() || alignment < page_size() || size < alignment {
            return Err(ArenaError::InvalidAlignment);
        }

        let mapping = unsafe {
            libc::mmap(
                ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                descriptor.as_raw_fd(),
                0,
            )
        };
        if mapping == libc::MAP_FAILED {
            return Err(last_errno());
        }
        let mapping = NonNull::new(mapping).expect("mmap returned null");
        let aligned = align_up(mapping.as_ptr() as usize, alignment).ok_or(ArenaError::Overflow)?;
        let prefix = aligned - mapping.as_ptr() as usize;
        let usable_size = size.checked_sub(prefix).ok_or(ArenaError::OutOfMemory)?;
        let state = match create_shared_state() {
            Ok(state) => state,
            Err(error) => {
                unsafe { libc::munmap(mapping.as_ptr(), size) };
                return Err(error);
            }
        };

        Ok(Self {
            mapping,
            mapping_len: size,
            base: NonNull::new(aligned as *mut u8).expect("aligned mmap address is null"),
            size: usable_size,
            alignment,
            state,
            _fd: Some(descriptor),
        })
    }

    pub(crate) fn anonymous(size: usize, alignment: usize) -> Result<Self, ArenaError> {
        Self::new_anonymous(size, alignment)
    }

    #[cfg(test)]
    fn anonymous_for_test(size: usize, alignment: usize) -> Result<Self, ArenaError> {
        Self::new_anonymous(size, alignment)
    }

    fn new_anonymous(size: usize, alignment: usize) -> Result<Self, ArenaError> {
        if size == 0 || !alignment.is_power_of_two() {
            return Err(ArenaError::InvalidAlignment);
        }

        let mapping_len = size
            .checked_add(alignment - 1)
            .ok_or(ArenaError::Overflow)?;
        let mapping = unsafe {
            libc::mmap(
                ptr::null_mut(),
                mapping_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANONYMOUS | libc::MAP_SHARED,
                -1,
                0,
            )
        };
        if mapping == libc::MAP_FAILED {
            return Err(last_errno());
        }

        let mapping = NonNull::new(mapping).expect("mmap returned null");
        let aligned = align_up(mapping.as_ptr() as usize, alignment).ok_or(ArenaError::Overflow)?;
        let state = match create_shared_state() {
            Ok(state) => state,
            Err(error) => {
                unsafe { libc::munmap(mapping.as_ptr(), mapping_len) };
                return Err(error);
            }
        };

        Ok(Self {
            mapping,
            mapping_len,
            base: NonNull::new(aligned as *mut u8).expect("aligned mmap address is null"),
            size,
            alignment,
            state,
            _fd: None,
        })
    }

    pub(crate) fn allocate(
        &self,
        size: usize,
        alignment: usize,
    ) -> Result<NonNull<c_void>, ArenaError> {
        if !alignment.is_power_of_two() {
            return Err(ArenaError::InvalidAlignment);
        }

        let requested_len = size.max(1);
        let rounded_len = align_up(requested_len, self.alignment).ok_or(ArenaError::Overflow)?;
        let allocation_alignment = alignment.max(self.alignment);
        let mut state = self.lock()?;
        let slot = state
            .extents()
            .iter()
            .position(|extent| !extent.active)
            .ok_or(ArenaError::OutOfMemory)?;

        let mut candidate = 0usize;
        loop {
            candidate =
                aligned_offset(self.base.as_ptr() as usize, candidate, allocation_alignment)
                    .ok_or(ArenaError::Overflow)?;
            let next = state
                .extents()
                .iter()
                .filter(|extent| extent.active && extent.offset >= candidate)
                .min_by_key(|extent| extent.offset)
                .copied();

            match next {
                Some(extent) if candidate + rounded_len <= extent.offset => break,
                Some(extent) => {
                    candidate = extent
                        .offset
                        .checked_add(extent.rounded_len)
                        .ok_or(ArenaError::Overflow)?;
                }
                None => break,
            }
        }

        if candidate
            .checked_add(rounded_len)
            .filter(|end| *end <= self.size)
            .is_none()
        {
            return Err(ArenaError::OutOfMemory);
        }

        state.extents_mut()[slot] = Extent {
            offset: candidate,
            rounded_len,
            requested_len,
            active: true,
        };

        let pointer = unsafe { self.base.as_ptr().add(candidate).cast::<c_void>() };
        Ok(NonNull::new(pointer).expect("arena allocation is null"))
    }

    pub(crate) fn deallocate(&self, pointer: NonNull<c_void>) -> Result<(), ArenaError> {
        let offset = self
            .pointer_offset(pointer.as_ptr())
            .ok_or(ArenaError::InvalidPointer)?;
        let mut state = self.lock()?;
        let extent = state
            .extents_mut()
            .iter_mut()
            .find(|extent| extent.active && extent.offset == offset)
            .ok_or(ArenaError::InvalidPointer)?;
        *extent = Extent::EMPTY;
        Ok(())
    }

    pub(crate) fn allocation_size(&self, pointer: NonNull<c_void>) -> Option<usize> {
        let offset = self.pointer_offset(pointer.as_ptr())?;
        let state = self.lock().ok()?;
        state
            .extents()
            .iter()
            .find(|extent| extent.active && extent.offset == offset)
            .map(|extent| extent.requested_len)
    }

    pub(crate) fn owns(&self, pointer: *const c_void) -> bool {
        self.pointer_offset(pointer).is_some()
    }

    fn pointer_offset(&self, pointer: *const c_void) -> Option<usize> {
        let start = self.base.as_ptr() as usize;
        let offset = (pointer as usize).checked_sub(start)?;
        (offset < self.size).then_some(offset)
    }

    #[cfg(test)]
    fn offset_of(&self, pointer: NonNull<c_void>) -> usize {
        self.pointer_offset(pointer.as_ptr()).unwrap()
    }

    fn lock(&self) -> Result<StateGuard, ArenaError> {
        let mutex = unsafe { ptr::addr_of_mut!((*self.state.as_ptr()).mutex) };
        let result = unsafe { libc::pthread_mutex_lock(mutex) };
        if result == libc::EOWNERDEAD {
            let consistent = unsafe { libc::pthread_mutex_consistent(mutex) };
            if consistent != 0 {
                return Err(ArenaError::System(consistent));
            }
        } else if result != 0 {
            return Err(ArenaError::System(result));
        }
        Ok(StateGuard { state: self.state })
    }
}

impl Drop for DaxArena {
    fn drop(&mut self) {
        unsafe {
            libc::pthread_mutex_destroy(ptr::addr_of_mut!((*self.state.as_ptr()).mutex));
            libc::munmap(self.state.as_ptr().cast(), mem::size_of::<SharedState>());
            libc::munmap(self.mapping.as_ptr(), self.mapping_len);
        }
    }
}

struct StateGuard {
    state: NonNull<SharedState>,
}

impl StateGuard {
    fn extents(&self) -> &[Extent; MAX_EXTENTS] {
        unsafe { &(*self.state.as_ptr()).extents }
    }

    fn extents_mut(&mut self) -> &mut [Extent; MAX_EXTENTS] {
        unsafe { &mut (*self.state.as_ptr()).extents }
    }
}

impl Drop for StateGuard {
    fn drop(&mut self) {
        unsafe {
            libc::pthread_mutex_unlock(ptr::addr_of_mut!((*self.state.as_ptr()).mutex));
        }
    }
}

fn create_shared_state() -> Result<NonNull<SharedState>, ArenaError> {
    let size = mem::size_of::<SharedState>();
    let mapping = unsafe {
        libc::mmap(
            ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANONYMOUS | libc::MAP_SHARED,
            -1,
            0,
        )
    };
    if mapping == libc::MAP_FAILED {
        return Err(last_errno());
    }

    unsafe { ptr::write_bytes(mapping, 0, size) };
    let state = NonNull::new(mapping.cast::<SharedState>()).expect("mmap returned null");
    let mut attributes = mem::MaybeUninit::<libc::pthread_mutexattr_t>::uninit();
    let mut result = unsafe { libc::pthread_mutexattr_init(attributes.as_mut_ptr()) };
    if result == 0 {
        result = unsafe {
            libc::pthread_mutexattr_setpshared(
                attributes.as_mut_ptr(),
                libc::PTHREAD_PROCESS_SHARED,
            )
        };
    }
    if result == 0 {
        result = unsafe {
            libc::pthread_mutexattr_setrobust(attributes.as_mut_ptr(), libc::PTHREAD_MUTEX_ROBUST)
        };
    }
    if result == 0 {
        result = unsafe {
            libc::pthread_mutex_init(
                ptr::addr_of_mut!((*state.as_ptr()).mutex),
                attributes.as_ptr(),
            )
        };
    }
    unsafe { libc::pthread_mutexattr_destroy(attributes.as_mut_ptr()) };

    if result != 0 {
        unsafe { libc::munmap(mapping, size) };
        return Err(ArenaError::System(result));
    }
    Ok(state)
}

fn align_up(value: usize, alignment: usize) -> Option<usize> {
    value
        .checked_add(alignment.checked_sub(1)?)
        .map(|value| value & !(alignment - 1))
}

fn aligned_offset(base: usize, offset: usize, alignment: usize) -> Option<usize> {
    let absolute = base.checked_add(offset)?;
    align_up(absolute, alignment)?.checked_sub(base)
}

fn last_errno() -> ArenaError {
    ArenaError::System(
        std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO),
    )
}

fn device_name(path: &CStr) -> Result<&CStr, ArenaError> {
    let bytes = path.to_bytes_with_nul();
    let start = bytes[..bytes.len() - 1]
        .iter()
        .rposition(|byte| *byte == b'/')
        .map_or(0, |index| index + 1);
    if start == bytes.len() - 1 {
        return Err(ArenaError::InvalidDevice);
    }
    CStr::from_bytes_with_nul(&bytes[start..]).map_err(|_| ArenaError::InvalidDevice)
}

fn parse_sysfs_usize(contents: &[u8]) -> Result<usize, ArenaError> {
    let text = core::str::from_utf8(contents).map_err(|_| ArenaError::InvalidDevice)?;
    let value = text
        .trim()
        .parse::<usize>()
        .map_err(|_| ArenaError::InvalidDevice)?;
    if value == 0 {
        return Err(ArenaError::InvalidDevice);
    }
    Ok(value)
}

fn read_sysfs_usize(path: &str) -> Result<usize, ArenaError> {
    let contents = fs::read(path)
        .map_err(|error| ArenaError::System(error.raw_os_error().unwrap_or(libc::EIO)))?;
    parse_sysfs_usize(&contents)
}

fn lock_exclusive(descriptor: libc::c_int) -> Result<(), ArenaError> {
    if unsafe { libc::flock(descriptor, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        Ok(())
    } else {
        Err(last_errno())
    }
}

fn page_size() -> usize {
    usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
        .ok()
        .filter(|size| *size != 0)
        .unwrap_or(4096)
}

#[cfg(test)]
mod tests {
    use super::{device_name, lock_exclusive, parse_sysfs_usize, ArenaError, DaxArena};
    use core::mem;
    use std::ffi::CStr;

    #[test]
    fn allocations_are_aligned_nonoverlapping_and_reused_after_free() {
        let arena = DaxArena::anonymous_for_test(16 << 20, 2 << 20).unwrap();
        let first = arena.allocate(3 << 20, 4096).unwrap();
        assert_eq!((first.as_ptr() as usize) % (2 << 20), 0);

        let second = arena.allocate(2 << 20, 4096).unwrap();
        assert_ne!(first, second);
        assert!(second.as_ptr() as usize >= first.as_ptr() as usize + (4 << 20));

        arena.deallocate(first).unwrap();
        let reused = arena.allocate(3 << 20, 4096).unwrap();
        assert_eq!(first, reused);
    }

    #[test]
    fn capacity_exhaustion_does_not_fall_back() {
        let arena = DaxArena::anonymous_for_test(4 << 20, 2 << 20).unwrap();
        assert!(arena.allocate(4 << 20, 4096).is_ok());
        assert_eq!(arena.allocate(1, 1), Err(ArenaError::OutOfMemory));
    }

    #[test]
    fn pointer_below_arena_is_not_owned() {
        let arena = DaxArena::anonymous_for_test(4 << 20, 2 << 20).unwrap();
        let below = arena.base.as_ptr().wrapping_sub(1).cast();
        assert!(!arena.owns(below));
    }

    #[test]
    fn allocation_state_is_shared_across_fork() {
        let arena = DaxArena::anonymous_for_test(16 << 20, 2 << 20).unwrap();
        let mut child_to_parent = [-1; 2];
        let mut parent_to_child = [-1; 2];
        assert_eq!(unsafe { libc::pipe(child_to_parent.as_mut_ptr()) }, 0);
        assert_eq!(unsafe { libc::pipe(parent_to_child.as_mut_ptr()) }, 0);

        let child = unsafe { libc::fork() };
        assert!(child >= 0);
        if child == 0 {
            unsafe {
                libc::close(child_to_parent[0]);
                libc::close(parent_to_child[1]);
            }
            let allocation = arena.allocate(4 << 20, 4096).unwrap();
            let offset = arena.offset_of(allocation);
            assert_eq!(
                unsafe {
                    libc::write(
                        child_to_parent[1],
                        (&offset as *const usize).cast(),
                        mem::size_of::<usize>(),
                    )
                },
                mem::size_of::<usize>() as isize,
            );
            let mut release = 0u8;
            assert_eq!(
                unsafe { libc::read(parent_to_child[0], (&mut release as *mut u8).cast(), 1) },
                1,
            );
            unsafe { libc::_exit(0) };
        }

        unsafe {
            libc::close(child_to_parent[1]);
            libc::close(parent_to_child[0]);
        }
        let mut child_offset = 0usize;
        assert_eq!(
            unsafe {
                libc::read(
                    child_to_parent[0],
                    (&mut child_offset as *mut usize).cast(),
                    mem::size_of::<usize>(),
                )
            },
            mem::size_of::<usize>() as isize,
        );

        let parent = arena.allocate(4 << 20, 4096).unwrap();
        assert_ne!(child_offset, arena.offset_of(parent));
        assert_eq!(
            unsafe { libc::write(parent_to_child[1], (&1u8 as *const u8).cast(), 1) },
            1,
        );
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);
    }

    #[test]
    fn parses_device_name_size_and_alignment() {
        assert_eq!(device_name(c"/dev/dax0.0").unwrap(), c"dax0.0");
        assert_eq!(parse_sysfs_usize(b"262144000\n").unwrap(), 262_144_000);
        assert_eq!(parse_sysfs_usize(b"2097152\n").unwrap(), 2_097_152);
        assert!(parse_sysfs_usize(b"not-a-size\n").is_err());
    }

    #[test]
    fn exclusive_lock_rejects_a_second_open_description() {
        let mut template = *b"/tmp/cxlalloc-lock-XXXXXX\0";
        let first = unsafe { libc::mkstemp(template.as_mut_ptr().cast()) };
        assert!(first >= 0);
        let path = CStr::from_bytes_until_nul(&template).unwrap();
        let second = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        assert!(second >= 0);

        assert_eq!(lock_exclusive(first), Ok(()));
        assert_eq!(
            lock_exclusive(second),
            Err(ArenaError::System(libc::EWOULDBLOCK))
        );

        unsafe {
            libc::close(second);
            libc::close(first);
            libc::unlink(path.as_ptr());
        }
    }
}
