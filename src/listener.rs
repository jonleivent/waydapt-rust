#![forbid(unsafe_code)]

use std::env;
use std::fs::{remove_file, File, Metadata};
use std::ops::Deref;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::OnceLock;

pub(crate) struct SocketListener {
    socket_path: PathBuf,
    unix_listener: UnixListener,
    lock_path: PathBuf,
    #[allow(unused)]
    lock_file: File, // own this so that it drops (and so unlocks) with us
}

impl Drop for SocketListener {
    fn drop(&mut self) {
        // Note that remove_file is never a truly safe operation because some other process may have
        // taken over that path for its own use.  There is no Linux way to prevent this, however.
        // We could keep a fd to the enclosing dir and use unlinkat - that would prevent some cases
        // of take-over (such as replacing the dir, or mounting over it), but not others.  It would
        // be nice if Linux had a version of unlink that took a path AND some other proof that we
        // have the right file, like a dev/inode pair, and verify atomically that the path and
        // dev/inode pair refer to the same file before unlinking atomically with that verification.
        remove_file(&self.socket_path).unwrap();
        remove_file(&self.lock_path).unwrap();
    }
}

impl SocketListener {
    pub(crate) fn new(display_name: &PathBuf) -> Self {
        // see wayland-rs/wayland-server/src/socket.rs
        let socket_path = if display_name.is_absolute() {
            display_name.clone()
        } else {
            let xdg_runtime_dir: PathBuf = env::var("XDG_RUNTIME_DIR").unwrap().into();
            assert!(
                xdg_runtime_dir.is_absolute(),
                "XDG_RUNTIME_DIR is not absolute: {xdg_runtime_dir:?}"
            );
            xdg_runtime_dir.join(display_name)
        };
        let lock_path = socket_path.with_extension("lock");
        let lock_file = lock_file(&lock_path);
        if socket_path.try_exists().unwrap() {
            remove_file(&socket_path).unwrap();
        }
        let unix_listener = UnixListener::bind(&socket_path).unwrap();
        unix_listener.set_nonblocking(true).unwrap();
        Self { socket_path, unix_listener, lock_path, lock_file }
    }
}

impl Deref for SocketListener {
    type Target = UnixListener;
    #[inline]
    fn deref(&self) -> &Self::Target { &self.unix_listener }
}

fn metadata_eq(meta1: &Metadata, meta2: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta1.dev() == meta2.dev() && meta1.ino() == meta2.ino()
}

fn file_still_at_path(file: &File, path: &PathBuf) -> bool {
    use std::fs::metadata;
    use std::io::ErrorKind;
    let file_meta = file.metadata().unwrap();
    match metadata(path) {
        Ok(disk_meta) => metadata_eq(&file_meta, &disk_meta),
        Err(err) if err.kind() != ErrorKind::NotFound => panic!("{err:?}"),
        _ => false,
    }
}

fn lock_file(path: &PathBuf) -> File {
    // panics if it can't lock
    use rustix::fs::{flock, FlockOperation};
    use std::os::unix::fs::OpenOptionsExt;
    loop {
        let lock_file = File::options()
            .truncate(false)
            .create(true)
            .read(true)
            .write(true)
            .mode(0o660)
            .open(path)
            .unwrap();
        flock(&lock_file, FlockOperation::NonBlockingLockExclusive)
            .unwrap_or_else(|_| panic!("Lock file {path:?} is already locked"));
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

// get the path to the wayland server's (not our!) socket.  This is done for every client session,
// but never changes, so memoize in a static.
pub(crate) fn get_server_socket_path() -> &'static PathBuf {
    static SOCKET_PATH: OnceLock<PathBuf> = OnceLock::new();

    SOCKET_PATH.get_or_init(|| {
        let socket_path: PathBuf = env::var("WAYLAND_DISPLAY").unwrap_or("wayland-0".into()).into();
        if socket_path.is_absolute() {
            return socket_path;
        }
        let Ok(xdg_runtime_dir) = env::var("XDG_RUNTIME_DIR") else {
            panic!("XDG_RUNTIME_DIR is not set")
        };
        let xdg_runtime_path = PathBuf::from(xdg_runtime_dir);
        assert!(
            xdg_runtime_path.is_absolute(),
            "XDG_RUNTIME_DIR is not absolute: {xdg_runtime_path:?}"
        );
        xdg_runtime_path.join(socket_path)
    })
}
