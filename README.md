# RedoxFS for FUSE and virtio-fs

This fork of [RedoxFS](https://gitlab.redox-os.org/redox-os/redoxfs) serves
RedoxFS images to Linux guests over virtio-fs, including DAX (guests map file
pages directly), and mounts them with FUSE on Linux.

- `redoxfs::fmap`: the page cache the Redox kernel borrows pages from when a
  RedoxFS file is `mmap`ed (`Fmap`/`FileMmapInfo` from the Redox scheme), made
  portable: buffers are memfds on Linux and address-stable memory on macOS, so
  a VMM can map them into a guest.
- `redoxfs::vfs::Vfs`: a thread-safe, FUSE-shaped view of a filesystem (inode
  numbers, lookup counts, setattr, readdir handles, ...). `setup_mapping` and
  `remove_mapping` implement `FUSE_SETUPMAPPING`/`FUSE_REMOVEMAPPING` against
  a VMM-provided `DaxWindow`.
- The `redoxfs` binary mounts images with FUSE through `Vfs`. FUSE is
  Linux-only: FUSE on macOS requires proprietary software.
- `vendor/libkrun` (a submodule, branch `redoxfs` of semistrict/libkrun) adds
  `krun_add_virtiofs_redoxfs()`, built with `make REDOXFS=1`. In the guest:
  `mount -t virtiofs <tag> /mnt -o dax=always`.

Clone with `git clone --recursive`.

### Testing

```sh
cargo nextest run                                   # macOS or Linux
scripts/test-linux.sh                               # Linux in Lima: FUSE, memfd cache
cargo +nightly check --target x86_64-unknown-redox  # the Redox scheme still builds
# End to end: boot a VM that uses a RedoxFS image through DAX
cd vendor/libkrun && make test REDOXFS=1 TEST=virtiofs-redoxfs
```

On macOS, libkrun's build needs `LIBCLANG_PATH` (e.g. `/opt/homebrew/opt/llvm/lib`).

## Upstream RedoxFS

This is the default filesystem of Redox OS inspired by [ZFS](https://docs.freebsd.org/en/books/handbook/zfs/) and adapted to a microkernel architecture.

(It's a replacement for [TFS](https://gitlab.redox-os.org/redox-os/tfs))

Current features:

- Compatible with Redox and Linux (FUSE)
- Copy-on-write
- Data/metadata checksums
- Transparent encryption
- Standard Unix file attributes
- File/directory size limit up to 193TiB (212TB)
- File/directory quantity limit up to 4 billion per 193TiB (2^32 - 1 = 4294967295)
- MIT licensed
- Disk encryption fully supported by the Redox bootloader, letting it load the kernel off an encrypted partition.

Being MIT licensed, RedoxFS can be bundled on GPL-licensed operating systems (Linux, for example).

### Install RedoxFS

```sh
cargo install redoxfs
```

You can also build RedoxFS from this repository.

### Configure your storage device to allow rootless usage

If you are on Linux you need root permission to acess block devices (storage), but it's recommended to run RedoxFS as rootless.

To do that you need to configure your storage device permission to your user with the following command:

```sh
sudo setfacl -m u:your-username:rw /path/to/disk
```

### Create, mount and customize your RedoxFS partition

See [the instructions in the book](https://doc.redox-os.org/book/redoxfs.html) for RedoxFS tooling usage.

Currently RedoxFS tooling are:

- `redoxfs` mount a RedoxFS disk
- `redoxfs-ar` write files to a RedoxFS disk
- `redoxfs-clone` clone a RedoxFS disk
- `redoxfs-mkfs` create an empty RedoxFS disk
- `redoxfs-resize` resize a RedoxFS disk

[![MIT licensed](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)
[![crates.io](http://meritbadge.herokuapp.com/redoxfs)](https://crates.io/crates/redoxfs)
[![docs.rs](https://docs.rs/redoxfs/badge.svg)](https://docs.rs/redoxfs)
