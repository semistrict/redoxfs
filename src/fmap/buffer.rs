//! Memory holding the cached contents of one memory-mapped file.
//!
//! Byte `i` of the buffer caches byte `i` of the file. The buffer grows as
//! larger file offsets are mapped. On Redox this is the anonymous
//! mmap/mremap the scheme always used; on other hosts the memory must also be
//! mappable into a guest, which constrains how it may grow:
//!
//! - Linux: a memfd. A VMM maps `(fd, file offset)` into a DAX window, so the
//!   host address may move on growth (`mremap`) without disturbing guests.
//! - macOS: anonymous memory handed to the hypervisor by host address, so it
//!   must not move while mapped. It is reserved with headroom and grown in
//!   place when possible; otherwise it moves only when nothing maps it.

#[cfg(not(target_os = "redox"))]
use core::ptr;

use syscall::error::Result;
#[cfg(not(any(target_os = "redox", target_os = "linux")))]
use syscall::error::ENOMEM;
#[cfg(not(target_os = "redox"))]
use syscall::error::{Error, EIO};

pub struct FmapBuffer {
    base: *mut u8,
    size: usize,
    #[cfg(not(any(target_os = "redox", target_os = "linux")))]
    reserved: usize,
    #[cfg(target_os = "linux")]
    fd: std::os::fd::OwnedFd,
}

// SAFETY: the buffer owns its mapping; callers synchronize access.
unsafe impl Send for FmapBuffer {}
unsafe impl Sync for FmapBuffer {}

impl core::fmt::Debug for FmapBuffer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FmapBuffer")
            .field("base", &self.base)
            .field("size", &self.size)
            .finish()
    }
}

#[cfg(not(target_os = "redox"))]
fn last_os_error() -> Error {
    let err = std::io::Error::last_os_error();
    #[cfg(feature = "log")]
    log::error!("fmap buffer: {err}");
    Error::new(err.raw_os_error().unwrap_or(EIO))
}

impl FmapBuffer {
    /// Host address of the first byte of the buffer.
    pub fn base(&self) -> *mut u8 {
        self.base
    }

    /// Current size in bytes.
    pub fn size(&self) -> usize {
        self.size
    }

    /// The memfd backing the buffer; file offset `i` lives at fd offset `i`.
    #[cfg(target_os = "linux")]
    pub fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd;
        self.fd.as_fd()
    }
}

#[cfg(target_os = "redox")]
impl FmapBuffer {
    /// Allocate a zeroed buffer of `size` bytes. `_file_size` is a sizing hint.
    pub fn new(size: usize, _file_size: u64) -> Result<Self> {
        let base = unsafe {
            libredox::call::mmap(libredox::call::MmapArgs {
                length: size,
                // PRIVATE/SHARED doesn't matter once the pages are passed in the fmap
                // handler.
                prot: libredox::flag::PROT_READ | libredox::flag::PROT_WRITE,
                flags: libredox::flag::MAP_PRIVATE,

                offset: 0,
                fd: !0,
                addr: core::ptr::null_mut(),
            })? as *mut u8
        };
        Ok(Self { base, size })
    }

    /// Grow to `new_size` bytes, preserving contents. The kernel keeps pages
    /// already passed to fmap valid, so the buffer may always move.
    pub fn grow(&mut self, new_size: usize, _movable: bool) -> Result<()> {
        use syscall::flag::{PROT_READ, PROT_WRITE};

        self.base = unsafe {
            syscall::syscall5(
                syscall::SYS_MREMAP,
                self.base as usize,
                self.size,
                0,
                new_size,
                syscall::MremapFlags::empty().bits() | (PROT_READ | PROT_WRITE).bits(),
            )? as *mut u8
        };
        self.size = new_size;
        Ok(())
    }
}

#[cfg(target_os = "redox")]
impl Drop for FmapBuffer {
    fn drop(&mut self) {
        if let Err(e) = unsafe { libredox::call::munmap(self.base as *mut _, self.size) } {
            log::error!("Munmap error {e}");
        }
    }
}

#[cfg(target_os = "linux")]
impl FmapBuffer {
    /// Allocate a zeroed buffer of `size` bytes. `_file_size` is a sizing hint.
    pub fn new(size: usize, _file_size: u64) -> Result<Self> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

        // SAFETY: the name is NUL-terminated.
        let raw = unsafe { libc::memfd_create(c"redoxfs-fmap".as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            return Err(last_os_error());
        }
        // SAFETY: memfd_create returned a new descriptor that we own.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: fd is a valid memfd.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) } < 0 {
            return Err(last_os_error());
        }
        // SAFETY: a fresh shared mapping of our memfd.
        let base = unsafe {
            libc::mmap(
                ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(last_os_error());
        }
        Ok(Self {
            base: base.cast(),
            size,
            fd,
        })
    }

    /// Grow to `new_size` bytes, preserving contents. Guests map the memfd,
    /// not our address, so the buffer may always move.
    pub fn grow(&mut self, new_size: usize, _movable: bool) -> Result<()> {
        use std::os::fd::AsRawFd;

        // SAFETY: fd is a valid memfd.
        if unsafe { libc::ftruncate(self.fd.as_raw_fd(), new_size as libc::off_t) } < 0 {
            return Err(last_os_error());
        }
        // SAFETY: remapping our own mapping.
        let base =
            unsafe { libc::mremap(self.base.cast(), self.size, new_size, libc::MREMAP_MAYMOVE) };
        if base == libc::MAP_FAILED {
            return Err(last_os_error());
        }
        self.base = base.cast();
        self.size = new_size;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl Drop for FmapBuffer {
    fn drop(&mut self) {
        // SAFETY: unmapping our own mapping.
        if unsafe { libc::munmap(self.base.cast(), self.size) } < 0 {
            last_os_error();
        }
    }
}

#[cfg(not(any(target_os = "redox", target_os = "linux")))]
impl FmapBuffer {
    /// Allocate a zeroed buffer of `size` bytes, reserving room for the whole
    /// file so that mapping more of it rarely needs to grow the reservation.
    pub fn new(size: usize, file_size: u64) -> Result<Self> {
        let reserved = reservation(size.max(file_size as usize))?;
        let base = map_anonymous(reserved)?;
        Ok(Self {
            base,
            size,
            reserved,
        })
    }

    /// Grow to `new_size` bytes, preserving contents. The hypervisor maps our
    /// host address, so the buffer may only move when it is not `movable`
    /// (i.e. no guest maps it); otherwise growth must succeed in place.
    pub fn grow(&mut self, new_size: usize, movable: bool) -> Result<()> {
        if new_size <= self.reserved {
            self.size = new_size;
            return Ok(());
        }

        let reserved = reservation(new_size)?;
        if self.extend_in_place(reserved) {
            self.reserved = reserved;
            self.size = new_size;
            return Ok(());
        }
        if !movable {
            return Err(Error::new(ENOMEM));
        }

        let base = map_anonymous(reserved)?;
        // SAFETY: both regions are valid for `self.size` bytes and distinct.
        unsafe { ptr::copy_nonoverlapping(self.base, base, self.size) };
        unmap(self.base, self.reserved);
        self.base = base;
        self.reserved = reserved;
        self.size = new_size;
        Ok(())
    }

    /// Try to extend the reservation to `reserved` bytes without moving it.
    fn extend_in_place(&mut self, reserved: usize) -> bool {
        // Without MAP_FIXED the address is only a hint, honoured when the
        // range is free, so nothing already mapped there is clobbered.
        let hint = unsafe { self.base.add(self.reserved) };
        let len = reserved - self.reserved;
        match map_anonymous_at(hint, len) {
            Ok(addr) if addr == hint => true,
            Ok(addr) => {
                unmap(addr, len);
                false
            }
            Err(_) => false,
        }
    }
}

#[cfg(not(any(target_os = "redox", target_os = "linux")))]
impl Drop for FmapBuffer {
    fn drop(&mut self) {
        unmap(self.base, self.reserved);
    }
}

/// Smallest reservation. Untouched pages cost only address space, and
/// headroom lets buffers grow in place while mapped.
#[cfg(not(any(target_os = "redox", target_os = "linux")))]
const MIN_RESERVATION: usize = 64 << 20;

/// Round `size` up to the next power of two, at least [`MIN_RESERVATION`].
#[cfg(not(any(target_os = "redox", target_os = "linux")))]
fn reservation(size: usize) -> Result<usize> {
    size.max(MIN_RESERVATION)
        .checked_next_power_of_two()
        .ok_or(Error::new(ENOMEM))
}

#[cfg(not(any(target_os = "redox", target_os = "linux")))]
fn map_anonymous(len: usize) -> Result<*mut u8> {
    map_anonymous_at(ptr::null_mut(), len)
}

/// Map `len` zeroed bytes, at `hint` if that range is free.
#[cfg(not(any(target_os = "redox", target_os = "linux")))]
fn map_anonymous_at(hint: *mut u8, len: usize) -> Result<*mut u8> {
    // SAFETY: a fresh private mapping; without MAP_FIXED the kernel never
    // replaces existing mappings.
    let base = unsafe {
        libc::mmap(
            hint.cast(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if base == libc::MAP_FAILED {
        return Err(last_os_error());
    }
    Ok(base.cast())
}

#[cfg(not(any(target_os = "redox", target_os = "linux")))]
fn unmap(base: *mut u8, len: usize) {
    // SAFETY: unmapping a region we mapped.
    if unsafe { libc::munmap(base.cast(), len) } < 0 {
        last_os_error();
    }
}
