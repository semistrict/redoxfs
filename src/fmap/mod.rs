//! Page cache for memory-mapped files.
//!
//! Extracted from the Redox scheme (`mount/redox/resource.rs`), where the
//! kernel satisfies `mmap` of a RedoxFS file by borrowing pages from this
//! cache: it calls [`fmap`] (the scheme's `mmap_prep`) to get the address of a
//! file range, maps those pages into the process, and calls [`funmap`] (the
//! scheme's `munmap`) when they are unmapped. virtio-fs DAX has the same
//! shape: `FUSE_SETUPMAPPING` asks for a file range to install into the
//! guest's DAX window and `FUSE_REMOVEMAPPING` takes it away, so the same
//! cache serves both.
//!
//! Each mapped file has one [`FileMmapInfo`]: a buffer whose byte `i` caches
//! file byte `i`, and a range tree of [`Fmap`]s recording which ranges are
//! loaded and how many mappings reference each. Writable ranges are written
//! back to the filesystem when unmapped or synced.
//!
//! Changes from the Redox original, all needed once a guest can observe the
//! buffer while ordinary reads and writes also reach the file:
//!
//! - [`write`] and [`truncate`] update the buffer instead of invalidating it
//!   (the Redox scheme bumped a version, so mapped pages went stale).
//! - [`read`] overlays cached ranges instead of requiring full coverage, so
//!   unsynced writes through a mapping are always visible.
//! - [`Fmap::sync`] never writes past the end of the file; mappings may
//!   extend beyond it, and writing them back would grow the file.
//! - The buffer is released once nothing uses it, rather than kept as a read
//!   cache until a write made it stale.
//! - Buffer memory comes from [`FmapBuffer`], which can also back a guest.
//! - Ranges are tracked in [`ranges::RangeMap`] instead of the `range-tree`
//!   crate, whose removal of partly stored ranges was broken.

use alloc::collections::BTreeMap;
use core::ops::Range;
use core::{ptr, slice};
use std::time::{SystemTime, UNIX_EPOCH};

use syscall::error::{Error, Result, EBADFD};
use syscall::flag::{MapFlags, PROT_WRITE};
use syscall::PAGE_SIZE;

use crate::{Disk, Node, Transaction, TreePtr};

pub use self::buffer::FmapBuffer;
use self::ranges::{RangeMap, Value};

mod buffer;
pub(crate) mod ranges;
#[cfg(test)]
mod tests;

/// Cached mappings of every mapped file, by node id.
pub type Fmaps = BTreeMap<u32, FileMmapInfo>;

#[derive(Debug)]
pub struct Fmap {
    rc: usize,
    flags: MapFlags,
    last_page_tail: u16,
}

impl Fmap {
    /// Load `unaligned_size` bytes at `offset` into the buffer at `base`.
    ///
    /// # Safety
    /// `base` must be valid for writes of `offset + unaligned_size` bytes.
    pub unsafe fn new<D: Disk>(
        node_ptr: TreePtr<Node>,
        flags: MapFlags,
        unaligned_size: usize,
        offset: u64,
        base: *mut u8,
        tx: &mut Transaction<D>,
    ) -> Result<Self> {
        let address = base.add(offset as usize);

        // Read buffer from disk
        let atime = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

        let buf = slice::from_raw_parts_mut(address, unaligned_size);

        let count = tx.read_node(node_ptr, offset, buf, atime.as_secs(), atime.subsec_nanos())?;

        // Make sure remaining data is zeroed
        buf[count..].fill(0_u8);

        Ok(Self {
            rc: 1,
            flags,
            last_page_tail: (unaligned_size % PAGE_SIZE) as u16,
        })
    }

    /// Write `size` bytes at `offset` back to the file, if mapped writable.
    ///
    /// # Safety
    /// `base` must be valid for reads of `offset + size` bytes.
    pub unsafe fn sync<D: Disk>(
        &mut self,
        node_ptr: TreePtr<Node>,
        base: *mut u8,
        offset: u64,
        size: usize,
        tx: &mut Transaction<D>,
    ) -> Result<()> {
        if self.flags & PROT_WRITE == PROT_WRITE {
            // Mappings may extend past the end of the file; never grow it.
            let file_size = tx.read_tree(node_ptr)?.data().size();
            let size = size.min(file_size.saturating_sub(offset) as usize);
            if size == 0 {
                return Ok(());
            }

            let mtime = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
            tx.write_node(
                node_ptr,
                offset,
                unsafe { core::slice::from_raw_parts(base.add(offset as usize), size) },
                mtime.as_secs(),
                mtime.subsec_nanos(),
            )?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct FileMmapInfo {
    buffer: Option<FmapBuffer>,
    pub ranges: RangeMap<Fmap>,
    pub open_fds: usize,
}

impl Default for FileMmapInfo {
    fn default() -> Self {
        Self::new()
    }
}

impl FileMmapInfo {
    pub fn new() -> Self {
        Self {
            buffer: None,
            ranges: RangeMap::new(),
            open_fds: 0,
        }
    }

    pub fn in_use(&self) -> bool {
        self.open_fds > 0 || self.mapped()
    }

    /// Whether any range is currently mapped.
    pub fn mapped(&self) -> bool {
        self.ranges.iter().any(|(_, fmap)| fmap.rc > 0)
    }

    /// The buffer caching this file, if any range has been loaded.
    pub fn buffer(&self) -> Option<&FmapBuffer> {
        self.buffer.as_ref()
    }

    /// Copy `data` into the loaded parts of `offset..offset + data.len()`.
    fn update(&mut self, offset: u64, data: Option<&[u8]>, len: usize) {
        let Some(buffer) = &self.buffer else {
            return;
        };
        let end = offset + len as u64;
        for (range, _) in self.ranges.conflicts(offset..end) {
            let start = range.start.max(offset);
            let stop = range.end.min(end);
            let dst = unsafe { buffer.base().add(start as usize) };
            let count = (stop - start) as usize;
            match data {
                Some(data) => unsafe {
                    ptr::copy_nonoverlapping(
                        data.as_ptr().add((start - offset) as usize),
                        dst,
                        count,
                    )
                },
                None => unsafe { ptr::write_bytes(dst, 0, count) },
            }
        }
    }
}

impl Drop for FileMmapInfo {
    fn drop(&mut self) {
        if self.in_use() {
            #[cfg(feature = "log")]
            log::error!("FileMmapInfo dropped while in use");
        }
    }
}

/// Read file data, preferring cached (possibly not yet written back) pages.
pub fn read<D: Disk>(
    fmaps: &mut Fmaps,
    node_ptr: TreePtr<Node>,
    buf: &mut [u8],
    offset: u64,
    tx: &mut Transaction<D>,
) -> Result<usize> {
    let atime = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let count = tx.read_node(node_ptr, offset, buf, atime.as_secs(), atime.subsec_nanos())?;

    if let Some(fmap_info) = fmaps.get(&node_ptr.id()) {
        if let Some(buffer) = &fmap_info.buffer {
            let end = offset + count as u64;
            for (range, _) in fmap_info.ranges.conflicts(offset..end) {
                let start = range.start.max(offset);
                let stop = range.end.min(end);
                unsafe {
                    ptr::copy_nonoverlapping(
                        buffer.base().add(start as usize),
                        buf.as_mut_ptr().add((start - offset) as usize),
                        (stop - start) as usize,
                    );
                }
            }
        }
    }

    Ok(count)
}

/// Write file data, keeping cached pages coherent.
pub fn write<D: Disk>(
    fmaps: &mut Fmaps,
    node_ptr: TreePtr<Node>,
    buf: &[u8],
    offset: u64,
    tx: &mut Transaction<D>,
) -> Result<usize> {
    let old_size = tx.read_tree(node_ptr)?.data().size();
    let mtime = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let count = tx.write_node(node_ptr, offset, buf, mtime.as_secs(), mtime.subsec_nanos())?;

    if let Some(fmap_info) = fmaps.get_mut(&node_ptr.id()) {
        // Bytes past the old end of file read as zero, whatever a mapping left there.
        if offset > old_size {
            fmap_info.update(old_size, None, (offset - old_size) as usize);
        }
        fmap_info.update(offset, Some(&buf[..count]), count);
    }

    Ok(count)
}

/// Change the file size, keeping cached pages coherent.
pub fn truncate<D: Disk>(
    fmaps: &mut Fmaps,
    node_ptr: TreePtr<Node>,
    size: u64,
    tx: &mut Transaction<D>,
) -> Result<()> {
    let old_size = tx.read_tree(node_ptr)?.data().size();
    let mtime = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    tx.truncate_node(node_ptr, size, mtime.as_secs(), mtime.subsec_nanos())?;

    if let Some(fmap_info) = fmaps.get_mut(&node_ptr.id()) {
        let start = old_size.min(size);
        let end = old_size.max(size);
        fmap_info.update(start, None, (end - start) as usize);
    }

    Ok(())
}

/// Load `offset..offset + unaligned_size` into the cache and take a
/// reference to it, returning the address of `offset` in the buffer.
///
/// The address stays valid until the matching [`funmap`] on Redox and macOS.
/// On Linux the buffer may move; map [`FmapBuffer::fd`] at `offset` instead.
pub fn fmap<D: Disk>(
    fmaps: &mut Fmaps,
    node_ptr: TreePtr<Node>,
    flags: MapFlags,
    unaligned_size: usize,
    offset: u64,
    tx: &mut Transaction<D>,
) -> Result<*mut u8> {
    let aligned_size = unaligned_size.next_multiple_of(PAGE_SIZE);

    // TODO: PROT_EXEC? It is however unenforcable without restricting anonymous mmap, since a
    // program can always map anonymous RW-, read from a file, then remap as R-E. But it might
    // be usable as a hint, prohibiting direct executable mmaps at least.

    let fmap_info = fmaps.entry(node_ptr.id()).or_default();

    let was_in_use = fmap_info.in_use();
    if !was_in_use {
        // Notify filesystem of open
        tx.on_open_node(node_ptr)?;
    }

    let result = fmap_inner(fmap_info, node_ptr, flags, aligned_size, offset, tx);

    if result.is_err() && !was_in_use {
        // Undo the open, the mapping never happened
        tx.on_close_node(node_ptr)?;
        fmaps.remove(&node_ptr.id());
    }

    result
}

fn fmap_inner<D: Disk>(
    fmap_info: &mut FileMmapInfo,
    node_ptr: TreePtr<Node>,
    flags: MapFlags,
    aligned_size: usize,
    offset: u64,
    tx: &mut Transaction<D>,
) -> Result<*mut u8> {
    let new_size = (offset as usize + aligned_size).next_multiple_of(PAGE_SIZE);
    match &mut fmap_info.buffer {
        Some(buffer) => {
            if new_size > buffer.size() {
                let movable = !fmap_info.ranges.iter().any(|(_, fmap)| fmap.rc > 0);
                buffer.grow(new_size, movable)?;
            }
        }
        None => {
            let file_size = tx.read_tree(node_ptr)?.data().size();
            fmap_info.buffer = Some(FmapBuffer::new(new_size, file_size)?);
        }
    }
    let base = fmap_info.buffer.as_ref().unwrap().base();

    let affected_fmaps =
        remove_and_unused(&mut fmap_info.ranges, offset..offset + aligned_size as u64);

    let mut result = Ok(());
    for (range, v_opt) in affected_fmaps {
        let range_size = range.end - range.start;
        let fmap = match v_opt {
            Some(mut fmap) => {
                fmap.rc += 1;
                fmap.flags |= flags;
                Ok(fmap)
            }
            None if result.is_ok() => unsafe {
                Fmap::new(node_ptr, flags, range_size as usize, range.start, base, tx)
            },
            // Loading an earlier range failed; leave the rest unloaded.
            None => continue,
        };
        match fmap {
            Ok(fmap) => {
                //FIXME: Use result?
                let _ = fmap_info.ranges.insert(range.start, range_size, fmap);
            }
            Err(err) => result = Err(err),
        }
    }

    if let Err(err) = result {
        // Drop the references taken above so the failed mapping leaves no trace.
        release(fmap_info, offset..offset + aligned_size as u64);
        return Err(err);
    }

    Ok(unsafe { base.add(offset as usize) })
}

/// Drop one reference to every loaded range in `range`, without syncing.
fn release(fmap_info: &mut FileMmapInfo, range: Range<u64>) {
    for (range, mut fmap) in fmap_info.ranges.remove(range) {
        fmap.rc = fmap.rc.saturating_sub(1);
        let _ = fmap_info
            .ranges
            .insert(range.start, range.end - range.start, fmap);
    }
}

/// Remove `range` from `ranges`, returning the removed parts along with the
/// unloaded gaps between them, in order.
fn remove_and_unused(
    ranges: &mut RangeMap<Fmap>,
    range: Range<u64>,
) -> Vec<(Range<u64>, Option<Fmap>)> {
    let mut ret = Vec::new();
    let mut next = range.start;
    for (part, fmap) in ranges.remove(range.clone()) {
        if part.start > next {
            ret.push((next..part.start, None));
        }
        next = part.end;
        ret.push((part, Some(fmap)));
    }
    if next < range.end {
        ret.push((next..range.end, None));
    }
    ret
}

/// Release a reference taken by [`fmap`], writing back writable pages.
pub fn funmap<D: Disk>(
    fmaps: &mut Fmaps,
    node_ptr: TreePtr<Node>,
    offset: u64,
    size: usize,
    tx: &mut Transaction<D>,
) -> Result<()> {
    let fmap_info = fmaps.get_mut(&node_ptr.id()).ok_or(Error::new(EBADFD))?;
    let base = fmap_info.buffer.as_ref().ok_or(Error::new(EBADFD))?.base();

    let affected_fmaps = fmap_info.ranges.remove(offset..offset + size as u64);

    let mut result = Ok(());
    for (range, mut fmap) in affected_fmaps {
        fmap.rc = fmap.rc.checked_sub(1).unwrap();

        //log::info!("SYNCING {}..{}", range.start, range.end);
        if result.is_ok() {
            result = unsafe {
                fmap.sync(
                    node_ptr,
                    base,
                    range.start,
                    (range.end - range.start) as usize,
                    tx,
                )
            };
        }

        //FIXME: Use result?
        let _ = fmap_info
            .ranges
            .insert(range.start, range.end - range.start, fmap);
    }
    // Keep unsynced pages cached so a later sync can retry
    result?;

    // Allow release of node if not in use anymore
    if !fmap_info.in_use() {
        // Notify filesystem of close
        tx.on_close_node(node_ptr)?;

        fmaps.remove(&node_ptr.id());
    }

    Ok(())
}

/// Write back every writable cached range of a file.
pub fn sync<D: Disk>(
    fmaps: &mut Fmaps,
    node_ptr: TreePtr<Node>,
    tx: &mut Transaction<D>,
) -> Result<()> {
    if let Some(fmap_info) = fmaps.get_mut(&node_ptr.id()) {
        if let Some(buffer) = &fmap_info.buffer {
            let base = buffer.base();
            for (range, fmap) in fmap_info.ranges.iter_mut() {
                unsafe {
                    fmap.sync(
                        node_ptr,
                        base,
                        range.start,
                        (range.end - range.start) as usize,
                        tx,
                    )?;
                }
            }
        }
    }

    Ok(())
}

impl Value for Fmap {
    fn try_merge_forward(self, other: &Self) -> core::result::Result<Self, Self> {
        if self.rc == other.rc && self.flags == other.flags && self.last_page_tail == 0 {
            Ok(self)
        } else {
            Err(self)
        }
    }
    fn try_merge_backwards(self, other: &Self) -> core::result::Result<Self, Self> {
        if self.rc == other.rc && self.flags == other.flags && other.last_page_tail == 0 {
            Ok(self)
        } else {
            Err(self)
        }
    }
    #[allow(unused_variables)]
    fn split(
        self,
        prev_range: Option<Range<u64>>,
        range: Range<u64>,
        next_range: Option<Range<u64>>,
    ) -> (Option<Self>, Self, Option<Self>) {
        (
            prev_range.map(|_range| Fmap {
                rc: self.rc,
                flags: self.flags,
                last_page_tail: 0,
            }),
            Fmap {
                rc: self.rc,
                flags: self.flags,
                last_page_tail: if next_range.is_none() {
                    self.last_page_tail
                } else {
                    0
                },
            },
            next_range.map(|_range| Fmap {
                rc: self.rc,
                flags: self.flags,
                last_page_tail: self.last_page_tail,
            }),
        )
    }
}
