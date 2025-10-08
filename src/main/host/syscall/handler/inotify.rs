use linux_api::errno::Errno;
use nix::sys::eventfd::EfdFlags;

use crate::host::descriptor::descriptor_table::DescriptorHandle;
use crate::host::syscall::handler::{SyscallContext, SyscallHandler};
use crate::host::syscall::type_formatting::SyscallStringArg;
use shadow_shim_helper_rs::syscall_types::ForeignPtr;

impl SyscallHandler {
    log_syscall!(
        inotify_init1,
        /* rv */ std::ffi::c_int,
        /* flags */ std::ffi::c_int,
    );
    pub fn inotify_init1(ctx: &mut SyscallContext, flags: std::ffi::c_int) -> Result<DescriptorHandle, Errno> {
        // Map a subset of inotify flags to eventfd flags for minimal behavior.
        let mut efd_flags = 0;
        // IN_NONBLOCK and IN_CLOEXEC share values with O_NONBLOCK/O_CLOEXEC on Linux.
        const IN_NONBLOCK: i32 = libc::IN_NONBLOCK as i32;
        const IN_CLOEXEC: i32 = libc::IN_CLOEXEC as i32;
        if (flags & IN_NONBLOCK) != 0 { efd_flags |= EfdFlags::EFD_NONBLOCK.bits(); }
        if (flags & IN_CLOEXEC) != 0 { efd_flags |= EfdFlags::EFD_CLOEXEC.bits(); }

        // Create an eventfd to represent the inotify instance; it'll remain non-readable
        // unless we later implement delivering events.
        Self::eventfd_helper(ctx, 0, efd_flags)
    }

    log_syscall!(
        inotify_add_watch,
        /* rv */ std::ffi::c_int,
        /* fd */ std::ffi::c_int,
        /* pathname */ SyscallStringArg,
        /* mask */ libc::c_uint,
    );
    pub fn inotify_add_watch(
        _ctx: &mut SyscallContext,
        _fd: std::ffi::c_int,
        _pathname: ForeignPtr<()>,
        _mask: libc::c_uint,
    ) -> Result<std::ffi::c_int, Errno> {
        // Minimal stub: succeed and return a fake positive watch descriptor.
        Ok(1)
    }

    log_syscall!(
        inotify_rm_watch,
        /* rv */ std::ffi::c_int,
        /* fd */ std::ffi::c_int,
        /* wd */ std::ffi::c_int,
    );
    pub fn inotify_rm_watch(
        _ctx: &mut SyscallContext,
        _fd: std::ffi::c_int,
        _wd: std::ffi::c_int,
    ) -> Result<std::ffi::c_int, Errno> {
        // Minimal stub: succeed.
        Ok(0)
    }
}


