use syscall::data::{Stat, TimeSpec};
use syscall::dirent::{DirEntry, DirentBuf, DirentKind};
use syscall::error::{Error, Result, EBADF, EINVAL, EISDIR, ENOTDIR, EPERM};
use syscall::flag::{
    MapFlags, F_GETFL, F_SETFL, MODE_PERM, O_ACCMODE, O_APPEND, O_RDONLY, O_RDWR, O_WRONLY,
    PROT_READ, PROT_WRITE,
};
use syscall::ENOENT;

use crate::{fmap, Disk, Node, Transaction, TreePtr, BLOCK_SIZE};

pub use crate::fmap::{FileMmapInfo, Fmaps};

pub trait Resource<D: Disk> {
    fn parent_ptr_opt(&self) -> Option<TreePtr<Node>>;

    fn node_ptr(&self) -> TreePtr<Node>;

    fn uid(&self) -> u32;

    fn set_path(&mut self, path: &str);

    fn read(
        &mut self,
        fmaps: &mut Fmaps,
        buf: &mut [u8],
        offset: u64,
        tx: &mut Transaction<D>,
    ) -> Result<usize>;

    fn write(
        &mut self,
        fmaps: &mut Fmaps,
        buf: &[u8],
        offset: u64,
        tx: &mut Transaction<D>,
    ) -> Result<usize>;

    fn fsize(&mut self, tx: &mut Transaction<D>) -> Result<u64>;

    fn fmap(
        &mut self,
        fmaps: &mut Fmaps,
        flags: MapFlags,
        size: usize,
        offset: u64,
        tx: &mut Transaction<D>,
    ) -> Result<usize>;

    fn funmap(
        &mut self,
        fmaps: &mut Fmaps,
        offset: u64,
        size: usize,
        tx: &mut Transaction<D>,
    ) -> Result<()>;

    fn fchmod(&mut self, mode: u16, tx: &mut Transaction<D>) -> Result<()> {
        let mut node = tx.read_tree(self.node_ptr())?;

        if node.data().uid() == self.uid() || self.uid() == 0 {
            let old_mode = node.data().mode();
            let new_mode = (old_mode & !MODE_PERM) | (mode & MODE_PERM);
            if old_mode != new_mode {
                node.data_mut().set_mode(new_mode);
                tx.sync_tree(node)?;
            }

            Ok(())
        } else {
            Err(Error::new(EPERM))
        }
    }

    fn fchown(&mut self, uid: u32, gid: u32, tx: &mut Transaction<D>) -> Result<()> {
        let mut node = tx.read_tree(self.node_ptr())?;

        let old_uid = node.data().uid();
        if old_uid == self.uid() || self.uid() == 0 {
            let mut node_changed = false;

            if uid as i32 != -1 {
                if uid != old_uid {
                    node.data_mut().set_uid(uid);
                    node_changed = true;
                }
            }

            if gid as i32 != -1 {
                let old_gid = node.data().gid();
                if gid != old_gid {
                    node.data_mut().set_gid(gid);
                    node_changed = true;
                }
            }

            if node_changed {
                tx.sync_tree(node)?;
            }

            Ok(())
        } else {
            Err(Error::new(EPERM))
        }
    }

    fn fcntl(&mut self, cmd: usize, arg: usize) -> Result<usize>;

    fn path(&self) -> &str;

    fn redox_path(&self) -> redox_path::RedoxReference<'_> {
        // SAFETY: paths already stored is assumed to be valid
        unsafe { redox_path::RedoxReference::new_unchecked(self.path()) }
    }

    fn stat(&self, stat: &mut Stat, tx: &mut Transaction<D>) -> Result<()> {
        let node = tx.read_tree(self.node_ptr())?;

        let ctime = node.data().ctime();
        let mtime = node.data().mtime();
        let atime = node.data().atime();
        *stat = Stat {
            st_dev: 0, // TODO
            st_ino: node.id() as u64,
            st_mode: node.data().mode(),
            st_nlink: node.data().links(),
            st_uid: node.data().uid(),
            st_gid: node.data().gid(),
            st_size: node.data().size(),
            st_blksize: 512,
            // Blocks is in 512 byte blocks, not in our block size
            st_blocks: node.data().blocks() * (BLOCK_SIZE / 512),
            st_mtime: mtime.0,
            st_mtime_nsec: mtime.1,
            st_atime: atime.0,
            st_atime_nsec: atime.1,
            st_ctime: ctime.0,
            st_ctime_nsec: ctime.1,
        };

        Ok(())
    }

    fn sync(&mut self, fmaps: &mut Fmaps, tx: &mut Transaction<D>) -> Result<()>;

    fn truncate(&mut self, fmaps: &mut Fmaps, len: u64, tx: &mut Transaction<D>) -> Result<()>;

    fn utimens(&mut self, times: &[TimeSpec], tx: &mut Transaction<D>) -> Result<()>;

    fn getdents<'buf>(
        &mut self,
        buf: DirentBuf<&'buf mut [u8]>,
        opaque_offset: u64,
        tx: &mut Transaction<D>,
    ) -> Result<DirentBuf<&'buf mut [u8]>>;
}

pub struct Entry {
    pub node_ptr: TreePtr<Node>,
    pub name: String,
}

pub struct DirResource {
    path: String,
    parent_ptr_opt: Option<TreePtr<Node>>,
    node_ptr: TreePtr<Node>,
    data: Option<Vec<Entry>>,
    uid: u32,
}

impl DirResource {
    pub fn new(
        path: String,
        parent_ptr_opt: Option<TreePtr<Node>>,
        node_ptr: TreePtr<Node>,
        data: Option<Vec<Entry>>,
        uid: u32,
    ) -> DirResource {
        DirResource {
            path,
            parent_ptr_opt,
            node_ptr,
            data,
            uid,
        }
    }
}

impl<D: Disk> Resource<D> for DirResource {
    fn parent_ptr_opt(&self) -> Option<TreePtr<Node>> {
        self.parent_ptr_opt
    }

    fn node_ptr(&self) -> TreePtr<Node> {
        self.node_ptr
    }

    fn uid(&self) -> u32 {
        self.uid
    }

    fn set_path(&mut self, path: &str) {
        self.path = path.to_string();
    }

    fn read(
        &mut self,
        _fmaps: &mut Fmaps,
        _buf: &mut [u8],
        _offset: u64,
        _tx: &mut Transaction<D>,
    ) -> Result<usize> {
        Err(Error::new(EISDIR))
    }

    fn write(
        &mut self,
        _fmaps: &mut Fmaps,
        _buf: &[u8],
        _offset: u64,
        _tx: &mut Transaction<D>,
    ) -> Result<usize> {
        Err(Error::new(EBADF))
    }

    fn fsize(&mut self, _tx: &mut Transaction<D>) -> Result<u64> {
        Ok(self.data.as_ref().ok_or(Error::new(EBADF))?.len() as u64)
    }

    fn fmap(
        &mut self,
        _fmaps: &mut Fmaps,
        _flags: MapFlags,
        _size: usize,
        _offset: u64,
        _tx: &mut Transaction<D>,
    ) -> Result<usize> {
        Err(Error::new(EBADF))
    }
    fn funmap(
        &mut self,
        _fmaps: &mut Fmaps,
        _offset: u64,
        _size: usize,
        _tx: &mut Transaction<D>,
    ) -> Result<()> {
        Err(Error::new(EBADF))
    }

    fn fcntl(&mut self, _cmd: usize, _arg: usize) -> Result<usize> {
        Err(Error::new(EBADF))
    }

    fn path(&self) -> &str {
        &self.path
    }

    fn sync(&mut self, _fmaps: &mut Fmaps, _tx: &mut Transaction<D>) -> Result<()> {
        Err(Error::new(EBADF))
    }

    fn truncate(&mut self, _fmaps: &mut Fmaps, _len: u64, _tx: &mut Transaction<D>) -> Result<()> {
        Err(Error::new(EBADF))
    }

    fn utimens(&mut self, times: &[TimeSpec], tx: &mut Transaction<D>) -> Result<()> {
        let mut node = tx.read_tree(self.node_ptr)?;

        if node.data().uid() == self.uid || self.uid == 0 {
            if let &[atime, mtime] = times {
                let mut node_changed = false;

                let old_mtime = node.data().mtime();
                let new_mtime = (mtime.tv_sec as u64, mtime.tv_nsec as u32);
                if old_mtime != new_mtime {
                    node.data_mut().set_mtime(new_mtime.0, new_mtime.1);
                    node_changed = true;
                }

                let old_atime = node.data().atime();
                let new_atime = (atime.tv_sec as u64, atime.tv_nsec as u32);
                if old_atime != new_atime {
                    node.data_mut().set_atime(new_atime.0, new_atime.1);
                    node_changed = true;
                }

                if node_changed {
                    tx.sync_tree(node)?;
                }
            }
            Ok(())
        } else {
            Err(Error::new(EPERM))
        }
    }

    fn getdents<'buf>(
        &mut self,
        mut buf: DirentBuf<&'buf mut [u8]>,
        opaque_offset: u64,
        tx: &mut Transaction<D>,
    ) -> Result<DirentBuf<&'buf mut [u8]>> {
        match &self.data {
            Some(data) => {
                let opaque_offset = opaque_offset as usize;
                for (idx, entry) in data.iter().enumerate().skip(opaque_offset) {
                    let child = match tx.read_tree(entry.node_ptr) {
                        Ok(r) => r,
                        Err(Error { errno: ENOENT }) => continue,
                        Err(err) => return Err(err),
                    };
                    let result = buf.entry(DirEntry {
                        inode: child.id() as u64,
                        next_opaque_id: idx as u64 + 1,
                        name: &entry.name,
                        kind: match child.data().mode() & Node::MODE_TYPE {
                            Node::MODE_DIR => DirentKind::Directory,
                            Node::MODE_FILE => DirentKind::Regular,
                            Node::MODE_SYMLINK => DirentKind::Symlink,
                            //TODO: more types?
                            _ => DirentKind::Unspecified,
                        },
                    });
                    if let Err(err) = result {
                        if err.errno == EINVAL && idx > opaque_offset {
                            // POSIX allows partial result of getdents
                            break;
                        } else {
                            return Err(err);
                        }
                    }
                }
                Ok(buf)
            }
            None => Err(Error::new(EBADF)),
        }
    }
}

pub struct FileResource {
    path: String,
    parent_ptr_opt: Option<TreePtr<Node>>,
    node_ptr: TreePtr<Node>,
    flags: usize,
    uid: u32,
}

impl FileResource {
    pub fn new(
        path: String,
        parent_ptr_opt: Option<TreePtr<Node>>,
        node_ptr: TreePtr<Node>,
        flags: usize,
        uid: u32,
    ) -> FileResource {
        FileResource {
            path,
            parent_ptr_opt,
            node_ptr,
            flags,
            uid,
        }
    }
}

impl<D: Disk> Resource<D> for FileResource {
    fn parent_ptr_opt(&self) -> Option<TreePtr<Node>> {
        self.parent_ptr_opt
    }

    fn node_ptr(&self) -> TreePtr<Node> {
        self.node_ptr
    }

    fn uid(&self) -> u32 {
        self.uid
    }

    fn set_path(&mut self, path: &str) {
        self.path = path.to_string();
    }

    fn read(
        &mut self,
        fmaps: &mut Fmaps,
        buf: &mut [u8],
        offset: u64,
        tx: &mut Transaction<D>,
    ) -> Result<usize> {
        if self.flags & O_ACCMODE != O_RDWR && self.flags & O_ACCMODE != O_RDONLY {
            return Err(Error::new(EBADF));
        }
        fmap::read(fmaps, self.node_ptr, buf, offset, tx)
    }

    fn write(
        &mut self,
        fmaps: &mut Fmaps,
        buf: &[u8],
        offset: u64,
        tx: &mut Transaction<D>,
    ) -> Result<usize> {
        if self.flags & O_ACCMODE != O_RDWR && self.flags & O_ACCMODE != O_WRONLY {
            return Err(Error::new(EBADF));
        }
        let effective_offset = if self.flags & O_APPEND == O_APPEND {
            let node = tx.read_tree(self.node_ptr)?;
            node.data().size()
        } else {
            offset
        };
        fmap::write(fmaps, self.node_ptr, buf, effective_offset, tx)
    }

    fn fsize(&mut self, tx: &mut Transaction<D>) -> Result<u64> {
        let node = tx.read_tree(self.node_ptr)?;
        Ok(node.data().size())
    }

    fn fmap(
        &mut self,
        fmaps: &mut Fmaps,
        flags: MapFlags,
        unaligned_size: usize,
        offset: u64,
        tx: &mut Transaction<D>,
    ) -> Result<usize> {
        //dbg!(&self.fmaps);
        let accmode = self.flags & O_ACCMODE;
        if flags.contains(PROT_READ) && !(accmode == O_RDWR || accmode == O_RDONLY) {
            return Err(Error::new(EBADF));
        }
        if flags.contains(PROT_WRITE) && !(accmode == O_RDWR || accmode == O_WRONLY) {
            return Err(Error::new(EBADF));
        }

        fmap::fmap(fmaps, self.node_ptr, flags, unaligned_size, offset, tx)
            .map(|address| address as usize)
    }

    fn funmap(
        &mut self,
        fmaps: &mut Fmaps,
        offset: u64,
        size: usize,
        tx: &mut Transaction<D>,
    ) -> Result<()> {
        fmap::funmap(fmaps, self.node_ptr, offset, size, tx)
    }

    fn fcntl(&mut self, cmd: usize, arg: usize) -> Result<usize> {
        match cmd {
            F_GETFL => Ok(self.flags),
            F_SETFL => {
                self.flags = (self.flags & O_ACCMODE) | (arg & !O_ACCMODE);
                Ok(0)
            }
            _ => Err(Error::new(EINVAL)),
        }
    }

    fn path(&self) -> &str {
        &self.path
    }

    fn sync(&mut self, fmaps: &mut Fmaps, tx: &mut Transaction<D>) -> Result<()> {
        fmap::sync(fmaps, self.node_ptr, tx)
    }

    fn truncate(&mut self, fmaps: &mut Fmaps, len: u64, tx: &mut Transaction<D>) -> Result<()> {
        if self.flags & O_ACCMODE == O_RDWR || self.flags & O_ACCMODE == O_WRONLY {
            fmap::truncate(fmaps, self.node_ptr, len, tx)
        } else {
            Err(Error::new(EBADF))
        }
    }

    fn utimens(&mut self, times: &[TimeSpec], tx: &mut Transaction<D>) -> Result<()> {
        let mut node = tx.read_tree(self.node_ptr)?;

        if node.data().uid() == self.uid || self.uid == 0 {
            if let &[atime, mtime] = times {
                let mut node_changed = false;

                let old_mtime = node.data().mtime();
                let new_mtime = (mtime.tv_sec as u64, mtime.tv_nsec as u32);
                if old_mtime != new_mtime {
                    node.data_mut().set_mtime(new_mtime.0, new_mtime.1);
                    node_changed = true;
                }

                let old_atime = node.data().atime();
                let new_atime = (atime.tv_sec as u64, atime.tv_nsec as u32);
                if old_atime != new_atime {
                    node.data_mut().set_atime(new_atime.0, new_atime.1);
                    node_changed = true;
                }

                if node_changed {
                    tx.sync_tree(node)?;
                }
            }
            Ok(())
        } else {
            Err(Error::new(EPERM))
        }
    }

    fn getdents<'buf>(
        &mut self,
        _buf: DirentBuf<&'buf mut [u8]>,
        _opaque_offset: u64,
        _tx: &mut Transaction<D>,
    ) -> Result<DirentBuf<&'buf mut [u8]>> {
        Err(Error::new(ENOTDIR))
    }
}

impl Drop for FileResource {
    fn drop(&mut self) {
        /*
        if !self.fmaps.is_empty() {
            eprintln!(
                "redoxfs: file {} still has {} fmaps!",
                self.path,
                self.fmaps.len()
            );
        }
        */
    }
}
