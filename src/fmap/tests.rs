use core::slice;
use std::time::{SystemTime, UNIX_EPOCH};

use syscall::flag::{MapFlags, PROT_READ, PROT_WRITE};

use super::{fmap, funmap, read, sync, truncate, write, Fmaps};
use crate::{DiskMemory, FileSystem, Node, TreePtr};

const RW: MapFlags = PROT_READ.union(PROT_WRITE);

fn new_fs() -> FileSystem<DiskMemory> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    FileSystem::create(
        DiskMemory::new(64 << 20),
        None,
        now.as_secs(),
        now.subsec_nanos(),
    )
    .unwrap()
}

fn create_file(fs: &mut FileSystem<DiskMemory>, name: &str, data: &[u8]) -> TreePtr<Node> {
    fs.tx(|tx| {
        let node = tx.create_node(TreePtr::root(), name, Node::MODE_FILE | 0o644, 1, 0)?;
        tx.write_node(node.ptr(), 0, data, 1, 0)?;
        Ok(node.ptr())
    })
    .unwrap()
}

fn file_contents(fs: &mut FileSystem<DiskMemory>, node: TreePtr<Node>) -> Vec<u8> {
    fs.tx(|tx| {
        let size = tx.read_tree(node)?.data().size();
        let mut buf = vec![0; size as usize];
        let count = tx.read_node(node, 0, &mut buf, 0, 0)?;
        buf.truncate(count);
        Ok(buf)
    })
    .unwrap()
}

/// View `len` bytes of cache memory at `ptr`, as a guest mapping would.
fn mapped<'a>(ptr: *mut u8, len: usize) -> &'a mut [u8] {
    unsafe { slice::from_raw_parts_mut(ptr, len) }
}

#[test]
fn fmap_loads_file_contents_and_zeroes_past_eof() {
    let mut fs = new_fs();
    let node = create_file(&mut fs, "file", b"hello");
    let mut fmaps = Fmaps::new();

    let ptr = fs
        .tx(|tx| fmap(&mut fmaps, node, PROT_READ, 4096, 0, tx))
        .unwrap();

    let page = mapped(ptr, 4096);
    assert_eq!(&page[..5], b"hello");
    assert!(page[5..].iter().all(|&b| b == 0));
}

#[test]
fn funmap_writes_back_without_growing_file() {
    let mut fs = new_fs();
    let node = create_file(&mut fs, "file", b"hello");
    let mut fmaps = Fmaps::new();

    let ptr = fs.tx(|tx| fmap(&mut fmaps, node, RW, 4096, 0, tx)).unwrap();
    let page = mapped(ptr, 4096);
    page[..5].copy_from_slice(b"HELLO");
    // Past the end of the file: must not be written back
    page[100] = 0xff;

    fs.tx(|tx| funmap(&mut fmaps, node, 0, 4096, tx)).unwrap();

    assert_eq!(file_contents(&mut fs, node), b"HELLO");
    assert!(fmaps.is_empty());
}

#[test]
fn read_only_mapping_is_not_written_back() {
    let mut fs = new_fs();
    let node = create_file(&mut fs, "file", b"hello");
    let mut fmaps = Fmaps::new();

    let ptr = fs
        .tx(|tx| fmap(&mut fmaps, node, PROT_READ, 4096, 0, tx))
        .unwrap();
    mapped(ptr, 5).copy_from_slice(b"HELLO");
    fs.tx(|tx| funmap(&mut fmaps, node, 0, 4096, tx)).unwrap();

    assert_eq!(file_contents(&mut fs, node), b"hello");
}

#[test]
fn read_sees_unsynced_mapped_writes() {
    let mut fs = new_fs();
    let node = create_file(&mut fs, "file", &[b'a'; 8192]);
    let mut fmaps = Fmaps::new();

    let ptr = fs
        .tx(|tx| fmap(&mut fmaps, node, RW, 4096, 4096, tx))
        .unwrap();
    mapped(ptr, 4).copy_from_slice(b"BBBB");

    // Straddles the mapped range: unmapped part from disk, mapped part from cache
    let mut buf = [0; 8];
    let count = fs
        .tx(|tx| read(&mut fmaps, node, &mut buf, 4092, tx))
        .unwrap();
    assert_eq!(count, 8);
    assert_eq!(&buf, b"aaaaBBBB");
}

#[test]
fn write_updates_mapped_pages() {
    let mut fs = new_fs();
    let node = create_file(&mut fs, "file", &[b'a'; 8192]);
    let mut fmaps = Fmaps::new();

    let ptr = fs.tx(|tx| fmap(&mut fmaps, node, RW, 8192, 0, tx)).unwrap();
    fs.tx(|tx| write(&mut fmaps, node, b"xyz", 4095, tx))
        .unwrap();

    assert_eq!(&mapped(ptr, 8192)[4094..4099], b"axyza");
    assert_eq!(&file_contents(&mut fs, node)[4094..4099], b"axyza");
}

#[test]
fn extending_write_zeroes_mapped_gap() {
    let mut fs = new_fs();
    let node = create_file(&mut fs, "file", b"hello");
    let mut fmaps = Fmaps::new();

    let ptr = fs.tx(|tx| fmap(&mut fmaps, node, RW, 4096, 0, tx)).unwrap();
    let page = mapped(ptr, 4096);
    // Junk written through the mapping past the end of the file
    page[100] = 0xff;

    fs.tx(|tx| write(&mut fmaps, node, b"end", 200, tx))
        .unwrap();

    assert_eq!(page[100], 0);
    assert_eq!(&page[200..203], b"end");

    let mut buf = [0xaa; 203];
    let count = fs.tx(|tx| read(&mut fmaps, node, &mut buf, 0, tx)).unwrap();
    assert_eq!(count, 203);
    assert_eq!(&buf[..5], b"hello");
    assert!(buf[5..200].iter().all(|&b| b == 0));
    assert_eq!(&buf[200..], b"end");
}

#[test]
fn truncate_zeroes_mapped_tail() {
    let mut fs = new_fs();
    let node = create_file(&mut fs, "file", b"hello");
    let mut fmaps = Fmaps::new();

    let ptr = fs.tx(|tx| fmap(&mut fmaps, node, RW, 4096, 0, tx)).unwrap();
    fs.tx(|tx| truncate(&mut fmaps, node, 2, tx)).unwrap();
    assert_eq!(&mapped(ptr, 5)[..], b"he\0\0\0");

    fs.tx(|tx| truncate(&mut fmaps, node, 4, tx)).unwrap();
    fs.tx(|tx| funmap(&mut fmaps, node, 0, 4096, tx)).unwrap();
    assert_eq!(file_contents(&mut fs, node), b"he\0\0");
}

#[test]
fn funmap_releases_adjacent_ranges() {
    let mut fs = new_fs();
    let node = create_file(&mut fs, "file", &[b'a'; 8192]);
    let mut fmaps = Fmaps::new();

    fs.tx(|tx| fmap(&mut fmaps, node, RW, 4096, 0, tx)).unwrap();
    fs.tx(|tx| fmap(&mut fmaps, node, PROT_READ, 4096, 4096, tx))
        .unwrap();

    fs.tx(|tx| funmap(&mut fmaps, node, 0, 8192, tx)).unwrap();

    assert!(fmaps.is_empty());
}

#[test]
fn partial_funmap_keeps_rest_mapped() {
    let mut fs = new_fs();
    let node = create_file(&mut fs, "file", &[b'a'; 8192]);
    let mut fmaps = Fmaps::new();

    let ptr = fs.tx(|tx| fmap(&mut fmaps, node, RW, 8192, 0, tx)).unwrap();
    fs.tx(|tx| funmap(&mut fmaps, node, 0, 4096, tx)).unwrap();
    assert!(fmaps[&node.id()].mapped());

    // Still coherent after the partial unmap
    mapped(ptr, 8192)[4096] = b'b';
    fs.tx(|tx| funmap(&mut fmaps, node, 4096, 4096, tx))
        .unwrap();
    assert!(fmaps.is_empty());
    assert_eq!(file_contents(&mut fs, node)[4096], b'b');
}

#[test]
fn overlapping_fmaps_are_reference_counted() {
    let mut fs = new_fs();
    let node = create_file(&mut fs, "file", &[b'a'; 4096]);
    let mut fmaps = Fmaps::new();

    let first = fs.tx(|tx| fmap(&mut fmaps, node, RW, 4096, 0, tx)).unwrap();
    let second = fs.tx(|tx| fmap(&mut fmaps, node, RW, 4096, 0, tx)).unwrap();
    assert_eq!(first, second);

    mapped(first, 1)[0] = b'b';
    fs.tx(|tx| funmap(&mut fmaps, node, 0, 4096, tx)).unwrap();
    assert!(fmaps[&node.id()].mapped());
    assert_eq!(file_contents(&mut fs, node)[0], b'b');

    fs.tx(|tx| funmap(&mut fmaps, node, 0, 4096, tx)).unwrap();
    assert!(fmaps.is_empty());
}

#[test]
fn buffer_grows_while_mapped() {
    let mut fs = new_fs();
    let node = create_file(&mut fs, "file", b"start");
    let far = 100 << 20;
    fs.tx(|tx| {
        tx.write_node(node, far, b"far", 1, 0)?;
        Ok(())
    })
    .unwrap();
    let mut fmaps = Fmaps::new();

    let near = fs.tx(|tx| fmap(&mut fmaps, node, RW, 4096, 0, tx)).unwrap();
    mapped(near, 5).copy_from_slice(b"START");

    let far_ptr = fs
        .tx(|tx| fmap(&mut fmaps, node, RW, 4096, far, tx))
        .unwrap();
    assert_eq!(&mapped(far_ptr, 3)[..], b"far");

    // Growth preserved the near range's unsynced contents
    let mut buf = [0; 5];
    fs.tx(|tx| read(&mut fmaps, node, &mut buf, 0, tx)).unwrap();
    assert_eq!(&buf, b"START");

    fs.tx(|tx| funmap(&mut fmaps, node, 0, 4096, tx)).unwrap();
    fs.tx(|tx| funmap(&mut fmaps, node, far, 4096, tx)).unwrap();
    assert!(fmaps.is_empty());
    assert_eq!(&file_contents(&mut fs, node)[..5], b"START");
}

#[test]
fn sync_writes_back_and_keeps_mapping() {
    let mut fs = new_fs();
    let node = create_file(&mut fs, "file", b"hello");
    let mut fmaps = Fmaps::new();

    let ptr = fs.tx(|tx| fmap(&mut fmaps, node, RW, 4096, 0, tx)).unwrap();
    mapped(ptr, 5).copy_from_slice(b"world");
    fs.tx(|tx| sync(&mut fmaps, node, tx)).unwrap();

    assert_eq!(file_contents(&mut fs, node), b"world");
    assert!(fmaps[&node.id()].mapped());
}

#[test]
fn mapping_keeps_unlinked_file_alive() {
    let mut fs = new_fs();
    let node = create_file(&mut fs, "file", b"hello");
    let mut fmaps = Fmaps::new();

    let ptr = fs.tx(|tx| fmap(&mut fmaps, node, RW, 4096, 0, tx)).unwrap();
    fs.tx(|tx| tx.remove_node(TreePtr::root(), "file", Node::MODE_FILE))
        .unwrap();

    // Still readable and writable through the mapping
    assert_eq!(&mapped(ptr, 5)[..], b"hello");
    mapped(ptr, 5).copy_from_slice(b"HELLO");
    fs.tx(|tx| sync(&mut fmaps, node, tx)).unwrap();
    assert_eq!(file_contents(&mut fs, node), b"HELLO");

    // Unmapping releases the node
    fs.tx(|tx| funmap(&mut fmaps, node, 0, 4096, tx)).unwrap();
    assert!(!fs.node_usages.contains_key(&node.id()));
}

#[test]
fn fmap_fills_gaps_around_loaded_range() {
    let mut fs = new_fs();
    let data: Vec<u8> = (0..3 * 4096).map(|i| (i / 4096) as u8 + b'a').collect();
    let node = create_file(&mut fs, "file", &data);
    let mut fmaps = Fmaps::new();

    let middle = fs
        .tx(|tx| fmap(&mut fmaps, node, RW, 4096, 4096, tx))
        .unwrap();
    // Unsynced change in the loaded range must survive the wider fmap
    mapped(middle, 1)[0] = b'X';

    let all = fs
        .tx(|tx| fmap(&mut fmaps, node, RW, 3 * 4096, 0, tx))
        .unwrap();
    let pages = mapped(all, 3 * 4096);
    assert_eq!(pages[0], b'a');
    assert_eq!(pages[4096], b'X');
    assert_eq!(pages[4097], b'b');
    assert_eq!(pages[2 * 4096], b'c');
}

/// Growth past the reservation, when something is mapped right after it.
#[cfg(not(any(target_os = "redox", target_os = "linux")))]
#[test]
fn buffer_grows_in_place_or_moves_only_when_allowed() {
    use super::FmapBuffer;

    let mut buffer = FmapBuffer::new(4096, 0).unwrap();
    let base = buffer.base();
    mapped(base, 5).copy_from_slice(b"keep!");
    let reserved = 64 << 20;

    // Occupy the range right after the reservation
    let blocker = unsafe {
        libc::mmap(
            base.add(reserved).cast(),
            4096,
            libc::PROT_READ,
            // A hint, not MAP_FIXED: never clobber whatever may be there
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    assert_eq!(blocker, unsafe { base.add(reserved) }.cast());

    // Mapped buffers cannot move, and must not clobber the neighbour
    let err = buffer.grow(reserved + 4096, false).unwrap_err();
    assert_eq!(err.errno, syscall::error::ENOMEM);
    assert_eq!(buffer.base(), base);

    unsafe { libc::munmap(blocker, 4096) };

    // Once the neighbour is gone, growth happens in place
    buffer.grow(reserved + 4096, false).unwrap();
    assert_eq!(buffer.base(), base);
    assert_eq!(buffer.size(), reserved + 4096);
    assert_eq!(&mapped(base, 5)[..], b"keep!");
    // The extension is usable
    mapped(unsafe { base.add(reserved) }, 1)[0] = 1;
}

/// Unmapped buffers may move when they cannot grow in place.
#[cfg(not(any(target_os = "redox", target_os = "linux")))]
#[test]
fn unmapped_buffer_moves_to_grow() {
    use super::FmapBuffer;

    let mut buffer = FmapBuffer::new(4096, 0).unwrap();
    let base = buffer.base();
    mapped(base, 5).copy_from_slice(b"keep!");
    let reserved = 64 << 20;
    let blocker = unsafe {
        libc::mmap(
            base.add(reserved).cast(),
            4096,
            libc::PROT_READ,
            // A hint, not MAP_FIXED: never clobber whatever may be there
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };

    assert_eq!(blocker, unsafe { base.add(reserved) }.cast());

    buffer.grow(reserved + 4096, true).unwrap();

    assert_ne!(buffer.base(), base);
    assert_eq!(&mapped(buffer.base(), 5)[..], b"keep!");
    unsafe { libc::munmap(blocker, 4096) };
}
