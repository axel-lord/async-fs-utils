#![doc = include_str!("../README.md")]

use ::std::{
    ffi::{OsStr, OsString},
    io,
    os::{
        fd::{BorrowedFd, FromRawFd, OwnedFd, RawFd},
        unix::ffi::OsStrExt,
    },
    panic::resume_unwind,
    path::Path,
};

use ::nix::{
    dir::Dir,
    errno::Errno,
    fcntl::{AtFlags, OFlag, OpenHow, ResolveFlag},
    sys::stat::{FileStat, Mode},
};
use ::smallvec::SmallVec;
use ::tokio::task::spawn_blocking;
use ::tokio::{sync::mpsc::error::SendError, task::JoinError};
use ::tokio_stream::Stream;

/// Re-export of nix that matches version used by crate.
pub use nix;

/// Construct an owned path from a reference.
fn owned_path<P: AsRef<Path>>(path: P) -> SmallVec<[u8; 32]> {
    path.as_ref().as_os_str().as_bytes().into()
}

/// Unwrap a join result by either panic or repanic.
///
/// # Panics
/// If the thread did panic or was canceled.
fn unwrap_joined<T>(t: Result<T, JoinError>) -> T {
    let err = match t {
        Ok(t) => return t,
        Err(err) => err,
    };

    match err.try_into_panic() {
        Ok(p) => resume_unwind(p),
        Err(err) => panic!("blocking thread canceled, {err}"),
    }
}

/// Run [nix::sys::stat::fstat] in a blocking thread.
///
/// # Errors
/// See [nix::sys::stat::fstat].
pub async fn file_stat(fd: RawFd) -> Result<FileStat, Errno> {
    unwrap_joined(spawn_blocking(move || nix::sys::stat::fstat(fd)).await)
}

/// Run [nix::sys::stat::fstatat] in a blocking thread.
///
/// # Errors
/// See [nix::sys::stat::fstatat].
pub async fn file_stat_at<P>(
    fd: Option<RawFd>,
    path: P,
    at_flags: AtFlags,
) -> Result<FileStat, Errno>
where
    P: AsRef<Path> + Send,
{
    let path = owned_path(path);
    unwrap_joined(
        spawn_blocking(move || {
            let path = OsStr::from_bytes(&path);
            nix::sys::stat::fstatat(fd, path, at_flags)
        })
        .await,
    )
}

/// Run [nix::dir::Dir::from] in a blocking thread.
///
/// # Errors
/// See [nix::dir::Dir::from].
pub async fn open_dir(fd: OwnedFd) -> Result<Dir, Errno> {
    unwrap_joined(spawn_blocking(move || Dir::from(fd)).await)
}

/// Read a directory as a stream.
pub fn read_dir(dir: Dir) -> impl Stream<Item = Result<nix::dir::Entry, Errno>> {
    let (tx, rx) = tokio::sync::mpsc::channel(64);

    spawn_blocking(move || {
        for value in dir.into_iter() {
            if let Err(SendError(t)) = tx.blocking_send(value) {
                log::error!("read_dir, failure to send item, {t:?}")
            }
        }
    });

    tokio_stream::wrappers::ReceiverStream::new(rx)
}

/// Clone a file descriptor.
///
/// # Errors
/// If the file descriptor cannot be cloned.
pub async fn clone_fd(fd: OwnedFd) -> (OwnedFd, Result<OwnedFd, io::Error>) {
    unwrap_joined(
        spawn_blocking(move || {
            let cloned = fd.try_clone();
            (fd, cloned)
        })
        .await,
    )
}

/// Run [nix::fcntl::openat2] in a blocking thread.
///
/// # Errors
/// See [nix::fcntl::openat2].
pub async fn open_at_2<P>(
    dir_fd: RawFd,
    path: P,
    mode: Mode,
    flags: OFlag,
    resolve: ResolveFlag,
) -> Result<OwnedFd, Errno>
where
    P: AsRef<Path> + Send,
{
    let path = owned_path(path);
    unwrap_joined(
        spawn_blocking(move || {
            let path = OsStr::from_bytes(&path);
            nix::fcntl::openat2(
                dir_fd,
                path,
                OpenHow::new().flags(flags).mode(mode).resolve(resolve),
            )
            .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
        })
        .await,
    )
}

/// Run [nix::fcntl::openat] in a blocking thread.
///
/// # Errors
/// See [nix::fcntl::openat].
pub async fn open_at<P>(
    dir_fd: Option<RawFd>,
    path: P,
    mode: Mode,
    flags: OFlag,
) -> Result<OwnedFd, Errno>
where
    P: AsRef<Path> + Send,
{
    let path = owned_path(path);
    unwrap_joined(
        spawn_blocking(move || {
            let path = OsStr::from_bytes(&path);
            nix::fcntl::openat(dir_fd, path, flags, mode)
                .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
        })
        .await,
    )
}

/// Run [nix::fcntl::readlinkat] in a blocking thread.
///
/// # Errors
/// See [nix::fcntl::readlinkat].
pub async fn read_link_at<P>(dir_fd: Option<RawFd>, path: P) -> Result<OsString, Errno>
where
    P: AsRef<Path> + Send,
{
    let path = owned_path(path);
    unwrap_joined(
        spawn_blocking(move || {
            let path = OsStr::from_bytes(&path);
            nix::fcntl::readlinkat(dir_fd, path)
        })
        .await,
    )
}

/// Run [reflink_at::reflink_unlinked] in a blocking thread.
///
/// ´dir_fd´ and ´dest´ specify on what filesystem to create the reflink.
///
/// # Errors
/// See [reflink_at::reflink_unlinked].
pub async fn reflink_unlinked<P>(
    dir_fd: Option<RawFd>,
    dest: P,
    src: RawFd,
    mode: Mode,
) -> Result<OwnedFd, Errno>
where
    P: AsRef<Path> + Send,
{
    let path = owned_path(dest);
    unwrap_joined(
        spawn_blocking(move || {
            let dest = OsStr::from_bytes(&path);
            reflink_at::reflink_unlinked(
                dir_fd.map(|dir_fd| unsafe { BorrowedFd::borrow_raw(dir_fd) }),
                dest.as_ref(),
                unsafe { BorrowedFd::borrow_raw(src) },
                mode,
            )
        })
        .await,
    )
}

/// Run [reflink_at::reflink] in a blocking thread.
///
/// # Errors
/// See [reflink_at::reflink].
pub async fn reflink(dest: RawFd, src: RawFd) -> Result<(), Errno> {
    unwrap_joined(
        spawn_blocking(move || {
            reflink_at::reflink(unsafe { BorrowedFd::borrow_raw(dest) }, unsafe {
                BorrowedFd::borrow_raw(src)
            })
        })
        .await,
    )
}
