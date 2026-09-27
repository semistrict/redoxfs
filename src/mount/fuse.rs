extern crate fuser;

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use self::fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, Session, TimeOrNow,
};

use crate::vfs::{Attr, Creds, FileKind, SetAttr, SetTime, Timespec, Vfs};
use crate::{filesystem, Disk};

const TTL: Duration = Duration::new(1, 0); // 1 second

pub fn mount<D, P, T, F>(
    filesystem: filesystem::FileSystem<D>,
    mountpoint: P,
    callback: F,
) -> io::Result<T>
where
    D: Disk,
    P: AsRef<Path>,
    F: FnOnce(&Path) -> T,
{
    let mountpoint = mountpoint.as_ref();

    let mut options = Vec::new();

    // This allows redoxfs being run as root (e.g. mounting /dev partition)
    // then accessing it from non root account, with built-in file access check.
    if std::env::var("REDOXFS_FUSE_SHARE").is_ok_and(|s| s == "1") {
        options.push(MountOption::AllowOther);
        options.push(MountOption::AutoUnmount);
        options.push(MountOption::DefaultPermissions);
    }

    let vfs = Vfs::new(filesystem);
    let res = {
        let mut session = Session::new(Fuse { vfs: &vfs }, mountpoint, &options)?;

        let res = callback(mountpoint);

        session.run()?;

        res
    };

    // Write back and clean up on unmount
    vfs.unmount()?;

    Ok(res)
}

pub struct Fuse<'f, D: Disk> {
    pub vfs: &'f Vfs<D>,
}

fn system_time(time: Timespec) -> SystemTime {
    UNIX_EPOCH + Duration::new(time.secs, time.nanos)
}

fn file_type(kind: FileKind) -> FileType {
    match kind {
        FileKind::File => FileType::RegularFile,
        FileKind::Directory => FileType::Directory,
        FileKind::Symlink => FileType::Symlink,
        FileKind::Socket => FileType::Socket,
    }
}

fn file_attr(attr: &Attr) -> FileAttr {
    FileAttr {
        ino: attr.ino,
        size: attr.size,
        blocks: attr.blocks,
        atime: system_time(attr.atime),
        mtime: system_time(attr.mtime),
        ctime: system_time(attr.ctime),
        crtime: UNIX_EPOCH,
        kind: file_type(attr.kind),
        perm: attr.perm,
        nlink: attr.nlink,
        uid: attr.uid,
        gid: attr.gid,
        rdev: 0,
        blksize: attr.blksize,
        flags: 0,
    }
}

fn set_time(time: TimeOrNow) -> SetTime {
    match time {
        TimeOrNow::Now => SetTime::Now,
        TimeOrNow::SpecificTime(time) => {
            let time = time.duration_since(UNIX_EPOCH).unwrap_or_default();
            SetTime::At(Timespec {
                secs: time.as_secs(),
                nanos: time.subsec_nanos(),
            })
        }
    }
}

fn creds(req: &Request) -> Creds {
    Creds {
        uid: req.uid(),
        gid: req.gid(),
    }
}

fn offset(offset: i64) -> u64 {
    offset.max(0) as u64
}

fn reply_entry(reply: ReplyEntry, result: syscall::error::Result<Attr>) {
    match result {
        Ok(attr) => reply.entry(&TTL, &file_attr(&attr), 0),
        Err(err) => reply.error(err.errno),
    }
}

fn reply_empty(reply: ReplyEmpty, result: syscall::error::Result<()>) {
    match result {
        Ok(()) => reply.ok(),
        Err(err) => reply.error(err.errno),
    }
}

impl<D: Disk> Filesystem for Fuse<'_, D> {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        reply_entry(reply, self.vfs.lookup(parent, name.as_bytes()));
    }

    fn forget(&mut self, _req: &Request, ino: u64, nlookup: u64) {
        self.vfs.forget(ino, nlookup);
    }

    fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        match self.vfs.getattr(ino) {
            Ok(attr) => reply.attr(&TTL, &file_attr(&attr)),
            Err(err) => reply.error(err.errno),
        }
    }

    fn setattr(
        &mut self,
        _req: &Request,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let set = SetAttr {
            perm: mode.map(|mode| mode as u16),
            uid,
            gid,
            size,
            atime: atime.map(set_time),
            mtime: mtime.map(set_time),
        };
        match self.vfs.setattr(ino, set) {
            Ok(attr) => reply.attr(&TTL, &file_attr(&attr)),
            Err(err) => reply.error(err.errno),
        }
    }

    fn readlink(&mut self, _req: &Request, ino: u64, reply: ReplyData) {
        match self.vfs.readlink(ino) {
            Ok(target) => reply.data(&target),
            Err(err) => reply.error(err.errno),
        }
    }

    fn mknod(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        reply_entry(
            reply,
            self.vfs.mknod(creds(req), parent, name.as_bytes(), mode),
        );
    }

    fn mkdir(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        reply_entry(
            reply,
            self.vfs
                .mkdir(creds(req), parent, name.as_bytes(), mode as u16),
        );
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        reply_empty(reply, self.vfs.unlink(parent, name.as_bytes()));
    }

    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        reply_empty(reply, self.vfs.rmdir(parent, name.as_bytes()));
    }

    fn symlink(
        &mut self,
        req: &Request,
        parent: u64,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        reply_entry(
            reply,
            self.vfs.symlink(
                creds(req),
                parent,
                link_name.as_bytes(),
                target.as_os_str().as_bytes(),
            ),
        );
    }

    fn rename(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        reply_empty(
            reply,
            self.vfs.rename(
                parent,
                name.as_bytes(),
                newparent,
                newname.as_bytes(),
                flags,
            ),
        );
    }

    fn link(
        &mut self,
        _req: &Request,
        ino: u64,
        newparent: u64,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        reply_entry(reply, self.vfs.link(ino, newparent, newname.as_bytes()));
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let mut data = vec![0; size as usize];
        match self.vfs.read(ino, self::offset(offset), &mut data) {
            Ok(count) => reply.data(&data[..count]),
            Err(err) => reply.error(err.errno),
        }
    }

    fn write(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        match self.vfs.write(ino, self::offset(offset), data) {
            Ok(count) => reply.written(count as u32),
            Err(err) => reply.error(err.errno),
        }
    }

    fn flush(&mut self, _req: &Request, _ino: u64, _fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        reply.ok();
    }

    fn fsync(&mut self, _req: &Request, ino: u64, _fh: u64, _datasync: bool, reply: ReplyEmpty) {
        reply_empty(reply, self.vfs.fsync(ino));
    }

    fn opendir(&mut self, _req: &Request, ino: u64, _flags: i32, reply: ReplyOpen) {
        match self.vfs.opendir(ino) {
            Ok(handle) => reply.opened(handle, 0),
            Err(err) => reply.error(err.errno),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let offset = self::offset(offset);
        let entries = match self.vfs.readdir(fh, offset) {
            Ok(entries) => entries,
            Err(err) => return reply.error(err.errno),
        };
        for (i, entry) in entries.iter().enumerate().skip(offset as usize) {
            let full = reply.add(
                entry.ino,
                i as i64 + 1,
                file_type(entry.kind),
                OsStr::from_bytes(&entry.name),
            );
            if full {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(&mut self, _req: &Request, _ino: u64, fh: u64, _flags: i32, reply: ReplyEmpty) {
        self.vfs.releasedir(fh);
        reply.ok();
    }

    fn fsyncdir(&mut self, _req: &Request, ino: u64, _fh: u64, _datasync: bool, reply: ReplyEmpty) {
        reply_empty(reply, self.vfs.fsync(ino));
    }

    fn statfs(&mut self, _req: &Request, _ino: u64, reply: ReplyStatfs) {
        let statfs = self.vfs.statfs();
        reply.statfs(
            statfs.blocks,
            statfs.blocks_free,
            statfs.blocks_free,
            0,
            0,
            statfs.block_size,
            statfs.name_max,
            statfs.block_size,
        );
    }

    fn create(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        match self
            .vfs
            .create(creds(req), parent, name.as_bytes(), mode as u16)
        {
            Ok(attr) => reply.created(&TTL, &file_attr(&attr), 0, 0, 0),
            Err(err) => reply.error(err.errno),
        }
    }
}
