use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Write},
    os::unix::{
        fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};

use reccursive_protocol::{
    ApiError, ApiErrorCode, AuthToken, Command, ProtocolValidationError, RequestEnvelope,
    RequestId, ResponseData, ResponseEnvelope, TransportError,
    transport::{read_message, write_message},
};
use reccursive_store::{Store, StoreError};
use thiserror::Error;
use uuid::Uuid;

/// Files owned by one daemon installation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServicePaths {
    pub state_dir: PathBuf,
    pub socket: PathBuf,
    pub lock: PathBuf,
    pub auth_token: PathBuf,
    pub database: PathBuf,
}

impl ServicePaths {
    #[must_use]
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        let state_dir = state_dir.into();
        Self {
            socket: state_dir.join("service.sock"),
            lock: state_dir.join("service.lock"),
            auth_token: state_dir.join("auth.token"),
            database: state_dir.join("state.sqlite"),
            state_dir,
        }
    }
}

/// Bound local API and exclusive owner of mutable service state.
pub struct LocalService {
    owner: ServiceOwner,
    store: Arc<Mutex<Store>>,
}

impl LocalService {
    /// Acquires exclusive ownership before opening the database or socket.
    pub fn bind(paths: ServicePaths) -> Result<Self, ServiceError> {
        let owner = ServiceOwner::acquire(paths)?;
        let store = Store::open(&owner.paths.database)?;
        Ok(Self {
            owner,
            store: Arc::new(Mutex::new(store)),
        })
    }

    /// Serves forever, creating one worker thread per accepted local connection.
    pub fn serve_forever(self) -> Result<(), ServiceError> {
        for connection in self.owner.listener.incoming() {
            let stream = connection?;
            let auth_token = self.owner.auth_token.clone();
            let store = Arc::clone(&self.store);
            thread::spawn(move || {
                let _ = handle_connection(stream, &auth_token, &store);
            });
        }
        Ok(())
    }

    #[cfg(test)]
    fn serve_connections(self, count: usize) -> Result<(), ServiceError> {
        let mut workers = Vec::with_capacity(count);
        for _ in 0..count {
            let (stream, _) = self.owner.listener.accept()?;
            let auth_token = self.owner.auth_token.clone();
            let store = Arc::clone(&self.store);
            workers.push(thread::spawn(move || {
                handle_connection(stream, &auth_token, &store)
            }));
        }
        for worker in workers {
            worker.join().map_err(|_| ServiceError::WorkerPanicked)??;
        }
        Ok(())
    }
}

struct ServiceOwner {
    paths: ServicePaths,
    _lock_file: File,
    listener: UnixListener,
    auth_token: AuthToken,
}

impl ServiceOwner {
    fn acquire(paths: ServicePaths) -> Result<Self, ServiceError> {
        prepare_state_directory(&paths.state_dir)?;
        ensure_regular_file_or_missing(&paths.lock)?;
        ensure_regular_file_or_missing(&paths.auth_token)?;
        ensure_regular_file_or_missing(&paths.database)?;
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&paths.lock)?;
        fs::set_permissions(&paths.lock, fs::Permissions::from_mode(0o600))?;
        match lock_file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(ServiceError::AlreadyRunning { path: paths.lock });
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(ServiceError::Io(error)),
        }

        let auth_token = load_or_create_auth_token(&paths.auth_token)?;
        remove_stale_socket(&paths.socket)?;
        let listener = UnixListener::bind(&paths.socket)?;
        fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            paths,
            _lock_file: lock_file,
            listener,
            auth_token,
        })
    }
}

impl Drop for ServiceOwner {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.paths.socket);
    }
}

fn prepare_state_directory(path: &Path) -> Result<(), ServiceError> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() {
            return Err(ServiceError::InsecureStateDirectory {
                path: path.to_path_buf(),
            });
        }
        if !metadata.is_dir() {
            return Err(ServiceError::InsecureStateDirectory {
                path: path.to_path_buf(),
            });
        }
    }
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn ensure_regular_file_or_missing(path: &Path) -> Result<(), ServiceError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(ServiceError::InsecureStateFile {
            path: path.to_path_buf(),
        }),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ServiceError::Io(error)),
    }
}

fn load_or_create_auth_token(path: &Path) -> Result<AuthToken, ServiceError> {
    let value = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut file) => {
            let value = Uuid::new_v4().simple().to_string();
            file.write_all(value.as_bytes())?;
            file.sync_all()?;
            value
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => fs::read_to_string(path)?,
        Err(error) => return Err(ServiceError::Io(error)),
    };
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(AuthToken::new(value.trim().to_owned())?)
}

fn remove_stale_socket(path: &Path) -> Result<(), ServiceError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() || metadata.file_type().is_symlink() => {
            fs::remove_file(path)?;
            Ok(())
        }
        Ok(_) => Err(ServiceError::SocketPathOccupied {
            path: path.to_path_buf(),
        }),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ServiceError::Io(error)),
    }
}

fn handle_connection(
    mut stream: UnixStream,
    expected_auth_token: &AuthToken,
    store: &Mutex<Store>,
) -> Result<(), ServiceError> {
    let request = match read_message::<RequestEnvelope>(&mut stream) {
        Ok(request) => request,
        Err(error) => {
            let response = ResponseEnvelope::failure(
                RequestId::new(),
                ApiError::new(ApiErrorCode::InvalidRequest, error.to_string(), false),
            );
            write_message(&mut stream, &response)?;
            return Ok(());
        }
    };

    let response = if let Err(error) = request.validate() {
        ResponseEnvelope::failure(
            request.request_id,
            ApiError::new(ApiErrorCode::UnsupportedVersion, error.to_string(), false),
        )
    } else if request.auth_token != *expected_auth_token {
        ResponseEnvelope::failure(
            request.request_id,
            ApiError::new(
                ApiErrorCode::Unauthorized,
                "local API authentication failed",
                false,
            ),
        )
    } else {
        match request.command {
            Command::Ping => match store.lock() {
                Ok(store) => ResponseEnvelope::success(
                    request.request_id,
                    ResponseData::Pong {
                        service_version: env!("CARGO_PKG_VERSION").to_owned(),
                        schema_version: store.schema_version()?,
                    },
                ),
                Err(_) => ResponseEnvelope::failure(
                    request.request_id,
                    ApiError::new(
                        ApiErrorCode::Internal,
                        "service state lock is unavailable",
                        true,
                    ),
                ),
            },
        }
    };
    write_message(&mut stream, &response)?;
    Ok(())
}

/// Service startup, ownership, transport, and storage failures.
#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("another service owner holds {}", path.display())]
    AlreadyRunning { path: PathBuf },
    #[error("state path is not a secure directory: {}", path.display())]
    InsecureStateDirectory { path: PathBuf },
    #[error("service state file is a symlink or non-file: {}", path.display())]
    InsecureStateFile { path: PathBuf },
    #[error("local API socket path is occupied by a non-socket file: {}", path.display())]
    SocketPathOccupied { path: PathBuf },
    #[error("service I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Protocol(#[from] ProtocolValidationError),
    #[error("local API worker panicked")]
    WorkerPanicked,
}

#[cfg(test)]
mod tests {
    use super::*;
    use reccursive_protocol::{API_VERSION, LocalClient, ResponseData};
    use std::net::Shutdown;
    use tempfile::tempdir;

    #[test]
    fn only_one_service_can_own_a_state_directory() {
        let directory = tempdir().unwrap();
        let paths = ServicePaths::new(directory.path());
        let first = LocalService::bind(paths.clone()).unwrap();
        assert!(matches!(
            LocalService::bind(paths.clone()),
            Err(ServiceError::AlreadyRunning { .. })
        ));
        drop(first);
        LocalService::bind(paths).unwrap();
    }

    #[test]
    fn state_directory_token_and_socket_are_owner_only() {
        let directory = tempdir().unwrap();
        let paths = ServicePaths::new(directory.path().join("state"));
        let _service = LocalService::bind(paths.clone()).unwrap();
        assert_eq!(
            fs::metadata(&paths.state_dir).unwrap().permissions().mode() & 0o077,
            0
        );
        assert_eq!(
            fs::metadata(&paths.auth_token)
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0
        );
        assert_eq!(
            fs::metadata(&paths.socket).unwrap().permissions().mode() & 0o077,
            0
        );
    }

    #[test]
    fn authenticated_ping_is_correlated_and_reports_store_schema() {
        let directory = tempdir().unwrap();
        let paths = ServicePaths::new(directory.path());
        let service = LocalService::bind(paths.clone()).unwrap();
        let worker = thread::spawn(move || service.serve_connections(1));
        let response = LocalClient::from_token_file(&paths.auth_token)
            .unwrap()
            .send(&paths.socket, Command::Ping)
            .unwrap();
        assert_eq!(response.api_version, API_VERSION);
        assert!(matches!(
            response.result,
            Ok(ResponseData::Pong {
                schema_version: reccursive_store::STORAGE_SCHEMA_VERSION,
                ..
            })
        ));
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn unauthorized_requests_receive_a_stable_error() {
        let directory = tempdir().unwrap();
        let paths = ServicePaths::new(directory.path());
        let service = LocalService::bind(paths.clone()).unwrap();
        let worker = thread::spawn(move || service.serve_connections(1));
        let response = LocalClient::new(AuthToken::new("b".repeat(32)).unwrap())
            .send(&paths.socket, Command::Ping)
            .unwrap();
        assert_eq!(
            response.result.unwrap_err().code,
            ApiErrorCode::Unauthorized
        );
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn unsupported_protocol_version_is_reported_with_the_request_id() {
        let directory = tempdir().unwrap();
        let paths = ServicePaths::new(directory.path());
        let service = LocalService::bind(paths.clone()).unwrap();
        let worker = thread::spawn(move || service.serve_connections(1));
        let token = AuthToken::new(fs::read_to_string(&paths.auth_token).unwrap()).unwrap();
        let mut request = RequestEnvelope::new(token, Command::Ping);
        request.api_version = API_VERSION + 1;
        let request_id = request.request_id;
        let mut stream = UnixStream::connect(&paths.socket).unwrap();
        write_message(&mut stream, &request).unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        let response: ResponseEnvelope = read_message(&mut stream).unwrap();
        assert_eq!(response.request_id, request_id);
        assert_eq!(
            response.result.unwrap_err().code,
            ApiErrorCode::UnsupportedVersion
        );
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn concurrent_clients_share_one_service_owner() {
        const CLIENTS: usize = 8;
        let directory = tempdir().unwrap();
        let paths = ServicePaths::new(directory.path());
        let service = LocalService::bind(paths.clone()).unwrap();
        let server = thread::spawn(move || service.serve_connections(CLIENTS));
        let client = LocalClient::from_token_file(&paths.auth_token).unwrap();
        let workers: Vec<_> = (0..CLIENTS)
            .map(|_| {
                let client = client.clone();
                let socket = paths.socket.clone();
                thread::spawn(move || client.send(socket, Command::Ping))
            })
            .collect();
        for worker in workers {
            assert!(worker.join().unwrap().unwrap().result.is_ok());
        }
        server.join().unwrap().unwrap();
    }
}
