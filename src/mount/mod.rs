#[cfg(all(target_os = "linux", not(fuzzing), feature = "fuse"))]
mod fuse;

#[cfg(all(target_os = "linux", fuzzing, feature = "fuse"))]
pub mod fuse;

#[cfg(all(target_os = "linux", feature = "fuse"))]
pub use self::fuse::mount;

#[cfg(all(
    not(target_os = "redox"),
    not(fuzzing),
    not(all(target_os = "linux", feature = "fuse"))
))]
mod stub;

#[cfg(all(
    not(target_os = "redox"),
    fuzzing,
    not(all(target_os = "linux", feature = "fuse"))
))]
pub mod stub;

#[cfg(all(
    not(target_os = "redox"),
    not(all(target_os = "linux", feature = "fuse"))
))]
pub use self::stub::mount;

#[cfg(target_os = "redox")]
mod redox;

#[cfg(target_os = "redox")]
pub use self::redox::mount;
