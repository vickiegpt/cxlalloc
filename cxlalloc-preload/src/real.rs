use core::ffi::c_void;
use std::ffi::CStr;
use std::sync::OnceLock;

pub(crate) struct RealAlloc {
    pub malloc: unsafe extern "C" fn(usize) -> *mut c_void,
    pub calloc: unsafe extern "C" fn(usize, usize) -> *mut c_void,
    pub realloc: unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void,
    pub free: unsafe extern "C" fn(*mut c_void),
    pub memalign: unsafe extern "C" fn(usize, usize) -> *mut c_void,
    pub valloc: unsafe extern "C" fn(usize) -> *mut c_void,
    pub pvalloc: unsafe extern "C" fn(usize) -> *mut c_void,
    pub malloc_usable_size: unsafe extern "C" fn(*mut c_void) -> usize,
}

static REAL: OnceLock<Result<RealAlloc, &'static str>> = OnceLock::new();

impl RealAlloc {
    pub(crate) fn resolve() -> Result<&'static Self, &'static str> {
        match REAL.get_or_init(|| unsafe { Self::load() }) {
            Ok(real) => Ok(real),
            Err(error) => Err(*error),
        }
    }

    unsafe fn load() -> Result<Self, &'static str> {
        Ok(Self {
            malloc: symbol(c"malloc")?,
            calloc: symbol(c"calloc")?,
            realloc: symbol(c"realloc")?,
            free: symbol(c"free")?,
            memalign: symbol(c"memalign")?,
            valloc: symbol(c"valloc")?,
            pvalloc: symbol(c"pvalloc")?,
            malloc_usable_size: symbol(c"malloc_usable_size")?,
        })
    }
}

unsafe fn symbol<T: Copy>(name: &CStr) -> Result<T, &'static str> {
    let pointer = libc::dlsym(libc::RTLD_NEXT, name.as_ptr());
    if pointer.is_null() {
        return Err("cxlalloc-preload: failed to resolve libc allocator symbol");
    }
    debug_assert_eq!(core::mem::size_of::<T>(), core::mem::size_of_val(&pointer));
    Ok(core::mem::transmute_copy(&pointer))
}

#[cfg(test)]
mod tests {
    use super::RealAlloc;

    #[test]
    fn resolves_malloc_and_free_from_next_object() {
        let real = RealAlloc::resolve().expect("resolve libc allocator");
        let pointer = unsafe { (real.malloc)(64) };
        assert!(!pointer.is_null());
        unsafe { (real.free)(pointer) };
    }
}
