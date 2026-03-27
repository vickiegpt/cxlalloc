use core::ffi;

use crate::try_libc;

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(tag = "policy", rename_all = "snake_case"))]
pub enum Numa {
    Bind { node: usize },
    Interleave { nodes: Vec<usize> },
}

impl Numa {
    // SAFETY: `mbind` will not dereference invalid address.
    #[expect(clippy::not_unsafe_ptr_arg_deref)]
    pub fn mbind(&self, address: *mut ffi::c_void, size: usize) -> crate::Result<()> {
        // Call syscall to avoid external C dependency on `libnuma`.
        //
        // https://github.com/numactl/numactl/blob/6c14bd59d438ebb5ef828e393e8563ba18f59cb2/syscall.c#L230-L235
        unsafe fn mbind_syscall(
            address: *mut ffi::c_void,
            size: libc::c_ulong,
            mode: libc::c_int,
            mask: *const libc::c_ulong,
            maxnode: libc::c_ulong,
            flags: libc::c_uint,
        ) -> i64 {
            unsafe { libc::syscall(libc::SYS_mbind, address, size, mode, mask, maxnode, flags) }
        }

        let (mode, mask) = self.to_mode_mask();

        unsafe {
            try_libc!(mbind_syscall(
                address,
                size as u64,
                mode,
                &mask,
                64,
                // MPOL_MF_STRICT sometimes raises EIO when called concurrently for the same
                // address range, so disable for now.
                // https://github.com/torvalds/linux/blob/0c559323bbaabee7346c12e74b497e283aaafef5/include/uapi/linux/mempolicy.h#L48
                0,
            ))?;
        }

        Ok(())
    }

    pub fn set_mempolicy(&self) -> crate::Result<()> {
        // Call syscall to avoid external C dependency on `libnuma`.
        //
        // https://man7.org/linux/man-pages/man2/set_mempolicy.2.html
        unsafe fn set_mempolicy_syscall(
            mode: libc::c_int,
            mask: *const libc::c_ulong,
            maxnode: libc::c_ulong,
        ) -> i64 {
            unsafe { libc::syscall(libc::SYS_set_mempolicy, mode, mask, maxnode) }
        }

        let (mode, mask) = self.to_mode_mask();

        unsafe {
            try_libc!(set_mempolicy_syscall(mode, &mask, 64,))?;
        }

        Ok(())
    }

    fn to_mode_mask(&self) -> (libc::c_int, libc::c_ulong) {
        let (mode, mask) = match self {
            Numa::Bind { node } => (libc::MPOL_BIND, 1u64 << node),
            Numa::Interleave { nodes } => (
                libc::MPOL_INTERLEAVE,
                nodes.iter().map(|node| 1u64 << node).fold(0, |l, r| l | r),
            ),
        };

        (mode | libc::MPOL_F_STATIC_NODES, mask)
    }
}
