//! A FUSE-shaped view of a RedoxFS filesystem, shared by the FUSE mount and
//! virtio-fs servers.
//!
//! [`Vfs`] speaks in inode numbers and lookup counts, as FUSE does, and keeps
//! every file operation coherent with the [`fmap`] page cache so that the
//! same files can be read and written while mapped into a virtio-fs DAX
//! window (see [`Vfs::setup_mapping`]).
//!
//! Inode numbers are node ids; the root directory is inode 1, which is also
//! `FUSE_ROOT_ID`. A node stays alive (even when unlinked) while the kernel
//! holds lookups on it or any of its pages are mapped.

use alloc::sync::Arc;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use syscall::error::{Error, Result, EINVAL, EISDIR, ENAMETOOLONG, ENOTDIR, EPERM};
use syscall::flag::{PROT_READ, PROT_WRITE};
use syscall::PAGE_SIZE;

use crate::fmap::{self, ranges::RangeMap, Fmaps};
use crate::{Disk, FileSystem, Node, TreeData, TreePtr, BLOCK_SIZE, DIR_ENTRY_MAX_LENGTH};

use self::dax::Grant;
pub use self::dax::{DaxWindow, MapSource};

mod dax;
#[cfg(test)]
mod tests;

/// Inode number of the root directory.
pub const ROOT_INO: u64 = 1;

/// Fail if the target of a rename exists.
pub const RENAME_NOREPLACE: u32 = 1 << 0;

const S_ISUID: u16 = 0o4000;
const S_ISGID: u16 = 0o2000;
const S_IXGRP: u16 = 0o0010;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    File,
    Directory,
    Symlink,
    Socket,
}

impl FileKind {
    fn of(node: &Node) -> Self {
        match node.mode() & Node::MODE_TYPE {
            Node::MODE_DIR => Self::Directory,
            Node::MODE_SYMLINK => Self::Symlink,
            Node::MODE_SOCK => Self::Socket,
            _ => Self::File,
        }
    }

    /// The `S_IFMT` bits for this kind.
    pub fn mode_type(self) -> u32 {
        (match self {
            Self::File => Node::MODE_FILE,
            Self::Directory => Node::MODE_DIR,
            Self::Symlink => Node::MODE_SYMLINK,
            Self::Socket => Node::MODE_SOCK,
        }) as u32
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timespec {
    pub secs: u64,
    pub nanos: u32,
}

impl Timespec {
    fn now() -> Self {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        Self {
            secs: now.as_secs(),
            nanos: now.subsec_nanos(),
        }
    }
}

impl From<(u64, u32)> for Timespec {
    fn from((secs, nanos): (u64, u32)) -> Self {
        Self { secs, nanos }
    }
}

/// Attributes of an inode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attr {
    pub ino: u64,
    pub size: u64,
    /// Allocated size in 512-byte units.
    pub blocks: u64,
    pub atime: Timespec,
    pub mtime: Timespec,
    pub ctime: Timespec,
    pub kind: FileKind,
    /// Permission bits, including setuid, setgid and sticky.
    pub perm: u16,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    /// Preferred I/O size.
    pub blksize: u32,
}

impl Attr {
    fn of(node: &TreeData<Node>) -> Self {
        let data = node.data();
        Self {
            ino: node.id() as u64,
            size: data.size(),
            blocks: data.blocks() * (BLOCK_SIZE / 512),
            atime: data.atime().into(),
            mtime: data.mtime().into(),
            ctime: data.ctime().into(),
            kind: FileKind::of(data),
            perm: data.mode() & Node::MODE_PERM,
            nlink: data.links(),
            uid: data.uid(),
            gid: data.gid(),
            blksize: BLOCK_SIZE as u32,
        }
    }

    /// `st_mode`: file type and permission bits.
    pub fn mode(&self) -> u32 {
        self.kind.mode_type() | self.perm as u32
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetTime {
    Now,
    At(Timespec),
}

impl SetTime {
    fn resolve(self) -> Timespec {
        match self {
            Self::Now => Timespec::now(),
            Self::At(time) => time,
        }
    }
}

/// Attribute changes; `None` leaves an attribute unchanged.
#[derive(Clone, Copy, Debug, Default)]
pub struct SetAttr {
    /// New permission bits (the file type cannot change).
    pub perm: Option<u16>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub atime: Option<SetTime>,
    pub mtime: Option<SetTime>,
    /// Clear setuid, and setgid if group-executable, as a write or chown by
    /// an unprivileged process does. Applied before `perm`.
    pub kill_suid_sgid: bool,
}

/// Credentials of the process creating a node, which will own it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Creds {
    pub uid: u32,
    pub gid: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub ino: u64,
    pub kind: FileKind,
    pub name: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatFs {
    pub block_size: u32,
    pub blocks: u64,
    pub blocks_free: u64,
    pub name_max: u32,
}

/// An open directory: a snapshot of its entries, including `.` and `..`.
struct OpenDir {
    ino: u64,
    entries: Arc<[DirEntry]>,
}

struct State<D: Disk> {
    fs: FileSystem<D>,
    fmaps: Fmaps,
    /// Kernel lookup count of each remembered inode.
    lookups: alloc::collections::BTreeMap<u32, u64>,
    dirs: alloc::collections::BTreeMap<u64, OpenDir>,
    next_handle: u64,
    /// What backs each mapped part of the DAX window.
    grants: RangeMap<Grant>,
}

/// A RedoxFS filesystem served by inode number. See the module docs.
pub struct Vfs<D: Disk> {
    state: Mutex<State<D>>,
}

fn node_ptr(ino: u64) -> Result<TreePtr<Node>> {
    u32::try_from(ino)
        .ok()
        .filter(|&id| id != 0)
        .map(TreePtr::new)
        .ok_or(Error::new(EINVAL))
}

fn name_str(name: &[u8]) -> Result<&str> {
    if name.len() > DIR_ENTRY_MAX_LENGTH {
        return Err(Error::new(ENAMETOOLONG));
    }
    core::str::from_utf8(name).map_err(|_| Error::new(EINVAL))
}

fn page_aligned(value: u64) -> bool {
    value.is_multiple_of(PAGE_SIZE as u64)
}

impl<D: Disk> Vfs<D> {
    pub fn new(fs: FileSystem<D>) -> Self {
        Self {
            state: Mutex::new(State {
                fs,
                fmaps: Fmaps::new(),
                lookups: Default::default(),
                dirs: Default::default(),
                next_handle: 1,
                grants: RangeMap::new(),
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, State<D>> {
        self.state.lock().expect("vfs state poisoned")
    }

    /// Write back all mapped pages, release every lookup and mapping, and
    /// return the filesystem. Nothing may reference the DAX window anymore.
    pub fn unmount(self) -> Result<FileSystem<D>> {
        let mut state = self.state.into_inner().expect("vfs state poisoned");
        let State {
            fs, fmaps, grants, ..
        } = &mut state;

        let mut result = Ok(());
        for (range, grant) in grants.remove(0..u64::MAX) {
            let release = fs.tx(|tx| {
                fmap::funmap(
                    fmaps,
                    TreePtr::new(grant.node),
                    grant.file_offset,
                    (range.end - range.start) as usize,
                    tx,
                )
            });
            result = result.and(release);
        }
        result?;

        // Lookups die with the mount; release nodes that were only kept for them.
        state.fs.node_usages.clear();
        state.fs.cleanup()?;
        Ok(state.fs)
    }

    /// Remember one more kernel reference to `node`.
    fn remember(
        lookups: &mut alloc::collections::BTreeMap<u32, u64>,
        tx: &mut crate::Transaction<D>,
        node: TreePtr<Node>,
    ) -> Result<()> {
        let count = lookups.entry(node.id()).or_insert(0);
        if *count == 0 {
            tx.on_open_node(node)?;
        }
        *count += 1;
        Ok(())
    }

    /// Look up `name` in `parent`, taking a lookup reference on the result.
    pub fn lookup(&self, parent: u64, name: &[u8]) -> Result<Attr> {
        let parent = node_ptr(parent)?;
        let name = name_str(name)?;
        let state = &mut *self.state();
        let lookups = &mut state.lookups;
        state.fs.tx(|tx| {
            let node = tx.find_node(parent, name)?;
            Self::remember(lookups, tx, node.ptr())?;
            Ok(Attr::of(&node))
        })
    }

    /// Drop `count` lookup references to `ino`.
    pub fn forget(&self, ino: u64, count: u64) {
        let Ok(node) = node_ptr(ino) else {
            return;
        };
        let state = &mut *self.state();
        let Some(lookups) = state.lookups.get_mut(&node.id()) else {
            // The root is never remembered, and the kernel may forget it.
            return;
        };
        *lookups = lookups.saturating_sub(count);
        if *lookups == 0 {
            state.lookups.remove(&node.id());
            if let Err(err) = state.fs.tx(|tx| tx.on_close_node(node)) {
                #[cfg(feature = "log")]
                log::error!("failed to release node {}: {}", node.id(), err);
                let _ = err;
            }
        }
    }

    pub fn getattr(&self, ino: u64) -> Result<Attr> {
        let node = node_ptr(ino)?;
        self.state().fs.tx(|tx| Ok(Attr::of(&tx.read_tree(node)?)))
    }

    pub fn setattr(&self, ino: u64, set: SetAttr) -> Result<Attr> {
        let node_ptr = node_ptr(ino)?;
        let state = &mut *self.state();
        let fmaps = &mut state.fmaps;
        state.fs.tx(|tx| {
            if let Some(size) = set.size {
                if tx.read_tree(node_ptr)?.data().is_dir() {
                    return Err(Error::new(EISDIR));
                }
                fmap::truncate(fmaps, node_ptr, size, tx)?;
            }

            let mut node = tx.read_tree(node_ptr)?;
            let data = node.data_mut();
            let mut changed = false;
            if set.kill_suid_sgid {
                let mode = data.mode();
                let mut killed = mode & !S_ISUID;
                if mode & S_IXGRP != 0 {
                    killed &= !S_ISGID;
                }
                changed |= killed != mode;
                data.set_mode(killed);
            }
            if let Some(perm) = set.perm {
                data.set_mode((data.mode() & Node::MODE_TYPE) | (perm & Node::MODE_PERM));
            }
            if let Some(uid) = set.uid {
                data.set_uid(uid);
            }
            if let Some(gid) = set.gid {
                data.set_gid(gid);
            }
            if let Some(atime) = set.atime {
                let atime = atime.resolve();
                data.set_atime(atime.secs, atime.nanos);
            }
            if let Some(mtime) = set.mtime {
                let mtime = mtime.resolve();
                data.set_mtime(mtime.secs, mtime.nanos);
            }

            let attr = Attr::of(&node);
            if changed
                || set.perm.is_some()
                || set.uid.is_some()
                || set.gid.is_some()
                || set.atime.is_some()
                || set.mtime.is_some()
            {
                tx.sync_tree(node)?;
            }
            Ok(attr)
        })
    }

    pub fn readlink(&self, ino: u64) -> Result<Vec<u8>> {
        let node_ptr = node_ptr(ino)?;
        let state = &mut *self.state();
        let fmaps = &mut state.fmaps;
        state.fs.tx(|tx| {
            let node = tx.read_tree(node_ptr)?;
            if !node.data().is_symlink() {
                return Err(Error::new(EINVAL));
            }
            let mut target = vec![0; node.data().size() as usize];
            let count = fmap::read(fmaps, node_ptr, &mut target, 0, tx)?;
            target.truncate(count);
            Ok(target)
        })
    }

    /// Create a node of type `mode` (with permission bits) owned by `creds`,
    /// taking a lookup reference on it. `contents` becomes its data.
    fn create_node(
        &self,
        creds: Creds,
        parent: u64,
        name: &[u8],
        mode: u16,
        contents: &[u8],
    ) -> Result<Attr> {
        let parent_ptr = node_ptr(parent)?;
        let name = name_str(name)?;
        let state = &mut *self.state();
        let lookups = &mut state.lookups;
        state.fs.tx(|tx| {
            let parent = tx.read_tree(parent_ptr)?;
            if !parent.data().is_dir() {
                return Err(Error::new(ENOTDIR));
            }

            // New nodes belong to their creator, except that setgid
            // directories pass on their group (and setgid, to directories).
            let mut mode = mode;
            let gid = if parent.data().mode() & S_ISGID != 0 {
                if mode & Node::MODE_TYPE == Node::MODE_DIR {
                    mode |= S_ISGID;
                }
                parent.data().gid()
            } else {
                creds.gid
            };

            let now = Timespec::now();
            let mut node = tx.create_node(parent_ptr, name, mode, now.secs, now.nanos)?;
            node.data_mut().set_mode(mode);
            node.data_mut().set_uid(creds.uid);
            node.data_mut().set_gid(gid);
            let node_ptr = node.ptr();
            tx.sync_tree(node)?;

            if !contents.is_empty() {
                tx.write_node(node_ptr, 0, contents, now.secs, now.nanos)?;
            }
            Self::remember(lookups, tx, node_ptr)?;
            Ok(Attr::of(&tx.read_tree(node_ptr)?))
        })
    }

    /// Create a regular file (the FUSE `create` operation, minus the open).
    pub fn create(&self, creds: Creds, parent: u64, name: &[u8], perm: u16) -> Result<Attr> {
        self.create_node(
            creds,
            parent,
            name,
            Node::MODE_FILE | (perm & Node::MODE_PERM),
            &[],
        )
    }

    /// Create a node from a full `st_mode`. Only regular files and sockets
    /// are supported.
    pub fn mknod(&self, creds: Creds, parent: u64, name: &[u8], mode: u32) -> Result<Attr> {
        let kind = (mode as u16) & Node::MODE_TYPE;
        if kind != Node::MODE_FILE && kind != Node::MODE_SOCK {
            return Err(Error::new(EPERM));
        }
        self.create_node(creds, parent, name, mode as u16, &[])
    }

    pub fn mkdir(&self, creds: Creds, parent: u64, name: &[u8], perm: u16) -> Result<Attr> {
        self.create_node(
            creds,
            parent,
            name,
            Node::MODE_DIR | (perm & Node::MODE_PERM),
            &[],
        )
    }

    pub fn symlink(&self, creds: Creds, parent: u64, name: &[u8], target: &[u8]) -> Result<Attr> {
        self.create_node(creds, parent, name, Node::MODE_SYMLINK | 0o777, target)
    }

    /// Add a hard link to `ino`, taking a lookup reference on it.
    pub fn link(&self, ino: u64, new_parent: u64, new_name: &[u8]) -> Result<Attr> {
        let node_ptr = node_ptr(ino)?;
        let new_parent = self::node_ptr(new_parent)?;
        let new_name = name_str(new_name)?;
        let state = &mut *self.state();
        let lookups = &mut state.lookups;
        state.fs.tx(|tx| {
            if tx.read_tree(node_ptr)?.data().is_dir() {
                return Err(Error::new(EPERM));
            }
            tx.check_name(&new_parent, new_name)?;
            tx.link_node(new_parent, new_name, node_ptr)?;
            Self::remember(lookups, tx, node_ptr)?;
            Ok(Attr::of(&tx.read_tree(node_ptr)?))
        })
    }

    pub fn unlink(&self, parent: u64, name: &[u8]) -> Result<()> {
        self.remove(parent, name, Node::MODE_FILE)
    }

    pub fn rmdir(&self, parent: u64, name: &[u8]) -> Result<()> {
        self.remove(parent, name, Node::MODE_DIR)
    }

    fn remove(&self, parent: u64, name: &[u8], mode: u16) -> Result<()> {
        let parent = node_ptr(parent)?;
        let name = name_str(name)?;
        self.state()
            .fs
            .tx(|tx| tx.remove_node(parent, name, mode).map(|_| ()))
    }

    /// Rename, replacing an existing target of a compatible type unless
    /// `flags` has [`RENAME_NOREPLACE`]. Other flags are unsupported.
    pub fn rename(
        &self,
        parent: u64,
        name: &[u8],
        new_parent: u64,
        new_name: &[u8],
        flags: u32,
    ) -> Result<()> {
        if flags & !RENAME_NOREPLACE != 0 {
            return Err(Error::new(EINVAL));
        }
        let parent = node_ptr(parent)?;
        let name = name_str(name)?;
        let new_parent = node_ptr(new_parent)?;
        let new_name = name_str(new_name)?;
        self.state().fs.tx(|tx| {
            if flags & RENAME_NOREPLACE != 0 {
                return tx.rename_node_no_replace(parent, name, new_parent, new_name);
            }

            let orig = tx.find_node(parent, name)?;
            if let Ok(target) = tx.find_node(new_parent, new_name) {
                match (orig.data().is_dir(), target.data().is_dir()) {
                    (true, false) => return Err(Error::new(ENOTDIR)),
                    (false, true) => return Err(Error::new(EISDIR)),
                    _ => (),
                }
            }
            tx.rename_node(parent, name, new_parent, new_name)
        })
    }

    pub fn read(&self, ino: u64, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let node = node_ptr(ino)?;
        let state = &mut *self.state();
        let fmaps = &mut state.fmaps;
        state.fs.tx(|tx| fmap::read(fmaps, node, buf, offset, tx))
    }

    pub fn write(&self, ino: u64, offset: u64, data: &[u8]) -> Result<usize> {
        let node = node_ptr(ino)?;
        let state = &mut *self.state();
        let fmaps = &mut state.fmaps;
        state.fs.tx(|tx| fmap::write(fmaps, node, data, offset, tx))
    }

    /// Write back mapped pages of `ino` and flush the disk.
    pub fn fsync(&self, ino: u64) -> Result<()> {
        let node = node_ptr(ino)?;
        let state = &mut *self.state();
        let fmaps = &mut state.fmaps;
        state.fs.tx(|tx| fmap::sync(fmaps, node, tx))?;
        state.fs.disk.flush()
    }

    /// Write back all mapped pages and flush the disk.
    pub fn sync_all(&self) -> Result<()> {
        let state = &mut *self.state();
        let fmaps = &mut state.fmaps;
        let nodes: Vec<u32> = fmaps.keys().copied().collect();
        state.fs.tx(|tx| {
            nodes
                .into_iter()
                .try_for_each(|node| fmap::sync(fmaps, TreePtr::new(node), tx))
        })?;
        state.fs.disk.flush()
    }

    /// Open directory `ino`, returning a handle for [`readdir`](Self::readdir).
    pub fn opendir(&self, ino: u64) -> Result<u64> {
        let state = &mut *self.state();
        let entries = Self::list(&mut state.fs, ino)?;
        let handle = state.next_handle;
        state.next_handle += 1;
        state.dirs.insert(handle, OpenDir { ino, entries });
        Ok(handle)
    }

    /// Entries of an open directory; the entry at index `i` has offset
    /// `i + 1`. Reading from offset 0 (a rewind) refreshes the snapshot.
    pub fn readdir(&self, handle: u64, offset: u64) -> Result<Arc<[DirEntry]>> {
        let state = &mut *self.state();
        let dir = state.dirs.get_mut(&handle).ok_or(Error::new(EINVAL))?;
        if offset == 0 {
            dir.entries = Self::list(&mut state.fs, dir.ino)?;
        }
        Ok(dir.entries.clone())
    }

    pub fn releasedir(&self, handle: u64) {
        self.state().dirs.remove(&handle);
    }

    fn list(fs: &mut FileSystem<D>, ino: u64) -> Result<Arc<[DirEntry]>> {
        let dir = node_ptr(ino)?;
        fs.tx(|tx| {
            if !tx.read_tree(dir)?.data().is_dir() {
                return Err(Error::new(ENOTDIR));
            }
            let mut children = Vec::new();
            tx.child_nodes(dir, &mut children)?;

            let mut entries = Vec::with_capacity(children.len() + 2);
            for name in [&b"."[..], b".."] {
                entries.push(DirEntry {
                    ino,
                    kind: FileKind::Directory,
                    name: name.to_vec(),
                });
            }
            for child in children {
                let Some(name) = child.name() else {
                    continue;
                };
                let node = tx.read_tree(child.node_ptr())?;
                entries.push(DirEntry {
                    ino: node.id() as u64,
                    kind: FileKind::of(node.data()),
                    name: name.as_bytes().to_vec(),
                });
            }
            Ok(entries.into())
        })
    }

    pub fn statfs(&self) -> StatFs {
        let state = self.state();
        StatFs {
            block_size: BLOCK_SIZE as u32,
            blocks: state.fs.header.size() / BLOCK_SIZE,
            blocks_free: state.fs.allocator().free(),
            name_max: DIR_ENTRY_MAX_LENGTH as u32,
        }
    }

    /// Map `len` bytes of file `ino` at `file_offset` into a DAX window at
    /// `window_offset`, replacing any mappings there (`FUSE_SETUPMAPPING`).
    ///
    /// Offsets and length must be page aligned. Writes through a writable
    /// mapping reach the file when it is synced or unmapped.
    pub fn setup_mapping(
        &self,
        ino: u64,
        file_offset: u64,
        len: u64,
        writable: bool,
        window_offset: u64,
        window: &dyn DaxWindow,
    ) -> Result<()> {
        let node = node_ptr(ino)?;
        let window_range =
            window_offset..window_offset.checked_add(len).ok_or(Error::new(EINVAL))?;
        if len == 0
            || !page_aligned(file_offset)
            || !page_aligned(len)
            || !page_aligned(window_offset)
            || file_offset.checked_add(len).is_none()
        {
            return Err(Error::new(EINVAL));
        }
        let flags = if writable {
            PROT_READ | PROT_WRITE
        } else {
            PROT_READ
        };

        let state = &mut *self.state();
        let State {
            fs, fmaps, grants, ..
        } = state;

        let addr = fs.tx(|tx| {
            if !tx.read_tree(node)?.data().is_file() {
                return Err(Error::new(EINVAL));
            }
            fmap::fmap(fmaps, node, flags, len as usize, file_offset, tx)
        })?;

        // Clear the window first: hypervisors may refuse to map over
        // existing mappings.
        if let Err(err) = Self::unmap_window(grants, window, window_range.clone()) {
            fs.tx(|tx| fmap::funmap(fmaps, node, file_offset, len as usize, tx))?;
            return Err(err);
        }
        let replaced = grants.remove(window_range);

        let source = map_source(fmaps, node, addr, len, file_offset);
        let mapped = window
            .map(window_offset, &source, writable)
            .map_err(io_error);
        if mapped.is_ok() {
            let _ = grants.insert(
                window_offset,
                len,
                Grant {
                    node: node.id(),
                    file_offset,
                },
            );
        } else {
            fs.tx(|tx| fmap::funmap(fmaps, node, file_offset, len as usize, tx))?;
        }

        let released = Self::release_grants(fs, fmaps, replaced);
        mapped.and(released)
    }

    /// Remove `len` bytes of DAX mappings at `window_offset`, writing back
    /// writable pages (`FUSE_REMOVEMAPPING`).
    pub fn remove_mapping(
        &self,
        window_offset: u64,
        len: u64,
        window: &dyn DaxWindow,
    ) -> Result<()> {
        let window_range =
            window_offset..window_offset.checked_add(len).ok_or(Error::new(EINVAL))?;

        let state = &mut *self.state();
        let State {
            fs, fmaps, grants, ..
        } = state;

        Self::unmap_window(grants, window, window_range.clone())?;
        let removed = grants.remove(window_range);
        Self::release_grants(fs, fmaps, removed)
    }

    /// Unmap the granted parts of `range` from the window.
    fn unmap_window(
        grants: &RangeMap<Grant>,
        window: &dyn DaxWindow,
        range: core::ops::Range<u64>,
    ) -> Result<()> {
        for (granted, _) in grants.conflicts(range.clone()) {
            let start = granted.start.max(range.start);
            let end = granted.end.min(range.end);
            window.unmap(start, end - start).map_err(io_error)?;
        }
        Ok(())
    }

    /// Release the cache references held by `removed` grants.
    fn release_grants(
        fs: &mut FileSystem<D>,
        fmaps: &mut Fmaps,
        removed: Vec<(core::ops::Range<u64>, Grant)>,
    ) -> Result<()> {
        let mut result = Ok(());
        for (range, grant) in removed {
            let released = fs.tx(|tx| {
                fmap::funmap(
                    fmaps,
                    TreePtr::new(grant.node),
                    grant.file_offset,
                    (range.end - range.start) as usize,
                    tx,
                )
            });
            result = result.and(released);
        }
        result
    }
}

/// The cache pages of `node` at `file_offset`, which [`fmap::fmap`] loaded
/// at `addr`.
fn map_source(
    fmaps: &Fmaps,
    node: TreePtr<Node>,
    addr: *mut u8,
    len: u64,
    file_offset: u64,
) -> MapSource<'_> {
    #[cfg(target_os = "linux")]
    {
        let buffer = fmaps[&node.id()].buffer().expect("fmap loaded a buffer");
        MapSource::new(addr, len, buffer.fd(), file_offset)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (fmaps, node, file_offset);
        MapSource::new(addr, len)
    }
}

fn io_error(err: std::io::Error) -> Error {
    #[cfg(feature = "log")]
    log::error!("dax window: {err}");
    Error::new(err.raw_os_error().unwrap_or(syscall::error::EIO))
}
