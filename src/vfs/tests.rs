use core::cell::RefCell;
use core::ops::Range;
use core::slice;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use syscall::error::{EEXIST, EINVAL, EISDIR, ENAMETOOLONG, ENOENT, ENOTDIR, ENOTEMPTY, EPERM};

use super::*;
use crate::DiskMemory;

const PAGE: u64 = PAGE_SIZE as u64;
const USER: Creds = Creds {
    uid: 1000,
    gid: 1000,
};

fn new_vfs() -> Vfs<DiskMemory> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let fs = FileSystem::create(
        DiskMemory::new(64 << 20),
        None,
        now.as_secs(),
        now.subsec_nanos(),
    )
    .unwrap();
    Vfs::new(fs)
}

/// Unmount and mount again, as after a restart.
fn remount(vfs: Vfs<DiskMemory>) -> Vfs<DiskMemory> {
    let fs = vfs.unmount().unwrap();
    Vfs::new(FileSystem::open(fs.disk, None, None, true).unwrap())
}

fn errno<T: core::fmt::Debug>(result: Result<T>) -> i32 {
    result.unwrap_err().errno
}

fn create_file(vfs: &Vfs<DiskMemory>, name: &str, data: &[u8]) -> u64 {
    let ino = vfs
        .create(USER, ROOT_INO, name.as_bytes(), 0o644)
        .unwrap()
        .ino;
    vfs.write(ino, 0, data).unwrap();
    ino
}

fn read_all(vfs: &Vfs<DiskMemory>, ino: u64) -> Vec<u8> {
    let size = vfs.getattr(ino).unwrap().size;
    let mut buf = vec![0; size as usize];
    let count = vfs.read(ino, 0, &mut buf).unwrap();
    buf.truncate(count);
    buf
}

fn names(vfs: &Vfs<DiskMemory>, dir: u64) -> Vec<String> {
    let handle = vfs.opendir(dir).unwrap();
    let entries = vfs.readdir(handle, 0).unwrap();
    vfs.releasedir(handle);
    let mut names: Vec<String> = entries
        .iter()
        .map(|entry| String::from_utf8(entry.name.clone()).unwrap())
        .collect();
    names.sort();
    names
}

/// A DAX window that, like a hypervisor, refuses to map over mappings.
struct TestWindow {
    mapped: RefCell<Vec<Range<u64>>>,
    fail_map: core::cell::Cell<bool>,
    #[cfg(target_os = "linux")]
    base: *mut u8,
    #[cfg(not(target_os = "linux"))]
    addrs: RefCell<alloc::collections::BTreeMap<u64, *mut u8>>,
}

#[cfg(target_os = "linux")]
const WINDOW_SIZE: u64 = 64 << 20;

impl TestWindow {
    fn new() -> Self {
        Self {
            mapped: RefCell::new(Vec::new()),
            fail_map: core::cell::Cell::new(false),
            #[cfg(target_os = "linux")]
            base: unsafe {
                let base = libc::mmap(
                    core::ptr::null_mut(),
                    WINDOW_SIZE as usize,
                    libc::PROT_NONE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                );
                assert_ne!(base, libc::MAP_FAILED);
                base.cast()
            },
            #[cfg(not(target_os = "linux"))]
            addrs: RefCell::new(Default::default()),
        }
    }

    fn is_mapped(&self, offset: u64) -> bool {
        self.mapped.borrow().iter().any(|r| r.contains(&offset))
    }

    /// `len` bytes of guest-visible memory at `offset` into the window.
    // Guest memory aliases the page cache by design.
    #[allow(clippy::mut_from_ref)]
    fn guest(&self, offset: u64, len: usize) -> &mut [u8] {
        assert!(
            self.is_mapped(offset),
            "window offset {offset:#x} not mapped"
        );
        #[cfg(target_os = "linux")]
        let ptr = unsafe { self.base.add(offset as usize) };
        #[cfg(not(target_os = "linux"))]
        let ptr = {
            let addrs = self.addrs.borrow();
            let (start, addr) = addrs.range(..=offset).next_back().unwrap();
            unsafe { addr.add((offset - start) as usize) }
        };
        unsafe { slice::from_raw_parts_mut(ptr, len) }
    }
}

impl DaxWindow for TestWindow {
    fn map(&self, window_offset: u64, source: &MapSource<'_>, writable: bool) -> io::Result<()> {
        if self.fail_map.get() {
            return Err(io::Error::from_raw_os_error(libc::ENOMEM));
        }
        let range = window_offset..window_offset + source.len();
        if self
            .mapped
            .borrow()
            .iter()
            .any(|r| r.start < range.end && range.start < r.end)
        {
            return Err(io::Error::from_raw_os_error(libc::EEXIST));
        }
        #[cfg(target_os = "linux")]
        unsafe {
            use std::os::fd::AsRawFd;
            let prot = if writable {
                libc::PROT_READ | libc::PROT_WRITE
            } else {
                libc::PROT_READ
            };
            let addr = libc::mmap(
                self.base.add(window_offset as usize).cast(),
                source.len() as usize,
                prot,
                libc::MAP_SHARED | libc::MAP_FIXED,
                source.fd().as_raw_fd(),
                source.fd_offset() as libc::off_t,
            );
            assert_ne!(addr, libc::MAP_FAILED);
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = writable;
            self.addrs.borrow_mut().insert(window_offset, source.addr());
        }
        self.mapped.borrow_mut().push(range);
        Ok(())
    }

    fn unmap(&self, window_offset: u64, len: u64) -> io::Result<()> {
        let range = window_offset..window_offset + len;
        let mut mapped = self.mapped.borrow_mut();
        let mut remaining = Vec::new();
        for r in mapped.drain(..) {
            if r.start < range.end && range.start < r.end {
                // Unmapping part of a mapping keeps the rest
                if r.start < range.start {
                    remaining.push(r.start..range.start);
                }
                if range.end < r.end {
                    remaining.push(range.end..r.end);
                    #[cfg(not(target_os = "linux"))]
                    {
                        let mut addrs = self.addrs.borrow_mut();
                        let addr = addrs[&r.start];
                        addrs.insert(range.end, unsafe {
                            addr.add((range.end - r.start) as usize)
                        });
                    }
                }
            } else {
                remaining.push(r);
            }
        }
        *mapped = remaining;
        #[cfg(target_os = "linux")]
        unsafe {
            let addr = libc::mmap(
                self.base.add(window_offset as usize).cast(),
                len as usize,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
                -1,
                0,
            );
            assert_ne!(addr, libc::MAP_FAILED);
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl Drop for TestWindow {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base.cast(), WINDOW_SIZE as usize) };
    }
}

fn mapped_nodes(vfs: &Vfs<DiskMemory>) -> usize {
    vfs.state().fmaps.len()
}

// Namespace

#[test]
fn create_lookup_and_read_back() {
    let vfs = new_vfs();
    let attr = vfs.create(USER, ROOT_INO, b"file", 0o640).unwrap();
    assert_eq!(attr.kind, FileKind::File);
    assert_eq!(attr.mode(), 0o100640);
    assert_eq!((attr.uid, attr.gid), (1000, 1000));

    assert_eq!(vfs.write(attr.ino, 0, b"hello").unwrap(), 5);

    let found = vfs.lookup(ROOT_INO, b"file").unwrap();
    assert_eq!(found.ino, attr.ino);
    assert_eq!(found.size, 5);
    assert_eq!(read_all(&vfs, attr.ino), b"hello");
}

#[test]
fn lookup_missing_is_enoent() {
    let vfs = new_vfs();
    assert_eq!(errno(vfs.lookup(ROOT_INO, b"missing")), ENOENT);
}

#[test]
fn create_existing_is_eexist() {
    let vfs = new_vfs();
    create_file(&vfs, "file", b"");
    assert_eq!(errno(vfs.create(USER, ROOT_INO, b"file", 0o644)), EEXIST);
}

#[test]
fn long_name_is_enametoolong() {
    let vfs = new_vfs();
    let name = [b'a'; DIR_ENTRY_MAX_LENGTH + 1];
    assert_eq!(
        errno(vfs.create(USER, ROOT_INO, &name, 0o644)),
        ENAMETOOLONG
    );
    assert_eq!(errno(vfs.lookup(ROOT_INO, &name)), ENAMETOOLONG);
}

#[test]
fn mkdir_and_rmdir() {
    let vfs = new_vfs();
    let dir = vfs.mkdir(USER, ROOT_INO, b"dir", 0o755).unwrap();
    assert_eq!(dir.kind, FileKind::Directory);
    vfs.create(USER, dir.ino, b"child", 0o644).unwrap();

    assert_eq!(errno(vfs.rmdir(ROOT_INO, b"dir")), ENOTEMPTY);
    vfs.unlink(dir.ino, b"child").unwrap();
    vfs.rmdir(ROOT_INO, b"dir").unwrap();
    assert_eq!(errno(vfs.lookup(ROOT_INO, b"dir")), ENOENT);
}

#[test]
fn unlink_directory_is_eisdir_and_rmdir_file_is_enotdir() {
    let vfs = new_vfs();
    vfs.mkdir(USER, ROOT_INO, b"dir", 0o755).unwrap();
    create_file(&vfs, "file", b"");

    assert_eq!(errno(vfs.unlink(ROOT_INO, b"dir")), EISDIR);
    assert_eq!(errno(vfs.rmdir(ROOT_INO, b"file")), ENOTDIR);
}

#[test]
fn create_in_file_is_enotdir() {
    let vfs = new_vfs();
    let file = create_file(&vfs, "file", b"");
    assert_eq!(errno(vfs.create(USER, file, b"child", 0o644)), ENOTDIR);
}

#[test]
fn setgid_directory_passes_on_group() {
    let vfs = new_vfs();
    let dir = vfs.mkdir(USER, ROOT_INO, b"shared", 0o2775).unwrap();
    vfs.setattr(
        dir.ino,
        SetAttr {
            gid: Some(50),
            ..Default::default()
        },
    )
    .unwrap();

    let file = vfs.create(USER, dir.ino, b"file", 0o644).unwrap();
    let subdir = vfs.mkdir(USER, dir.ino, b"sub", 0o755).unwrap();

    assert_eq!((file.uid, file.gid), (1000, 50));
    assert_eq!(file.perm, 0o644);
    assert_eq!(subdir.gid, 50);
    assert_eq!(subdir.perm, 0o2755);
}

#[test]
fn symlink_and_readlink() {
    let vfs = new_vfs();
    let link = vfs.symlink(USER, ROOT_INO, b"link", b"../target").unwrap();
    assert_eq!(link.kind, FileKind::Symlink);
    assert_eq!(link.size, 9);
    assert_eq!(vfs.readlink(link.ino).unwrap(), b"../target");

    let file = create_file(&vfs, "file", b"x");
    assert_eq!(errno(vfs.readlink(file)), EINVAL);
}

#[test]
fn mknod_supports_files_and_sockets_only() {
    let vfs = new_vfs();
    let sock = vfs.mknod(USER, ROOT_INO, b"sock", 0o140755).unwrap();
    assert_eq!(sock.kind, FileKind::Socket);
    let file = vfs.mknod(USER, ROOT_INO, b"file", 0o100600).unwrap();
    assert_eq!(file.mode(), 0o100600);

    // FIFO
    assert_eq!(errno(vfs.mknod(USER, ROOT_INO, b"fifo", 0o010644)), EPERM);
}

#[test]
fn hard_link_shares_data() {
    let vfs = new_vfs();
    let file = create_file(&vfs, "file", b"shared");

    let linked = vfs.link(file, ROOT_INO, b"alias").unwrap();
    assert_eq!(linked.ino, file);
    assert_eq!(linked.nlink, 2);

    vfs.unlink(ROOT_INO, b"file").unwrap();
    let alias = vfs.lookup(ROOT_INO, b"alias").unwrap();
    assert_eq!(alias.nlink, 1);
    assert_eq!(read_all(&vfs, alias.ino), b"shared");
}

#[test]
fn link_errors() {
    let vfs = new_vfs();
    let dir = vfs.mkdir(USER, ROOT_INO, b"dir", 0o755).unwrap();
    let file = create_file(&vfs, "file", b"");

    assert_eq!(errno(vfs.link(dir.ino, ROOT_INO, b"dirlink")), EPERM);
    assert_eq!(errno(vfs.link(file, ROOT_INO, b"dir")), EEXIST);
}

#[test]
fn rename_replaces_compatible_targets_only() {
    let vfs = new_vfs();
    create_file(&vfs, "a", b"a");
    create_file(&vfs, "b", b"b");
    vfs.mkdir(USER, ROOT_INO, b"dir", 0o755).unwrap();
    vfs.mkdir(USER, ROOT_INO, b"empty", 0o755).unwrap();

    assert_eq!(
        errno(vfs.rename(ROOT_INO, b"a", ROOT_INO, b"dir", 0)),
        EISDIR
    );
    assert_eq!(
        errno(vfs.rename(ROOT_INO, b"dir", ROOT_INO, b"a", 0)),
        ENOTDIR
    );
    assert_eq!(
        errno(vfs.rename(ROOT_INO, b"a", ROOT_INO, b"b", RENAME_NOREPLACE)),
        EEXIST
    );
    assert_eq!(errno(vfs.rename(ROOT_INO, b"a", ROOT_INO, b"b", 2)), EINVAL);

    vfs.rename(ROOT_INO, b"a", ROOT_INO, b"b", 0).unwrap();
    vfs.rename(ROOT_INO, b"dir", ROOT_INO, b"empty", 0).unwrap();

    assert_eq!(names(&vfs, ROOT_INO), [".", "..", "b", "empty"]);
    let b = vfs.lookup(ROOT_INO, b"b").unwrap();
    assert_eq!(read_all(&vfs, b.ino), b"a");
}

#[test]
fn rename_across_directories() {
    let vfs = new_vfs();
    let dir = vfs.mkdir(USER, ROOT_INO, b"dir", 0o755).unwrap();
    let file = create_file(&vfs, "file", b"moved");

    vfs.rename(ROOT_INO, b"file", dir.ino, b"inside", RENAME_NOREPLACE)
        .unwrap();

    assert_eq!(names(&vfs, dir.ino), [".", "..", "inside"]);
    assert_eq!(vfs.lookup(dir.ino, b"inside").unwrap().ino, file);
}

#[test]
fn readdir_lists_kinds_and_refreshes_on_rewind() {
    let vfs = new_vfs();
    let dir = vfs.mkdir(USER, ROOT_INO, b"dir", 0o755).unwrap();
    create_file(&vfs, "file", b"");
    vfs.symlink(USER, ROOT_INO, b"link", b"file").unwrap();

    let handle = vfs.opendir(ROOT_INO).unwrap();
    let entries = vfs.readdir(handle, 0).unwrap();
    let mut kinds: Vec<(Vec<u8>, FileKind)> = entries
        .iter()
        .map(|entry| (entry.name.clone(), entry.kind))
        .collect();
    kinds.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        kinds,
        [
            (b".".to_vec(), FileKind::Directory),
            (b"..".to_vec(), FileKind::Directory),
            (b"dir".to_vec(), FileKind::Directory),
            (b"file".to_vec(), FileKind::File),
            (b"link".to_vec(), FileKind::Symlink),
        ]
    );
    assert_eq!(
        entries
            .iter()
            .find(|entry| entry.name == b"dir")
            .unwrap()
            .ino,
        dir.ino
    );

    create_file(&vfs, "new", b"");
    // Continuing keeps the snapshot; rewinding refreshes it
    assert_eq!(vfs.readdir(handle, 3).unwrap().len(), 5);
    assert_eq!(vfs.readdir(handle, 0).unwrap().len(), 6);
    vfs.releasedir(handle);
    assert_eq!(errno(vfs.readdir(handle, 0)), EINVAL);
}

#[test]
fn opendir_on_file_is_enotdir() {
    let vfs = new_vfs();
    let file = create_file(&vfs, "file", b"");
    assert_eq!(errno(vfs.opendir(file)), ENOTDIR);
}

#[test]
fn setattr_changes_metadata() {
    let vfs = new_vfs();
    let file = create_file(&vfs, "file", b"hello world");
    let time = Timespec {
        secs: 1_000_000,
        nanos: 42,
    };

    let attr = vfs
        .setattr(
            file,
            SetAttr {
                perm: Some(0o4600),
                uid: Some(7),
                gid: Some(8),
                size: Some(5),
                atime: Some(SetTime::At(time)),
                mtime: Some(SetTime::At(time)),
                kill_suid_sgid: false,
            },
        )
        .unwrap();

    assert_eq!(attr.mode(), 0o104600);
    assert_eq!((attr.uid, attr.gid), (7, 8));
    assert_eq!(attr.size, 5);
    assert_eq!((attr.atime, attr.mtime), (time, time));
    assert_eq!(vfs.getattr(file).unwrap(), attr);
    assert_eq!(read_all(&vfs, file), b"hello");
}

#[test]
fn kill_suid_sgid_follows_linux_rules() {
    let vfs = new_vfs();
    let kill = SetAttr {
        kill_suid_sgid: true,
        ..Default::default()
    };
    let perm = |perm| SetAttr {
        perm: Some(perm),
        ..Default::default()
    };

    let exec = create_file(&vfs, "exec", b"");
    vfs.setattr(exec, perm(0o6755)).unwrap();
    assert_eq!(vfs.setattr(exec, kill).unwrap().perm, 0o755);

    // Without group execute, setgid marks mandatory locking and stays
    let locked = create_file(&vfs, "locked", b"");
    vfs.setattr(locked, perm(0o6644)).unwrap();
    assert_eq!(vfs.setattr(locked, kill).unwrap().perm, 0o2644);
}

#[test]
fn truncate_directory_is_eisdir() {
    let vfs = new_vfs();
    let dir = vfs.mkdir(USER, ROOT_INO, b"dir", 0o755).unwrap();
    let set = SetAttr {
        size: Some(0),
        ..Default::default()
    };
    assert_eq!(errno(vfs.setattr(dir.ino, set)), EISDIR);
}

#[test]
fn statfs_reports_space() {
    let vfs = new_vfs();
    let before = vfs.statfs();
    assert_eq!(before.block_size, 4096);
    assert!(before.blocks_free < before.blocks);

    create_file(&vfs, "file", &[1; 64 << 10]);
    assert!(vfs.statfs().blocks_free < before.blocks_free);
}

#[test]
fn data_survives_remount() {
    let vfs = new_vfs();
    let dir = vfs.mkdir(USER, ROOT_INO, b"dir", 0o755).unwrap();
    let file = vfs.create(USER, dir.ino, b"file", 0o644).unwrap().ino;
    vfs.write(file, 0, b"persistent").unwrap();

    let vfs = remount(vfs);
    let dir = vfs.lookup(ROOT_INO, b"dir").unwrap();
    let file = vfs.lookup(dir.ino, b"file").unwrap();
    assert_eq!(read_all(&vfs, file.ino), b"persistent");
}

// Lookup counts

#[test]
fn looked_up_file_survives_unlink_until_forgotten() {
    let vfs = new_vfs();
    let file = create_file(&vfs, "file", b"still here");
    vfs.lookup(ROOT_INO, b"file").unwrap();

    vfs.unlink(ROOT_INO, b"file").unwrap();
    assert_eq!(read_all(&vfs, file), b"still here");
    vfs.write(file, 0, b"STILL").unwrap();

    // create and lookup each took a reference
    vfs.forget(file, 1);
    assert_eq!(read_all(&vfs, file), b"STILL here");
    let free_before = vfs.statfs().blocks_free;
    vfs.forget(file, 1);

    assert!(vfs.statfs().blocks_free > free_before);
    assert!(!vfs.state().fs.node_usages.contains_key(&(file as u32)));
}

#[test]
fn forgetting_unknown_inodes_is_harmless() {
    let vfs = new_vfs();
    vfs.forget(ROOT_INO, 1);
    vfs.forget(12345, 1);
    vfs.forget(0, 1);
    assert_eq!(vfs.getattr(ROOT_INO).unwrap().kind, FileKind::Directory);
}

// DAX

#[test]
fn guest_writes_are_visible_to_reads_and_persist_after_fsync() {
    let vfs = new_vfs();
    let window = TestWindow::new();
    let file = create_file(&vfs, "file", &[b'a'; 8192]);

    vfs.setup_mapping(file, 0, 8192, true, 2 * PAGE, &window)
        .unwrap();
    let guest = window.guest(2 * PAGE, 8192);
    assert_eq!(guest[0], b'a');
    guest[4096..4101].copy_from_slice(b"guest");

    // Unsynced guest writes are visible to regular reads
    let mut buf = [0; 5];
    vfs.read(file, 4096, &mut buf).unwrap();
    assert_eq!(&buf, b"guest");

    vfs.fsync(file).unwrap();
    // Mapping survives fsync
    assert!(window.is_mapped(2 * PAGE));

    // On disk, bypassing the page cache
    let on_disk = vfs
        .state()
        .fs
        .tx(|tx| {
            let mut buf = [0; 5];
            tx.read_node(TreePtr::new(file as u32), 4096, &mut buf, 0, 0)?;
            Ok(buf)
        })
        .unwrap();
    assert_eq!(&on_disk, b"guest");
}

#[test]
fn writes_are_visible_through_mapping() {
    let vfs = new_vfs();
    let window = TestWindow::new();
    let file = create_file(&vfs, "file", &[b'a'; 4096]);

    vfs.setup_mapping(file, 0, 4096, false, 0, &window).unwrap();
    vfs.write(file, 10, b"written").unwrap();

    assert_eq!(&window.guest(10, 7)[..], b"written");
}

#[test]
fn extending_write_is_visible_past_old_eof() {
    let vfs = new_vfs();
    let window = TestWindow::new();
    let file = create_file(&vfs, "file", b"short");

    // Guests map whole chunks, past the end of the file
    vfs.setup_mapping(file, 0, 8192, true, 0, &window).unwrap();
    vfs.write(file, 6000, b"appended").unwrap();

    let guest = window.guest(0, 8192);
    assert_eq!(&guest[..5], b"short");
    assert!(guest[5..6000].iter().all(|&b| b == 0));
    assert_eq!(&guest[6000..6008], b"appended");
}

#[test]
fn truncate_is_visible_through_mapping() {
    let vfs = new_vfs();
    let window = TestWindow::new();
    let file = create_file(&vfs, "file", b"hello world");

    vfs.setup_mapping(file, 0, 4096, true, 0, &window).unwrap();
    let set = SetAttr {
        size: Some(5),
        ..Default::default()
    };
    vfs.setattr(file, set).unwrap();

    assert_eq!(&window.guest(0, 11)[..], b"hello\0\0\0\0\0\0");
}

#[test]
fn remove_mapping_writes_back_and_releases() {
    let vfs = new_vfs();
    let window = TestWindow::new();
    let file = create_file(&vfs, "file", b"hello");

    vfs.setup_mapping(file, 0, 4096, true, 0, &window).unwrap();
    window.guest(0, 5).copy_from_slice(b"HELLO");
    // Past the end of the file: must not grow it
    window.guest(100, 1)[0] = b'!';

    vfs.remove_mapping(0, 4096, &window).unwrap();

    assert!(!window.is_mapped(0));
    assert_eq!(mapped_nodes(&vfs), 0);
    assert_eq!(read_all(&vfs, file), b"HELLO");
}

#[test]
fn read_only_mapping_is_not_written_back() {
    let vfs = new_vfs();
    let window = TestWindow::new();
    let file = create_file(&vfs, "file", b"hello");

    vfs.setup_mapping(file, 0, 4096, false, 0, &window).unwrap();
    vfs.remove_mapping(0, 4096, &window).unwrap();

    assert_eq!(read_all(&vfs, file), b"hello");
}

#[test]
fn remapping_in_place_keeps_unsynced_writes() {
    let vfs = new_vfs();
    let window = TestWindow::new();
    let file = create_file(&vfs, "file", b"hello");

    vfs.setup_mapping(file, 0, 4096, true, 0, &window).unwrap();
    window.guest(0, 5).copy_from_slice(b"HELLO");

    // The window refuses to map over a mapping, so this must unmap first
    vfs.setup_mapping(file, 0, 4096, true, 0, &window).unwrap();
    assert_eq!(&window.guest(0, 5)[..], b"HELLO");
    assert_eq!(window.mapped.borrow().len(), 1);

    vfs.remove_mapping(0, 4096, &window).unwrap();
    assert_eq!(mapped_nodes(&vfs), 0);
    assert_eq!(read_all(&vfs, file), b"HELLO");
}

#[test]
fn replacing_a_mapping_releases_the_old_file() {
    let vfs = new_vfs();
    let window = TestWindow::new();
    let a = create_file(&vfs, "a", b"aaaa");
    let b = create_file(&vfs, "b", b"bbbb");

    vfs.setup_mapping(a, 0, 4096, true, 0, &window).unwrap();
    window.guest(0, 1)[0] = b'A';
    vfs.setup_mapping(b, 0, 4096, true, 0, &window).unwrap();

    assert_eq!(read_all(&vfs, a), b"Aaaa");
    assert_eq!(&window.guest(0, 4)[..], b"bbbb");
    assert_eq!(mapped_nodes(&vfs), 1);
}

#[test]
fn partial_remove_mapping_keeps_the_rest() {
    let vfs = new_vfs();
    let window = TestWindow::new();
    let file = create_file(&vfs, "file", &[b'a'; 3 * 4096]);

    vfs.setup_mapping(file, 4096, 2 * 4096, true, 0, &window)
        .unwrap();
    vfs.remove_mapping(0, 4096, &window).unwrap();

    assert!(!window.is_mapped(0));
    // Window offset 4096 still maps file offset 8192
    window.guest(4096, 1)[0] = b'Z';
    vfs.remove_mapping(4096, 4096, &window).unwrap();

    assert_eq!(mapped_nodes(&vfs), 0);
    assert_eq!(read_all(&vfs, file)[8192], b'Z');
}

#[test]
fn remove_mapping_spanning_several_grants() {
    let vfs = new_vfs();
    let window = TestWindow::new();
    let a = create_file(&vfs, "a", b"a");
    let b = create_file(&vfs, "b", b"b");

    vfs.setup_mapping(a, 0, 4096, true, 0, &window).unwrap();
    vfs.setup_mapping(b, 0, 4096, true, 4096, &window).unwrap();
    window.guest(0, 1)[0] = b'A';
    window.guest(4096, 1)[0] = b'B';

    vfs.remove_mapping(0, 1 << 20, &window).unwrap();

    assert!(window.mapped.borrow().is_empty());
    assert_eq!(mapped_nodes(&vfs), 0);
    assert_eq!(read_all(&vfs, a), b"A");
    assert_eq!(read_all(&vfs, b), b"B");
}

#[test]
fn mapped_file_survives_unlink_and_forget() {
    let vfs = new_vfs();
    let window = TestWindow::new();
    let file = create_file(&vfs, "file", b"mapped");

    vfs.setup_mapping(file, 0, 4096, true, 0, &window).unwrap();
    vfs.unlink(ROOT_INO, b"file").unwrap();
    vfs.forget(file, 1);

    assert_eq!(&window.guest(0, 6)[..], b"mapped");
    assert!(vfs.state().fs.node_usages.contains_key(&(file as u32)));

    vfs.remove_mapping(0, 4096, &window).unwrap();
    assert!(!vfs.state().fs.node_usages.contains_key(&(file as u32)));
}

#[test]
fn setup_mapping_rejects_bad_requests() {
    let vfs = new_vfs();
    let window = TestWindow::new();
    let file = create_file(&vfs, "file", b"x");
    let dir = vfs.mkdir(USER, ROOT_INO, b"dir", 0o755).unwrap();

    assert_eq!(
        errno(vfs.setup_mapping(file, 1, 4096, true, 0, &window)),
        EINVAL
    );
    assert_eq!(
        errno(vfs.setup_mapping(file, 0, 100, true, 0, &window)),
        EINVAL
    );
    assert_eq!(
        errno(vfs.setup_mapping(file, 0, 4096, true, 1, &window)),
        EINVAL
    );
    assert_eq!(
        errno(vfs.setup_mapping(file, 0, 0, true, 0, &window)),
        EINVAL
    );
    assert_eq!(
        errno(vfs.setup_mapping(file, 0, 4096, true, u64::MAX - 4095, &window)),
        EINVAL
    );
    assert_eq!(
        errno(vfs.setup_mapping(dir.ino, 0, 4096, true, 0, &window)),
        EINVAL
    );

    assert!(window.mapped.borrow().is_empty());
    assert_eq!(mapped_nodes(&vfs), 0);
}

#[test]
fn failed_window_map_leaves_no_trace() {
    let vfs = new_vfs();
    let window = TestWindow::new();
    let a = create_file(&vfs, "a", b"a");
    let b = create_file(&vfs, "b", b"b");

    vfs.setup_mapping(a, 0, 4096, true, 0, &window).unwrap();
    window.guest(0, 1)[0] = b'A';

    window.fail_map.set(true);
    let err = errno(vfs.setup_mapping(b, 0, 4096, true, 0, &window));
    assert_eq!(err, syscall::error::ENOMEM);

    // The old mapping was cleared (and written back) before the attempt
    assert!(window.mapped.borrow().is_empty());
    assert_eq!(mapped_nodes(&vfs), 0);
    assert_eq!(read_all(&vfs, a), b"A");
}

#[test]
fn unmount_writes_back_outstanding_mappings() {
    let vfs = new_vfs();
    let window = TestWindow::new();
    let file = create_file(&vfs, "file", b"hello");

    vfs.setup_mapping(file, 0, 4096, true, 0, &window).unwrap();
    window.guest(0, 5).copy_from_slice(b"bye!!");

    let vfs = remount(vfs);
    let file = vfs.lookup(ROOT_INO, b"file").unwrap().ino;
    assert_eq!(read_all(&vfs, file), b"bye!!");
}

#[test]
fn mapping_large_sparse_file_in_chunks() {
    let vfs = new_vfs();
    let window = TestWindow::new();
    let file = create_file(&vfs, "file", b"start");
    let chunk = 2 << 20;
    vfs.write(file, 10 * chunk, b"end").unwrap();

    // Like a Linux guest: 2 MiB chunks, mapped while earlier ones stay mapped
    vfs.setup_mapping(file, 0, chunk, true, 0, &window).unwrap();
    vfs.setup_mapping(file, 10 * chunk, chunk, true, chunk, &window)
        .unwrap();
    window.guest(0, 5).copy_from_slice(b"START");

    assert_eq!(&window.guest(chunk, 3)[..], b"end");
    assert_eq!(&window.guest(0, 5)[..], b"START");

    vfs.remove_mapping(0, 2 * chunk, &window).unwrap();
    assert_eq!(&read_all(&vfs, file)[..5], b"START");
    assert_eq!(vfs.getattr(file).unwrap().size, 10 * chunk + 3);
}
