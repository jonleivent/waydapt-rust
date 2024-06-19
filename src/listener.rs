#![allow(unused)]
#![forbid(unsafe_code)]

use nix::NixPath;
use std::env;
use std::fs::{remove_file, File, Metadata};
use std::ops::Deref;
use std::os::unix::io::OwnedFd;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process;

pub(crate) struct SocketListener {
    socket_path: PathBuf,
    unix_listener: UnixListener,
    lock_path: PathBuf,
    lock_fd: OwnedFd,
    do_removes: bool,
}

// Using the pid test won't work because we might daemonize after creating the SocketListener.
// Also, if we try to update the pid after daemonizing, that's too late because the socket and lock
// file would get removed during the daemonization.  Instead, we have to

impl Drop for SocketListener {
    fn drop(&mut self) {
        if self.do_removes {
            // Note that remove_file is never a truly safe operation to do because some other
            // process may have taken over that path for its own use.  There is no Linux way to
            // prevent this, however.  We could keep a fd to the enclosing dir and use unlinkat -
            // that would prevent some cases of take-over (such as replacing the dir, or mounting
            // over it), but not others.  It would be nice if Linux had a version of unlink that
            // took a path AND some other proof that we have the right file, like a dev/inode pair,
            // and verify atomically that the path and dev/inode pair refer to the same file before
            // unlinking atomically with that verification.
            let _ = remove_file(&self.socket_path);
            let _ = remove_file(&self.lock_path);
        }
    }
}

impl Deref for SocketListener {
    type Target = UnixListener;
    fn deref(&self) -> &Self::Target {
        &self.unix_listener
    }
}

impl SocketListener {
    pub(crate) fn new(display_name: &PathBuf) -> Self {
        // see wayland-rs/wayland-server/src/socket.rs
        let socket_path = if display_name.is_absolute() {
            display_name.clone()
        } else {
            let xdg_runtime_dir: PathBuf = env::var("XDG_RUNTIME_DIR").unwrap().into();
            if !xdg_runtime_dir.is_absolute() {
                panic!("XDG_RUNTIME_DIR is not absolute: {xdg_runtime_dir:?}")
            }
            xdg_runtime_dir.join(display_name)
        };
        let lock_path = socket_path.with_extension("lock");
        let lock_fd = lock_file(&lock_path).into();
        if socket_path.try_exists().unwrap() {
            remove_file(&socket_path).unwrap();
        }
        let unix_listener = UnixListener::bind(&socket_path).unwrap();
        let pid_when_created = process::id();
        Self { socket_path, unix_listener, lock_path, lock_fd, do_removes: true }
    }

    pub(crate) fn drop_without_removes(mut self) {
        self.do_removes = false;
    }
}

fn metadata_eq(meta1: &Metadata, meta2: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta1.dev() == meta2.dev() && meta1.ino() == meta2.ino()
}

fn file_still_at_path(file: &File, path: &PathBuf) -> bool {
    use std::fs::metadata;
    use std::io::ErrorKind;
    use std::os::unix::fs::OpenOptionsExt;
    let file_meta = file.metadata().unwrap();
    match metadata(path) {
        Ok(disk_meta) => metadata_eq(&file_meta, &disk_meta),
        Err(err) if err.kind() != ErrorKind::NotFound => panic!(),
        _ => false,
    }
}

fn lock_file(path: &PathBuf) -> File {
    // panics if it can't lock
    use rustix::fs::{flock, FlockOperation};
    use std::os::unix::fs::OpenOptionsExt;
    loop {
        let lock_file = File::options()
            .truncate(true)
            .create(true)
            .read(true)
            .write(true)
            .mode(0o660)
            .open(path)
            .unwrap();
        flock(&lock_file, FlockOperation::NonBlockingLockExclusive).unwrap();
        // There is a potential data race between the above file open and flock vs. the
        // remove_file call in SocketListener::drop.  The remove_file is done on the lock path
        // while the flock is held.  However, if server A starts up and gets to the above open
        // call, then server B does its remove_file, then server A gets to the flock, server A
        // will be locking an unlinked file.  Which means it won't be protecting its usage of
        // the socket vs. subsequent server startups.  So loop to make sure that we flock the
        // same file that we opened/created.
        if file_still_at_path(&lock_file, path) {
            return lock_file;
        }
    }
}

// get the path to the wayland server's (not our!) socket
pub(crate) fn get_server_socket_path() -> PathBuf {
    let socket_path: PathBuf = env::var("WAYLAND_DISPLAY").unwrap_or("wayland-0".into()).into();
    if socket_path.is_absolute() {
        return socket_path;
    }
    let Ok(xdg_runtime_dir) = env::var("XDG_RUNTIME_DIR") else {
        panic!("XDG_RUNTIME_DIR is not set")
    };
    let xdg_runtime_path = PathBuf::from(xdg_runtime_dir);
    if !xdg_runtime_path.is_absolute() {
        panic!("XDG_RUNTIME_DIR is not absolute: {xdg_runtime_path:?}")
    }
    xdg_runtime_path.join(socket_path)
}
