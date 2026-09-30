use std::ffi::OsString;
use std::fs::{self, File, Metadata, OpenOptions, Permissions};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::net::{UnixListener, UnixStream};
use tokio::time::{sleep, timeout};

const CONNECT_RETRIES: usize = 100;
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(20);
const STALE_SOCKET_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

pub type LocalServerStream = UnixStream;
pub type LocalClientStream = UnixStream;

#[derive(Debug)]
pub struct OwnerOnlyLocalListener {
    listener: UnixListener,
    endpoint: PathBuf,
    identity: FileIdentity,
    lock: EndpointLock,
}

impl OwnerOnlyLocalListener {
    pub async fn bind(endpoint: impl AsRef<Path>) -> io::Result<Self> {
        let owner_uid = current_euid();
        let endpoint = validated_endpoint(endpoint.as_ref(), owner_uid)?;
        let lock = EndpointLock::acquire(&endpoint, owner_uid)?;
        remove_stale_socket(&endpoint, owner_uid, &lock).await?;

        let listener = UnixListener::bind(&endpoint)?;
        validate_listener(&listener)?;
        let initial_metadata = match owned_socket_metadata(&endpoint, owner_uid) {
            Ok(metadata) => metadata,
            Err(error) => {
                drop(listener);
                return Err(error);
            }
        };
        let initial_identity = FileIdentity::from_metadata(&initial_metadata);

        if let Err(error) = fs::set_permissions(&endpoint, Permissions::from_mode(0o600)) {
            drop(listener);
            remove_socket_if_same(&endpoint, initial_identity, &lock);
            return Err(error);
        }

        let metadata = match owned_socket_metadata(&endpoint, owner_uid) {
            Ok(metadata)
                if FileIdentity::from_metadata(&metadata) == initial_identity
                    && metadata.permissions().mode() & 0o7777 == 0o600 =>
            {
                metadata
            }
            Ok(_) => {
                drop(listener);
                remove_socket_if_same(&endpoint, initial_identity, &lock);
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "local socket changed while securing {}",
                        endpoint.display()
                    ),
                ));
            }
            Err(error) => {
                drop(listener);
                remove_socket_if_same(&endpoint, initial_identity, &lock);
                return Err(error);
            }
        };

        Ok(Self {
            listener,
            endpoint,
            identity: FileIdentity::from_metadata(&metadata),
            lock,
        })
    }

    pub async fn accept(&mut self) -> io::Result<LocalServerStream> {
        loop {
            let (stream, _) = self.listener.accept().await?;
            let peer_uid = stream.peer_cred()?.uid();
            if trusted_peer_uid(peer_uid, self.identity.uid) {
                return Ok(stream);
            }
            drop(stream);
        }
    }
}

impl Drop for OwnerOnlyLocalListener {
    fn drop(&mut self) {
        remove_socket_if_same(&self.endpoint, self.identity, &self.lock);
    }
}

pub async fn connect_local_stream(endpoint: impl AsRef<Path>) -> io::Result<LocalClientStream> {
    let owner_uid = current_euid();
    let endpoint = validated_endpoint(endpoint.as_ref(), owner_uid)?;
    let mut last_error = None;

    for attempt in 0..CONNECT_RETRIES {
        match UnixStream::connect(&endpoint).await {
            Ok(stream) => {
                let peer_uid = stream.peer_cred()?.uid();
                if trusted_peer_uid(peer_uid, owner_uid) {
                    return Ok(stream);
                }
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "local socket peer for {} has untrusted uid {peer_uid}",
                        endpoint.display()
                    ),
                ));
            }
            Err(error) if retryable_connect_error(&error) => {
                let has_more_attempts = attempt + 1 < CONNECT_RETRIES;
                last_error = Some(error);
                if has_more_attempts {
                    sleep(CONNECT_RETRY_DELAY).await;
                }
            }
            Err(error) => return Err(error),
        }
    }

    Err(last_error.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("local socket was not available at {}", endpoint.display()),
        )
    }))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    uid: u32,
}

impl FileIdentity {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            uid: metadata.uid(),
        }
    }
}

#[derive(Debug)]
struct EndpointLock {
    _file: File,
}

impl EndpointLock {
    fn acquire(endpoint: &Path, owner_uid: u32) -> io::Result<Self> {
        let path = endpoint_lock_path(endpoint);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)?;
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("local socket lock is not a regular file: {}", path.display()),
            ));
        }
        if metadata.uid() != owner_uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "local socket lock {} is owned by uid {}, expected {owner_uid}",
                    path.display(),
                    metadata.uid()
                ),
            ));
        }
        let identity = FileIdentity::from_metadata(&metadata);
        let path_metadata = fs::symlink_metadata(&path)?;
        if !path_metadata.file_type().is_file()
            || FileIdentity::from_metadata(&path_metadata) != identity
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("local socket lock path changed: {}", path.display()),
            ));
        }

        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let error = io::Error::last_os_error();
            let raw_error = error.raw_os_error();
            if raw_error == Some(libc::EWOULDBLOCK) || raw_error == Some(libc::EAGAIN) {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("local socket lock is already held: {}", path.display()),
                ));
            }
            return Err(error);
        }

        file.set_permissions(Permissions::from_mode(0o600))?;
        let secured_metadata = file.metadata()?;
        let secured_path_metadata = fs::symlink_metadata(&path)?;
        let secured_identity = FileIdentity::from_metadata(&secured_metadata);
        if !secured_metadata.file_type().is_file()
            || secured_metadata.uid() != owner_uid
            || secured_metadata.permissions().mode() & 0o7777 != 0o600
            || secured_identity != identity
            || !secured_path_metadata.file_type().is_file()
            || FileIdentity::from_metadata(&secured_path_metadata) != identity
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("local socket lock is not securely fixed: {}", path.display()),
            ));
        }

        Ok(Self { _file: file })
    }
}

fn current_euid() -> u32 {
    unsafe { libc::geteuid() }
}

// UID 0 is the Unix analogue of the LocalSystem identity trusted by the
// Windows transport. Peer credentials are checked before any application
// bytes are read or written.
fn trusted_peer_uid(peer_uid: u32, owner_uid: u32) -> bool {
    peer_uid == owner_uid || peer_uid == 0
}

fn validated_endpoint(endpoint: &Path, owner_uid: u32) -> io::Result<PathBuf> {
    if !endpoint.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("local socket endpoint must be absolute: {}", endpoint.display()),
        ));
    }
    let file_name = endpoint.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "local socket endpoint must end in a filename: {}",
                endpoint.display()
            ),
        )
    })?;
    let parent = endpoint.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "local socket endpoint has no parent directory: {}",
                endpoint.display()
            ),
        )
    })?;
    let parent = fs::canonicalize(parent)?;
    let metadata = fs::symlink_metadata(&parent)?;
    if !metadata.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("local socket parent is not a directory: {}", parent.display()),
        ));
    }
    if metadata.uid() != owner_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "local socket parent {} is owned by uid {}, expected {owner_uid}",
                parent.display(),
                metadata.uid()
            ),
        ));
    }
    let mode = metadata.permissions().mode() & 0o7777;
    if mode != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "local socket parent {} has mode {mode:04o}, expected 0700",
                parent.display()
            ),
        ));
    }

    Ok(parent.join(file_name))
}

fn endpoint_lock_path(endpoint: &Path) -> PathBuf {
    let mut lock_name = OsString::from(".");
    lock_name.push(
        endpoint
            .file_name()
            .expect("validated endpoint has a filename"),
    );
    lock_name.push(".gate4agent.lock");
    endpoint.with_file_name(lock_name)
}

fn validate_listener(listener: &UnixListener) -> io::Result<()> {
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(listener.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT != libc::S_IFSOCK {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "bound listener file descriptor is not a Unix socket",
        ));
    }
    Ok(())
}

async fn remove_stale_socket(
    endpoint: &Path,
    owner_uid: u32,
    _lock: &EndpointLock,
) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(endpoint) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "refusing to replace non-socket endpoint {}",
                endpoint.display()
            ),
        ));
    }
    if metadata.uid() != owner_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing to replace socket {} owned by uid {}",
                endpoint.display(),
                metadata.uid()
            ),
        ));
    }
    let original_identity = FileIdentity::from_metadata(&metadata);

    match timeout(STALE_SOCKET_PROBE_TIMEOUT, UnixStream::connect(endpoint)).await {
        Ok(Ok(stream)) => {
            drop(stream);
            return Err(live_socket_collision(endpoint));
        }
        Err(_) => return Err(live_socket_collision(endpoint)),
        Ok(Err(error)) if error.kind() == io::ErrorKind::ConnectionRefused => {}
        Ok(Err(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Ok(Err(error)) => return Err(error),
    }

    let current_metadata = match fs::symlink_metadata(endpoint) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let unchanged_owned_socket = current_metadata.file_type().is_socket()
        && current_metadata.uid() == owner_uid
        && FileIdentity::from_metadata(&current_metadata) == original_identity;
    if !unchanged_owned_socket {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "local socket endpoint changed during stale probe: {}",
                endpoint.display()
            ),
        ));
    }

    fs::remove_file(endpoint)
}

fn owned_socket_metadata(endpoint: &Path, owner_uid: u32) -> io::Result<Metadata> {
    let metadata = fs::symlink_metadata(endpoint)?;
    if !metadata.file_type().is_socket() || metadata.uid() != owner_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "bound endpoint is not an owned socket: {}",
                endpoint.display()
            ),
        ));
    }
    Ok(metadata)
}

fn remove_socket_if_same(endpoint: &Path, identity: FileIdentity, _lock: &EndpointLock) {
    let Ok(metadata) = fs::symlink_metadata(endpoint) else {
        return;
    };
    if metadata.file_type().is_socket()
        && FileIdentity::from_metadata(&metadata) == identity
    {
        let _ = fs::remove_file(endpoint);
    }
}

fn retryable_connect_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

fn live_socket_collision(endpoint: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::AddrInUse,
        format!("live local socket already exists at {}", endpoint.display()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new() -> Self {
            let base = fs::canonicalize(std::env::temp_dir()).expect("canonical temp directory");
            for _ in 0..100 {
                let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let path = base.join(format!("g{:x}-{sequence:x}", std::process::id()));
                match fs::create_dir(&path) {
                    Ok(()) => {
                        fs::set_permissions(&path, Permissions::from_mode(0o700))
                            .expect("secure test directory");
                        return Self { path };
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("create test directory: {error}"),
                }
            }
            panic!("allocate unique short test directory")
        }

        fn endpoint(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            if let Ok(entries) = fs::read_dir(&self.path) {
                for entry in entries.flatten() {
                    match entry.file_type() {
                        Ok(file_type) if file_type.is_dir() => {
                            let _ = fs::remove_dir(entry.path());
                        }
                        _ => {
                            let _ = fs::remove_file(entry.path());
                        }
                    }
                }
            }
            let _ = fs::remove_dir(&self.path);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn owner_only_socket_has_secure_modes_and_roundtrips() {
        let directory = TestDirectory::new();
        let endpoint = directory.endpoint("n.sock");
        let lock_path = endpoint_lock_path(&endpoint);
        assert_eq!(
            lock_path.file_name(),
            Some(std::ffi::OsStr::new(".n.sock.gate4agent.lock"))
        );
        let mut listener = OwnerOnlyLocalListener::bind(&endpoint)
            .await
            .expect("bind owner-only socket");

        assert_eq!(
            fs::metadata(&directory.path)
                .expect("stat parent")
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );
        assert_eq!(
            fs::symlink_metadata(&endpoint)
                .expect("stat socket")
                .permissions()
                .mode()
                & 0o7777,
            0o600
        );
        let lock_metadata = fs::symlink_metadata(&lock_path).expect("stat endpoint lock");
        assert!(lock_metadata.file_type().is_file());
        assert_eq!(lock_metadata.uid(), current_euid());
        assert_eq!(lock_metadata.permissions().mode() & 0o7777, 0o600);

        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept owner client");
            let mut request = [0_u8; 4];
            stream.read_exact(&mut request).await.expect("read request");
            assert_eq!(&request, b"ping");
            stream.write_all(b"pong").await.expect("write response");
        });
        let mut client = connect_local_stream(&endpoint)
            .await
            .expect("connect owner client");
        client.write_all(b"ping").await.expect("write request");
        let mut response = [0_u8; 4];
        client.read_exact(&mut response).await.expect("read response");
        assert_eq!(&response, b"pong");
        server.await.expect("join server");
        assert!(lock_path.is_file(), "listener drop must retain lock file");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_accept_keeps_listener_usable() {
        let directory = TestDirectory::new();
        let endpoint = directory.endpoint("n.sock");
        let mut listener = OwnerOnlyLocalListener::bind(&endpoint)
            .await
            .expect("bind owner-only socket");

        timeout(Duration::from_millis(10), listener.accept())
            .await
            .expect_err("accept remains pending without a client");
        let client = connect_local_stream(&endpoint)
            .await
            .expect("connect after cancelled accept");
        let server = listener.accept().await.expect("accept after cancellation");
        drop(client);
        drop(server);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn second_listener_lock_collision_does_not_remove_live_socket() {
        let directory = TestDirectory::new();
        let endpoint = directory.endpoint("n.sock");
        let listener = OwnerOnlyLocalListener::bind(&endpoint)
            .await
            .expect("bind first listener");
        let identity = FileIdentity::from_metadata(
            &fs::symlink_metadata(&endpoint).expect("stat first socket"),
        );
        let lock_path = endpoint_lock_path(&endpoint);
        let lock_identity = FileIdentity::from_metadata(
            &fs::symlink_metadata(&lock_path).expect("stat held endpoint lock"),
        );

        let error = OwnerOnlyLocalListener::bind(&endpoint)
            .await
            .expect_err("reject live collision");
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert_eq!(
            FileIdentity::from_metadata(
                &fs::symlink_metadata(&endpoint).expect("live socket remains")
            ),
            identity
        );
        assert_eq!(
            FileIdentity::from_metadata(
                &fs::symlink_metadata(&lock_path).expect("held endpoint lock remains")
            ),
            lock_identity
        );
        drop(listener);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn endpoint_specific_locks_allow_two_listeners_in_one_parent() {
        let directory = TestDirectory::new();
        let node_endpoint = directory.endpoint("n.sock");
        let c2_endpoint = directory.endpoint("c.sock");
        assert_ne!(
            endpoint_lock_path(&node_endpoint),
            endpoint_lock_path(&c2_endpoint)
        );

        let node_listener = OwnerOnlyLocalListener::bind(&node_endpoint)
            .await
            .expect("bind node listener");
        let c2_listener = OwnerOnlyLocalListener::bind(&c2_endpoint)
            .await
            .expect("bind C2 listener in same parent");
        assert!(node_endpoint.exists());
        assert!(c2_endpoint.exists());
        drop(c2_listener);
        drop(node_listener);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn safely_recovers_owned_stale_socket() {
        let directory = TestDirectory::new();
        let endpoint = directory.endpoint("n.sock");
        let stale = std::os::unix::net::UnixListener::bind(&endpoint)
            .expect("bind stale socket fixture");
        drop(stale);
        let mut listener = OwnerOnlyLocalListener::bind(&endpoint)
            .await
            .expect("recover stale socket");
        let client = connect_local_stream(&endpoint)
            .await
            .expect("connect to recovered socket");
        let server = listener
            .accept()
            .await
            .expect("accept on recovered socket");
        drop(client);
        drop(server);
        drop(listener);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refuses_regular_file_and_symlink_endpoints() {
        let directory = TestDirectory::new();
        let regular = directory.endpoint("regular");
        File::create(&regular).expect("create regular endpoint fixture");
        let regular_error = OwnerOnlyLocalListener::bind(&regular)
            .await
            .expect_err("refuse regular file");
        assert_eq!(regular_error.kind(), io::ErrorKind::AlreadyExists);
        assert!(regular.is_file());

        let target = directory.endpoint("target");
        File::create(&target).expect("create symlink target");
        let link = directory.endpoint("link.sock");
        symlink(&target, &link).expect("create endpoint symlink");
        let symlink_error = OwnerOnlyLocalListener::bind(&link)
            .await
            .expect_err("refuse endpoint symlink");
        assert_eq!(symlink_error.kind(), io::ErrorKind::AlreadyExists);
        assert!(
            fs::symlink_metadata(&link)
                .expect("symlink remains")
                .file_type()
                .is_symlink()
        );
        assert!(target.is_file());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_relative_endpoint_and_insecure_parent_mode() {
        let relative_error = OwnerOnlyLocalListener::bind(Path::new("n.sock"))
            .await
            .expect_err("reject relative endpoint");
        assert_eq!(relative_error.kind(), io::ErrorKind::InvalidInput);

        let directory = TestDirectory::new();
        fs::set_permissions(&directory.path, Permissions::from_mode(0o750))
            .expect("make parent insecure");
        let endpoint = directory.endpoint("n.sock");
        let mode_error = OwnerOnlyLocalListener::bind(&endpoint)
            .await
            .expect_err("reject insecure parent mode");
        assert_eq!(mode_error.kind(), io::ErrorKind::PermissionDenied);
        assert!(!endpoint.exists());
        fs::set_permissions(&directory.path, Permissions::from_mode(0o700))
            .expect("restore parent mode for cleanup");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refuses_symlink_and_nonregular_endpoint_locks() {
        let directory = TestDirectory::new();

        let symlink_endpoint = directory.endpoint("s.sock");
        let symlink_lock = endpoint_lock_path(&symlink_endpoint);
        let target = directory.endpoint("lock-target");
        File::create(&target).expect("create lock symlink target");
        symlink(&target, &symlink_lock).expect("create malicious lock symlink");
        OwnerOnlyLocalListener::bind(&symlink_endpoint)
            .await
            .expect_err("refuse symlink lock");
        assert!(
            fs::symlink_metadata(&symlink_lock)
                .expect("malicious lock symlink remains")
                .file_type()
                .is_symlink()
        );
        assert!(!symlink_endpoint.exists());

        let directory_endpoint = directory.endpoint("d.sock");
        let directory_lock = endpoint_lock_path(&directory_endpoint);
        fs::create_dir(&directory_lock).expect("create nonregular lock fixture");
        OwnerOnlyLocalListener::bind(&directory_endpoint)
            .await
            .expect_err("refuse nonregular lock");
        assert!(directory_lock.is_dir());
        assert!(!directory_endpoint.exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drop_preserves_replacement_socket_inode() {
        let directory = TestDirectory::new();
        let endpoint = directory.endpoint("n.sock");
        let lock_path = endpoint_lock_path(&endpoint);
        let listener = OwnerOnlyLocalListener::bind(&endpoint)
            .await
            .expect("bind managed socket");
        fs::remove_file(&endpoint).expect("unlink managed socket fixture");
        let replacement = std::os::unix::net::UnixListener::bind(&endpoint)
            .expect("bind replacement socket");
        let replacement_identity = FileIdentity::from_metadata(
            &fs::symlink_metadata(&endpoint).expect("stat replacement socket"),
        );

        let collision = OwnerOnlyLocalListener::bind(&endpoint)
            .await
            .expect_err("held endpoint lock blocks bind after path replacement");
        assert_eq!(collision.kind(), io::ErrorKind::AddrInUse);
        assert_eq!(
            FileIdentity::from_metadata(
                &fs::symlink_metadata(&endpoint).expect("replacement survives collision")
            ),
            replacement_identity
        );

        drop(listener);
        assert_eq!(
            FileIdentity::from_metadata(
                &fs::symlink_metadata(&endpoint).expect("replacement socket remains")
            ),
            replacement_identity
        );
        assert!(lock_path.is_file(), "replacement race must retain lock file");
        drop(replacement);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drop_cleans_socket_and_allows_rebind() {
        let directory = TestDirectory::new();
        let endpoint = directory.endpoint("n.sock");
        let lock_path = endpoint_lock_path(&endpoint);
        let listener = OwnerOnlyLocalListener::bind(&endpoint)
            .await
            .expect("bind first socket");
        assert!(endpoint.exists());
        drop(listener);
        assert!(!endpoint.exists());
        let lock_identity = FileIdentity::from_metadata(
            &fs::symlink_metadata(&lock_path).expect("lock remains after cleanup"),
        );

        let rebound = OwnerOnlyLocalListener::bind(&endpoint)
            .await
            .expect("rebind after cleanup");
        assert!(endpoint.exists());
        drop(rebound);
        assert!(!endpoint.exists());
        assert_eq!(
            FileIdentity::from_metadata(
                &fs::symlink_metadata(&lock_path).expect("same lock remains after rebind")
            ),
            lock_identity
        );
    }
}
