#![doc = include_str!("../../README.md")]

use ::std::{
    ffi::{OsStr, OsString},
    future::Future,
    io,
    os::{
        fd::{BorrowedFd, FromRawFd, OwnedFd, RawFd},
        unix::ffi::OsStrExt,
    },
    panic::resume_unwind,
    path::Path,
};
use std::{pin::Pin, task::Poll};

use ::async_fs_utils_attr::in_blocking;
use ::futures_core::FusedStream;
use ::nix::{
    dir::Dir,
    errno::Errno,
    fcntl::{AtFlags, OFlag, OpenHow, RenameFlags},
    sys::stat::{FileStat, Mode},
    NixPath,
};
use ::reflink_at::{OnExists, ReflinkAtError};
use ::smallvec::SmallVec;
use ::tokio::task::{JoinError, JoinHandle};
use ::tokio_stream::Stream;

/// Re-export of nix that matches version used by crate.
pub use nix;

/// SmallVec backed path, used for sending paths.
#[derive(Debug)]
struct OwnedPath(SmallVec<[u8; 32]>);

impl OwnedPath {
    /// Create a new owned path from a path-like.
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        Self(path.as_ref().as_os_str().as_bytes().into())
    }
}

impl NixPath for OwnedPath {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn with_nix_path<T, F>(&self, f: F) -> nix::Result<T>
    where
        F: FnOnce(&std::ffi::CStr) -> T,
    {
        OsStr::from_bytes(&self.0).with_nix_path(f)
    }
}

impl AsRef<Path> for OwnedPath {
    fn as_ref(&self) -> &Path {
        Path::new(OsStr::from_bytes(&self.0))
    }
}

impl From<&Path> for OwnedPath {
    fn from(value: &Path) -> Self {
        Self::new(value)
    }
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

/// Read a directory as a stream.
pub fn read_dir(dir: Dir) -> impl Send + FusedStream<Item = Result<nix::dir::Entry, Errno>> {
    /// Read dir stream.
    #[derive(Debug)]
    struct ReadDir(Option<JoinHandle<Option<IterEntry>>>);

    /// Iter entry pair.
    #[derive(Debug)]
    struct IterEntry(nix::dir::OwningIter, Result<nix::dir::Entry, Errno>);

    impl Stream for ReadDir {
        type Item = Result<nix::dir::Entry, Errno>;

        fn poll_next(
            mut self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> Poll<Option<Self::Item>> {
            // If no JoinHandle exists stream has finished.
            let Some(mut handle) = self.0.take() else {
                return Poll::Ready(None);
            };

            // Only case that returns pending waking handled by poll of JoinHandle.
            let Poll::Ready(value) = Pin::new(&mut handle).poll(cx) else {
                self.0 = Some(handle);
                return Poll::Pending;
            };

            // Handle returned finished stream. Might panic Might panic.
            let Some(IterEntry(mut iter, entry)) = unwrap_joined(value) else {
                return Poll::Ready(None);
            };

            // Get next entry.
            self.0 = Some(tokio::task::spawn_blocking(move || {
                iter.next().map(|next| IterEntry(iter, next))
            }));

            Poll::Ready(Some(entry))
        }
    }

    impl FusedStream for ReadDir {
        fn is_terminated(&self) -> bool {
            self.0.is_none()
        }
    }

    ReadDir(Some(tokio::task::spawn_blocking(move || {
        let mut iter = dir.into_iter();
        iter.next().map(|next| IterEntry(iter, next))
    })))
}

#[in_blocking(wrapped = nix::sys::stat::fstat, defer_err)]
fn file_stat(
    /// file to stat
    fd: RawFd,
) -> Result<FileStat, Errno> {
    nix::sys::stat::fstat(fd)
}

#[in_blocking(wrapped = nix::sys::stat::fstatat, defer_err)]
fn file_stat_at(
    /// Directory to resolve path in.
    fd: Option<RawFd>,
    /// File to stat.
    #[path]
    path: OwnedPath,
    /// Flags used when resolving path.
    at_flags: AtFlags,
) -> Result<FileStat, Errno> {
    nix::sys::stat::fstatat(fd, &path, at_flags)
}

#[in_blocking(wrapped = nix::dir::Dir::from, defer_err)]
fn open_dir(
    /// Directory to open. Responsibility to close it is transferred to returned object.
    #[raw_fd]
    fd: RawFd,
) -> Result<Dir, Errno> {
    Dir::from_fd(fd)
}

#[in_blocking(wrapped = OwnedFd::try_clone, defer_err)]
fn clone_fd(
    /// File descriptor to clone.
    fd: OwnedFd,
) -> (OwnedFd, Result<OwnedFd, io::Error>) {
    let cloned = fd.try_clone();
    (fd, cloned)
}

#[in_blocking(wrapped = nix::fcntl::openat2, defer_err)]
fn open_at_2(
    /// Direcory to resolve path in.
    dir_fd: RawFd,
    /// File to open.
    #[path]
    path: OwnedPath,
    /// How to open file, such as how paths are resolved and what mode new files use.
    how: OpenHow,
) -> Result<OwnedFd, Errno> {
    nix::fcntl::openat2(dir_fd, &path, how).map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
}

#[in_blocking(wrapped = nix::fcntl::openat, defer_err)]
fn open_at(
    /// Directory to resolve path in.
    dir_fd: Option<RawFd>,
    /// File to open.
    #[path]
    path: OwnedPath,
    /// What mode to use if creating the file.
    mode: Mode,
    /// File open flags, such as O_RDWR for read-write or O_RDONLY for read-only.
    flags: OFlag,
) -> Result<OwnedFd, Errno> {
    nix::fcntl::openat(dir_fd, &path, flags, mode).map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
}

#[in_blocking(wrapped = nix::fcntl::readlinkat, defer_err)]
fn read_link_at(
    /// Directory to resolve path in.
    dir_fd: Option<RawFd>,
    /// Path to link to read.
    #[path]
    path: OwnedPath,
) -> Result<OsString, Errno> {
    nix::fcntl::readlinkat(dir_fd, &path)
}

/// ´dir_fd´ and ´dest´ specify on what filesystem to create the reflink.
#[in_blocking(wrapped = reflink_at::reflink_unlinked, defer_err)]
fn reflink_unlinked(
    /// Directory to resolve dest in.
    dir_fd: Option<RawFd>,
    /// Where to create reflink.
    #[path]
    dest: OwnedPath,
    /// File to reflik to.
    src: RawFd,
    /// What file mode to use for created file.
    mode: Mode,
) -> Result<OwnedFd, Errno> {
    reflink_at::reflink_unlinked(
        dir_fd.map(|dir_fd| unsafe { BorrowedFd::borrow_raw(dir_fd) }),
        dest.as_ref(),
        unsafe { BorrowedFd::borrow_raw(src) },
        mode,
    )
}

#[in_blocking(wrapped = reflink_at::reflink, defer_err)]
fn reflink(
    /// File to overwrite with reflink.
    dest: RawFd,
    /// File to reflink to.
    src: RawFd,
) -> Result<(), Errno> {
    reflink_at::reflink(unsafe { BorrowedFd::borrow_raw(dest) }, unsafe {
        BorrowedFd::borrow_raw(src)
    })
}

#[in_blocking(wrapped = reflink_at::reflink_at, defer_err)]
fn reflink_at(
    /// Directory file descriptor to resolve dest in.
    dir_fd: Option<RawFd>,
    /// Where to create reflink.
    #[path]
    dest: OwnedPath,
    /// File to reflink to.
    src: RawFd,
    /// What file mode to use for created reflink.
    mode: Mode,
    /// How to handle a file existing at dest.
    on_exists: OnExists,
) -> Result<OwnedFd, ReflinkAtError> {
    reflink_at::reflink_at(
        dir_fd.map(|dir_fd| unsafe { BorrowedFd::borrow_raw(dir_fd) }),
        dest.as_ref(),
        unsafe { BorrowedFd::borrow_raw(src) },
        mode,
        on_exists,
    )
}

#[in_blocking(wrapped = nix::fcntl::renameat, defer_err)]
fn rename_at(
    /// Directory to resolve old_path in.
    old_dir_fd: Option<RawFd>,
    /// File to rename.
    #[path]
    old_path: OwnedPath,
    /// Directory to resolve new_path in.
    new_dir_fd: Option<RawFd>,
    /// What to rename file to.
    #[path]
    new_path: OwnedPath,
) -> Result<(), Errno> {
    nix::fcntl::renameat(old_dir_fd, &old_path, new_dir_fd, &new_path)
}

#[in_blocking(wrapped = nix::fcntl::renameat2, defer_err)]
fn rename_at_2(
    /// Directory to resolve old_path in.
    old_dir_fd: Option<RawFd>,
    /// File to rename.
    #[path]
    old_path: OwnedPath,
    /// Directory to resolve new_path in.
    new_dir_fd: Option<RawFd>,
    /// What to rename file to.
    #[path]
    new_path: OwnedPath,
    /// Additional flags to use when renaming, such as whether or not to replace existing files or
    /// swap with them.
    flags: RenameFlags,
) -> Result<(), Errno> {
    nix::fcntl::renameat2(old_dir_fd, &old_path, new_dir_fd, &new_path, flags)
}
