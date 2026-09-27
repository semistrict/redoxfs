//! virtio-fs DAX windows.
//!
//! A DAX window is a region of guest physical memory that the VMM fills with
//! file pages on `FUSE_SETUPMAPPING` and clears on `FUSE_REMOVEMAPPING`. The
//! pages come from the [`fmap`](crate::fmap) cache, exactly as the Redox
//! kernel borrows them into an address space on `mmap`. [`Grant`]s play the
//! part of the kernel's `FmapBorrowed` grants: they remember which file range
//! backs each part of the window, so that removing (part of) a mapping
//! releases (part of) the cache reference taken for it.

use core::marker::PhantomData;
use core::ops::Range;
use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::BorrowedFd;

use crate::fmap::ranges::Value;

/// The VMM side of a DAX window: installs cache pages into guest memory.
pub trait DaxWindow {
    /// Make `source` visible to the guest at `window_offset` bytes into the
    /// window, replacing whatever was mapped there.
    fn map(&self, window_offset: u64, source: &MapSource<'_>, writable: bool) -> io::Result<()>;

    /// Remove `len` bytes of mappings at `window_offset` bytes into the window.
    ///
    /// Once this returns, the guest must no longer be able to reach the pages
    /// that were mapped there: they may be freed.
    fn unmap(&self, window_offset: u64, len: u64) -> io::Result<()>;
}

/// Cache pages to install into a DAX window.
///
/// Valid until the matching unmap; the cache keeps the pages alive until then.
pub struct MapSource<'a> {
    addr: *mut u8,
    len: u64,
    #[cfg(target_os = "linux")]
    fd: BorrowedFd<'a>,
    #[cfg(target_os = "linux")]
    fd_offset: u64,
    _cache: PhantomData<&'a ()>,
}

impl<'a> MapSource<'a> {
    pub(crate) fn new(
        addr: *mut u8,
        len: u64,
        #[cfg(target_os = "linux")] fd: BorrowedFd<'a>,
        #[cfg(target_os = "linux")] fd_offset: u64,
    ) -> Self {
        Self {
            addr,
            len,
            #[cfg(target_os = "linux")]
            fd,
            #[cfg(target_os = "linux")]
            fd_offset,
            _cache: PhantomData,
        }
    }

    /// Host address of the pages.
    ///
    /// Hypervisors that map host memory by address (e.g. `hv_vm_map` on
    /// macOS) use this. On Linux the cache may move its own mapping, so map
    /// [`fd`](Self::fd) instead.
    pub fn addr(&self) -> *mut u8 {
        self.addr
    }

    /// Length in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// File descriptor holding the pages, to `mmap` with `MAP_SHARED` at
    /// [`fd_offset`](Self::fd_offset).
    #[cfg(target_os = "linux")]
    pub fn fd(&self) -> BorrowedFd<'a> {
        self.fd
    }

    #[cfg(target_os = "linux")]
    pub fn fd_offset(&self) -> u64 {
        self.fd_offset
    }
}

/// The file range backing part of a DAX window.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Grant {
    pub node: u32,
    pub file_offset: u64,
}

impl Value for Grant {
    fn try_merge_backwards(self, _other: &Self) -> Result<Self, Self> {
        // Each grant is released separately, as it was taken.
        Err(self)
    }

    fn try_merge_forward(self, _other: &Self) -> Result<Self, Self> {
        Err(self)
    }

    fn split(
        self,
        prev_range: Option<Range<u64>>,
        range: Range<u64>,
        next_range: Option<Range<u64>>,
    ) -> (Option<Self>, Self, Option<Self>) {
        let start = prev_range.as_ref().map_or(range.start, |prev| prev.start);
        let at = |window_offset: u64| Grant {
            node: self.node,
            file_offset: self.file_offset + (window_offset - start),
        };
        (
            prev_range.map(|prev| at(prev.start)),
            at(range.start),
            next_range.map(|next| at(next.start)),
        )
    }
}
