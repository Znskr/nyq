//! Durable MCP OAuth credential store (profile file, optional macOS keychain).
//!
//! Ported 1:1 from `zig-reference/src/core/mcp/mcp_auth_store.zig`. The native
//! keychain backend is not in this batch; Linux/Windows always use the profile
//! file. Tests inject a fake keychain for migration coverage.

use super::mcp_auth::{self, Credentials};
use crate::core::mcp::json_value::{self, Value};
use crate::core::shared::{debug_trace, io, profile_paths};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

const SCHEMA_VERSION: i64 = 1;
const MAX_STORE_BYTES: usize = 1024 * 1024;
const LOCK_FILE_NAME: &str = "credentials.lock";
const LOCK_DEADLINE_MS: u64 = 2_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StorageBackend {
    ProfileFile,
    MacosKeychain,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadDecision {
    UseProfileFile,
    MigrateProfileFile,
    ReadKeychain,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Missing,
    Authenticated,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SaveResult {
    pub repaired_entries: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeleteResult {
    pub removed: usize,
    pub repaired_entries: usize,
    pub durable: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreError {
    HomeNotSet,
    InvalidMcpCredentialStore,
    McpCredentialStoreTooLarge,
    McpCredentialStoreWriteMismatch,
    DurablePathUnsafe,
    PrivateStatePermissionsUnsupported,
    FileNotFound,
    Cancelled,
    KeychainItemNotFound,
    KeychainReadFailed,
    KeychainWriteFailed,
    Io,
    LockBusy,
}

impl From<io::Error> for StoreError {
    fn from(err: io::Error) -> Self {
        match err {
            io::Error::Cancelled => Self::Cancelled,
            io::Error::DurablePathUnsafe => Self::DurablePathUnsafe,
            io::Error::PrivateStatePermissionsUnsupported => {
                Self::PrivateStatePermissionsUnsupported
            }
            io::Error::LockBusy => Self::LockBusy,
            io::Error::StreamTooLong => Self::InvalidMcpCredentialStore,
            _ => Self::Io,
        }
    }
}

struct StoredCredential {
    server_identity: String,
    credentials: Credentials,
}

struct Store {
    credentials: Vec<StoredCredential>,
    rejected_entries: usize,
}

impl Store {
    fn empty() -> Self {
        Self {
            credentials: Vec::new(),
            rejected_entries: 0,
        }
    }
}

struct LockedDir {
    dir: io::VerifiedDir,
    lock: io::TimedAdvisoryLock,
}

impl Drop for LockedDir {
    fn drop(&mut self) {
        // TimedAdvisoryLock releases on drop.
    }
}

fn select_storage_backend(macos: bool, keychain_disabled: bool, keychain_available: bool) -> StorageBackend {
    if macos && !keychain_disabled && keychain_available {
        StorageBackend::MacosKeychain
    } else {
        StorageBackend::ProfileFile
    }
}

fn storage_backend() -> Result<StorageBackend, StoreError> {
    let disabled = io::getenv(b"FX_DISABLE_KEYCHAIN").as_deref() == Some(b"1");
    Ok(select_storage_backend(cfg!(target_os = "macos"), disabled, false))
}

fn select_read_decision(backend: StorageBackend, profile_file_present: bool) -> ReadDecision {
    match backend {
        StorageBackend::ProfileFile => ReadDecision::UseProfileFile,
        StorageBackend::MacosKeychain => {
            if profile_file_present {
                ReadDecision::MigrateProfileFile
            } else {
                ReadDecision::ReadKeychain
            }
        }
    }
}

pub fn load(
    server_identity: &str,
    endpoint: &str,
    configured_resource: Option<&str>,
    configured_issuer: Option<&str>,
) -> Result<Option<Credentials>, StoreError> {
    load_controlled(
        server_identity,
        endpoint,
        configured_resource,
        configured_issuer,
        None,
    )
}

pub fn load_cancellable(
    server_identity: &str,
    endpoint: &str,
    configured_resource: Option<&str>,
    configured_issuer: Option<&str>,
    cancel_flag: &AtomicBool,
) -> Result<Option<Credentials>, StoreError> {
    load_controlled(
        server_identity,
        endpoint,
        configured_resource,
        configured_issuer,
        Some(cancel_flag),
    )
}

fn load_controlled(
    server_identity: &str,
    endpoint: &str,
    configured_resource: Option<&str>,
    configured_issuer: Option<&str>,
    cancel_flag: Option<&AtomicBool>,
) -> Result<Option<Credentials>, StoreError> {
    let backend = storage_backend()?;
    let locked = match open_locked_dir_for_read_controlled(backend, cancel_flag)? {
        Some(locked) => locked,
        None => return Ok(None),
    };
    let store = load_store_controlled(&locked.dir, backend, None, cancel_flag)?;
    let canonical_endpoint = mcp_auth::canonical_resource(endpoint)
        .map_err(|_| StoreError::InvalidMcpCredentialStore)?;
    let canonical_resource = match configured_resource {
        Some(value) => Some(
            mcp_auth::canonical_resource(value)
                .map_err(|_| StoreError::InvalidMcpCredentialStore)?,
        ),
        None => None,
    };
    let mut matched: Option<&StoredCredential> = None;
    for entry in &store.credentials {
        if entry.server_identity != server_identity
            || entry.credentials.endpoint != canonical_endpoint
        {
            continue;
        }
        if let Some(resource) = &canonical_resource {
            if &entry.credentials.resource != resource {
                continue;
            }
        }
        if let Some(issuer) = configured_issuer {
            if entry.credentials.issuer != issuer {
                continue;
            }
        }
        if matched.is_some() {
            return Ok(None);
        }
        matched = Some(entry);
    }
    Ok(matched.map(|entry| entry.credentials.clone()))
}

pub fn status(
    server_identity: &str,
    endpoint: &str,
    configured_resource: Option<&str>,
    configured_issuer: Option<&str>,
) -> Result<Status, StoreError> {
    Ok(
        match load(
            server_identity,
            endpoint,
            configured_resource,
            configured_issuer,
        )? {
            Some(_) => Status::Authenticated,
            None => Status::Missing,
        },
    )
}

pub fn save(server_identity: &str, credentials: &Credentials) -> Result<SaveResult, StoreError> {
    let backend = storage_backend()?;
    let locked = open_or_create_locked_dir()?;
    let mut store = load_store(&locked.dir, backend, None)?;
    let replacement = StoredCredential {
        server_identity: server_identity.to_string(),
        credentials: credentials.clone(),
    };
    if let Some(entry) = store
        .credentials
        .iter_mut()
        .find(|entry| same_identity(entry, server_identity, credentials))
    {
        *entry = replacement;
    } else {
        store.credentials.push(replacement);
    }
    write_store(&locked.dir, &store, backend, None)?;
    Ok(SaveResult {
        repaired_entries: store.rejected_entries,
    })
}

pub fn delete(server_identity: &str, endpoint: &str) -> Result<DeleteResult, StoreError> {
    let backend = storage_backend()?;
    let locked = match open_locked_dir_for_read(backend)? {
        Some(locked) => locked,
        None => return Ok(DeleteResult::default()),
    };
    let mut store = load_store(&locked.dir, backend, None)?;
    let canonical_endpoint = mcp_auth::canonical_resource(endpoint)
        .map_err(|_| StoreError::InvalidMcpCredentialStore)?;
    let mut removed = 0usize;
    store.credentials.retain(|entry| {
        let drop_it = entry.server_identity == server_identity
            && entry.credentials.endpoint == canonical_endpoint;
        if drop_it {
            removed += 1;
        }
        !drop_it
    });
    if removed == 0 && store.rejected_entries == 0 {
        return Ok(DeleteResult::default());
    }
    if store.credentials.is_empty() {
        clear_store(&locked.dir, backend, None)?;
    } else {
        write_store(&locked.dir, &store, backend, None)?;
    }
    Ok(DeleteResult {
        removed,
        repaired_entries: store.rejected_entries,
        durable: true,
    })
}

fn same_identity(entry: &StoredCredential, server_identity: &str, credentials: &Credentials) -> bool {
    entry.server_identity == server_identity
        && entry.credentials.endpoint == credentials.endpoint
        && entry.credentials.resource == credentials.resource
        && entry.credentials.issuer == credentials.issuer
}

fn home_path() -> Result<PathBuf, StoreError> {
    let home = io::getenv(b"HOME").ok_or(StoreError::HomeNotSet)?;
    String::from_utf8(home)
        .map(PathBuf::from)
        .map_err(|_| StoreError::HomeNotSet)
}

fn open_or_create_locked_dir() -> Result<LockedDir, StoreError> {
    open_or_create_locked_dir_controlled(None)
}

fn open_or_create_locked_dir_controlled(
    cancel_flag: Option<&AtomicBool>,
) -> Result<LockedDir, StoreError> {
    let home = home_path()?;
    let root = io::open_or_create_verified_private_dir(&home, profile_paths::root_dir_name)?;
    let credentials_dir = io::open_or_create_verified_private_dir(
        &root.path,
        profile_paths::mcp_credentials_dir_name,
    )?;
    let lock = io::acquire_timed_advisory_lock_cancellable(
        &credentials_dir,
        LOCK_FILE_NAME,
        LOCK_DEADLINE_MS,
        cancel_flag,
    )?;
    Ok(LockedDir {
        dir: credentials_dir,
        lock,
    })
}

fn open_existing_locked_dir() -> Result<Option<LockedDir>, StoreError> {
    open_existing_locked_dir_controlled(None)
}

fn open_existing_locked_dir_controlled(
    cancel_flag: Option<&AtomicBool>,
) -> Result<Option<LockedDir>, StoreError> {
    let home = home_path()?;
    let root = match open_existing_private_child(&home, profile_paths::root_dir_name) {
        Ok(dir) => dir,
        Err(StoreError::FileNotFound) => return Ok(None),
        Err(err) => return Err(err),
    };
    let credentials_dir =
        match open_existing_private_child(&root.path, profile_paths::mcp_credentials_dir_name) {
            Ok(dir) => dir,
            Err(StoreError::FileNotFound) => return Ok(None),
            Err(err) => return Err(err),
        };
    let lock = io::acquire_timed_advisory_lock_cancellable(
        &credentials_dir,
        LOCK_FILE_NAME,
        LOCK_DEADLINE_MS,
        cancel_flag,
    )?;
    Ok(Some(LockedDir {
        dir: credentials_dir,
        lock,
    }))
}

fn open_locked_dir_for_read(backend: StorageBackend) -> Result<Option<LockedDir>, StoreError> {
    open_locked_dir_for_read_controlled(backend, None)
}

fn open_locked_dir_for_read_controlled(
    backend: StorageBackend,
    cancel_flag: Option<&AtomicBool>,
) -> Result<Option<LockedDir>, StoreError> {
    match backend {
        StorageBackend::ProfileFile => open_existing_locked_dir_controlled(cancel_flag),
        StorageBackend::MacosKeychain => {
            open_or_create_locked_dir_controlled(cancel_flag).map(Some)
        }
    }
}

fn open_existing_private_child(parent: &Path, name: &str) -> Result<io::VerifiedDir, StoreError> {
    let path = parent.join(name);
    match fs::symlink_metadata(&path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(StoreError::FileNotFound)
        }
        Err(_) => return Err(StoreError::Io),
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            return Err(StoreError::DurablePathUnsafe)
        }
        Ok(_) => {}
    }
    normalize_and_verify_private_dir(&path)?;
    Ok(io::VerifiedDir {
        dir: File::open(&path).map_err(|_| StoreError::Io)?,
        path,
    })
}

fn normalize_and_verify_private_dir(path: &Path) -> Result<(), StoreError> {
    let initial = fs::metadata(path).map_err(|_| StoreError::Io)?;
    if !initial.is_dir() {
        return Err(StoreError::DurablePathUnsafe);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if initial.permissions().mode() & 0o200 == 0 {
            return Err(StoreError::PrivateStatePermissionsUnsupported);
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| StoreError::PrivateStatePermissionsUnsupported)?;
        let stat = fs::metadata(path).map_err(|_| StoreError::Io)?;
        if !stat.is_dir() || stat.permissions().mode() & 0o777 != 0o700 {
            return Err(StoreError::PrivateStatePermissionsUnsupported);
        }
    }
    Ok(())
}

trait Keychain {
    fn load(
        &mut self,
        cancel_flag: Option<&AtomicBool>,
    ) -> Result<Option<Vec<u8>>, StoreError>;
    fn store(&mut self, value: &[u8], cancel_flag: Option<&AtomicBool>) -> Result<(), StoreError>;
    fn delete(&mut self) -> Result<bool, StoreError>;
}

struct NativeKeychain;

impl Keychain for NativeKeychain {
    fn load(&mut self, _: Option<&AtomicBool>) -> Result<Option<Vec<u8>>, StoreError> {
        Err(StoreError::KeychainItemNotFound)
    }
    fn store(&mut self, _: &[u8], _: Option<&AtomicBool>) -> Result<(), StoreError> {
        Err(StoreError::KeychainWriteFailed)
    }
    fn delete(&mut self) -> Result<bool, StoreError> {
        Ok(false)
    }
}

fn load_store(
    dir: &io::VerifiedDir,
    backend: StorageBackend,
    keychain: Option<&mut dyn Keychain>,
) -> Result<Store, StoreError> {
    load_store_controlled(dir, backend, keychain, None)
}

fn load_store_controlled(
    dir: &io::VerifiedDir,
    backend: StorageBackend,
    keychain: Option<&mut dyn Keychain>,
    cancel_flag: Option<&AtomicBool>,
) -> Result<Store, StoreError> {
    check_cancellation(cancel_flag)?;
    let from_file = load_from_dir(dir)?;
    match select_read_decision(backend, from_file.is_some()) {
        ReadDecision::UseProfileFile => Ok(from_file.unwrap_or_else(Store::empty)),
        ReadDecision::MigrateProfileFile => {
            let store = from_file.unwrap();
            write_store_to_keychain(&store, keychain, cancel_flag)?;
            if store.rejected_entries > 0 {
                debug_trace::logf(
                    "mcp",
                    format_args!(
                        "removed unreadable MCP credential entries actor=migration count={}",
                        store.rejected_entries
                    ),
                );
            }
            check_cancellation(cancel_flag)?;
            delete_credential_file(dir)?;
            Ok(store)
        }
        ReadDecision::ReadKeychain => {
            check_cancellation(cancel_flag)?;
            let maybe_bytes = match keychain {
                Some(chain) => match chain.load(cancel_flag) {
                    Ok(bytes) => bytes,
                    Err(StoreError::KeychainItemNotFound) => None,
                    Err(err) => return Err(err),
                },
                None => None,
            };
            let bytes = match maybe_bytes {
                Some(bytes) => bytes,
                None => return Ok(Store::empty()),
            };
            if bytes.len() > MAX_STORE_BYTES {
                return Err(StoreError::InvalidMcpCredentialStore);
            }
            parse_store(&bytes)
        }
    }
}

fn load_from_dir(dir: &io::VerifiedDir) -> Result<Option<Store>, StoreError> {
    let path = dir.path.join(profile_paths::mcp_credentials_file_name);
    let mut file = match OpenOptions::new().read(true).open(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(StoreError::Io),
    };
    let meta = file.metadata().map_err(|_| StoreError::Io)?;
    if !meta.is_file() {
        return Err(StoreError::DurablePathUnsafe);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if meta.nlink() != 1 {
            return Err(StoreError::DurablePathUnsafe);
        }
        if meta.permissions().mode() & 0o777 != 0o600 {
            return Err(StoreError::PrivateStatePermissionsUnsupported);
        }
    }
    let bytes = io::read_file_to_end(&mut file, MAX_STORE_BYTES)?;
    parse_store(&bytes).map(Some)
}

fn json_integer(value: &Value) -> Option<i64> {
    match value {
        Value::Integer(n) => Some(*n),
        Value::NumberString(lexeme) => {
            if lexeme.contains('.') || lexeme.contains('e') || lexeme.contains('E') {
                None
            } else {
                lexeme.parse().ok()
            }
        }
        _ => None,
    }
}

fn parse_store(bytes: &[u8]) -> Result<Store, StoreError> {
    let text = std::str::from_utf8(bytes).map_err(|_| StoreError::InvalidMcpCredentialStore)?;
    let parsed = json_value::parse_json(text).map_err(|_| StoreError::InvalidMcpCredentialStore)?;
    let object = parsed
        .as_object()
        .ok_or(StoreError::InvalidMcpCredentialStore)?;
    let version = object
        .get("version")
        .ok_or(StoreError::InvalidMcpCredentialStore)?;
    let version = json_integer(version).ok_or(StoreError::InvalidMcpCredentialStore)?;
    if version != SCHEMA_VERSION {
        return Err(StoreError::InvalidMcpCredentialStore);
    }
    let values = object
        .get("credentials")
        .ok_or(StoreError::InvalidMcpCredentialStore)?;
    let values = values
        .as_array()
        .ok_or(StoreError::InvalidMcpCredentialStore)?;
    let mut store = Store::empty();
    for value in values {
        let object = match value.as_object() {
            Some(object) => object,
            None => {
                store.rejected_entries += 1;
                continue;
            }
        };
        let server_identity = match dupe_required_string(object, "server_identity") {
            Ok(value) => value,
            Err(StoreError::InvalidMcpCredentialStore) => {
                store.rejected_entries += 1;
                continue;
            }
            Err(err) => return Err(err),
        };
        let credentials = match parse_credentials(object) {
            Ok(value) => value,
            Err(StoreError::InvalidMcpCredentialStore) => {
                store.rejected_entries += 1;
                continue;
            }
            Err(err) => return Err(err),
        };
        store.credentials.push(StoredCredential {
            server_identity,
            credentials,
        });
    }
    Ok(store)
}

fn parse_credentials(object: &json_value::JsonMap) -> Result<Credentials, StoreError> {
    let endpoint = dupe_required_string(object, "endpoint")?;
    let resource = dupe_required_string(object, "resource")?;
    let issuer = dupe_required_string(object, "issuer")?;
    let client_id = dupe_required_string(object, "client_id")?;
    let client_secret = dupe_optional_string(object, "client_secret")?;
    let access_token = dupe_required_string(object, "access_token")?;
    let refresh_token = dupe_optional_string(object, "refresh_token")?;
    let scope = dupe_string_allow_empty(object, "scope")?;
    let token_type = dupe_required_string(object, "token_type")?;
    let token_endpoint_auth_method = dupe_required_string(object, "token_endpoint_auth_method")?;
    let authorization_endpoint = dupe_required_string(object, "authorization_endpoint")?;
    let token_endpoint = dupe_required_string(object, "token_endpoint")?;
    let revocation_endpoint = dupe_optional_string(object, "revocation_endpoint")?;
    let expires_at_value = object
        .get("expires_at_ms")
        .ok_or(StoreError::InvalidMcpCredentialStore)?;
    let expires_at_ms = match expires_at_value {
        Value::Null => i64::MAX,
        other => json_integer(other).ok_or(StoreError::InvalidMcpCredentialStore)?,
    };
    Ok(Credentials {
        endpoint,
        resource,
        issuer,
        client_id,
        client_secret,
        access_token,
        refresh_token,
        scope,
        token_type,
        token_endpoint_auth_method,
        expires_at_ms,
        authorization_endpoint,
        token_endpoint,
        revocation_endpoint,
    })
}

fn write_store(
    dir: &io::VerifiedDir,
    store: &Store,
    backend: StorageBackend,
    keychain: Option<&mut dyn Keychain>,
) -> Result<(), StoreError> {
    match backend {
        StorageBackend::ProfileFile => write_store_to_file(dir, store),
        StorageBackend::MacosKeychain => write_store_to_keychain(store, keychain, None),
    }
}

fn write_store_to_file(dir: &io::VerifiedDir, store: &Store) -> Result<(), StoreError> {
    let bytes = serialize_store(store)?;
    io::durable_replace_verified(dir, profile_paths::mcp_credentials_file_name, &bytes)?;
    Ok(())
}

fn write_store_to_keychain(
    store: &Store,
    keychain: Option<&mut dyn Keychain>,
    cancel_flag: Option<&AtomicBool>,
) -> Result<(), StoreError> {
    let keychain = keychain.ok_or(StoreError::KeychainWriteFailed)?;
    let bytes = serialize_store(store)?;
    check_cancellation(cancel_flag)?;
    keychain.store(&bytes, cancel_flag)?;
    check_cancellation(cancel_flag)?;
    let persisted = match keychain.load(cancel_flag) {
        Ok(Some(value)) => value,
        Ok(None) | Err(StoreError::KeychainItemNotFound) => {
            return Err(StoreError::McpCredentialStoreWriteMismatch)
        }
        Err(err) => return Err(err),
    };
    if bytes != persisted {
        return Err(StoreError::McpCredentialStoreWriteMismatch);
    }
    Ok(())
}

fn check_cancellation(cancel_flag: Option<&AtomicBool>) -> Result<(), StoreError> {
    if cancel_flag.is_some_and(|flag| flag.load(Ordering::Acquire)) {
        Err(StoreError::Cancelled)
    } else {
        Ok(())
    }
}

fn serialize_store(store: &Store) -> Result<Vec<u8>, StoreError> {
    let mut out = b"{\"version\":1,\"credentials\":[".to_vec();
    for (index, entry) in store.credentials.iter().enumerate() {
        if index > 0 {
            out.push(b',');
        }
        write_entry(&mut out, entry);
    }
    out.extend_from_slice(b"]}");
    if out.len() > MAX_STORE_BYTES {
        return Err(StoreError::McpCredentialStoreTooLarge);
    }
    Ok(out)
}

fn clear_store(
    dir: &io::VerifiedDir,
    backend: StorageBackend,
    mut keychain: Option<&mut dyn Keychain>,
) -> Result<(), StoreError> {
    match backend {
        StorageBackend::ProfileFile => delete_credential_file(dir),
        StorageBackend::MacosKeychain => {
            if let Some(chain) = keychain.as_mut() {
                let _ = chain.delete()?;
            }
            delete_credential_file(dir)
        }
    }
}

fn delete_credential_file(dir: &io::VerifiedDir) -> Result<(), StoreError> {
    let path = dir.path.join(profile_paths::mcp_credentials_file_name);
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(StoreError::Io),
    }
    io::sync_verified_dir(dir)?;
    Ok(())
}

fn write_entry(out: &mut Vec<u8>, entry: &StoredCredential) {
    let credentials = &entry.credentials;
    out.extend_from_slice(b"{\"server_identity\":");
    write_json_string(out, &entry.server_identity);
    for (key, value) in [
        ("endpoint", credentials.endpoint.as_str()),
        ("resource", credentials.resource.as_str()),
        ("issuer", credentials.issuer.as_str()),
        ("client_id", credentials.client_id.as_str()),
    ] {
        write!(out, ",\"{key}\":").ok();
        write_json_string(out, value);
    }
    out.extend_from_slice(b",\"client_secret\":");
    write_optional_string(out, credentials.client_secret.as_deref());
    out.extend_from_slice(b",\"access_token\":");
    write_json_string(out, &credentials.access_token);
    out.extend_from_slice(b",\"refresh_token\":");
    write_optional_string(out, credentials.refresh_token.as_deref());
    for (key, value) in [
        ("scope", credentials.scope.as_str()),
        ("token_type", credentials.token_type.as_str()),
        (
            "token_endpoint_auth_method",
            credentials.token_endpoint_auth_method.as_str(),
        ),
    ] {
        write!(out, ",\"{key}\":").ok();
        write_json_string(out, value);
    }
    write!(out, ",\"expires_at_ms\":{}", credentials.expires_at_ms).ok();
    out.extend_from_slice(b",\"authorization_endpoint\":");
    write_json_string(out, &credentials.authorization_endpoint);
    out.extend_from_slice(b",\"token_endpoint\":");
    write_json_string(out, &credentials.token_endpoint);
    out.extend_from_slice(b",\"revocation_endpoint\":");
    write_optional_string(out, credentials.revocation_endpoint.as_deref());
    out.push(b'}');
}

fn write_optional_string(out: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(text) => write_json_string(out, text),
        None => out.extend_from_slice(b"null"),
    }
}

fn write_json_string(out: &mut Vec<u8>, value: &str) {
    out.push(b'"');
    for &byte in value.as_bytes() {
        match byte {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0..=0x1f => out.extend_from_slice(format!("\\u{byte:04x}").as_bytes()),
            _ => out.push(byte),
        }
    }
    out.push(b'"');
}

fn dupe_required_string(object: &json_value::JsonMap, key: &str) -> Result<String, StoreError> {
    let value = object
        .get(key)
        .ok_or(StoreError::InvalidMcpCredentialStore)?;
    let text = value
        .as_str()
        .ok_or(StoreError::InvalidMcpCredentialStore)?;
    if text.is_empty() {
        return Err(StoreError::InvalidMcpCredentialStore);
    }
    Ok(text.to_string())
}

fn dupe_string_allow_empty(object: &json_value::JsonMap, key: &str) -> Result<String, StoreError> {
    let value = object
        .get(key)
        .ok_or(StoreError::InvalidMcpCredentialStore)?;
    let text = value
        .as_str()
        .ok_or(StoreError::InvalidMcpCredentialStore)?;
    Ok(text.to_string())
}

fn dupe_optional_string(
    object: &json_value::JsonMap,
    key: &str,
) -> Result<Option<String>, StoreError> {
    let value = match object.get(key) {
        Some(value) => value,
        None => return Ok(None),
    };
    if value.is_null() {
        return Ok(None);
    }
    let text = value
        .as_str()
        .ok_or(StoreError::InvalidMcpCredentialStore)?;
    if text.is_empty() {
        return Err(StoreError::InvalidMcpCredentialStore);
    }
    Ok(Some(text.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct TestHome<'a> {
        _guard: MutexGuard<'a, ()>,
        path: PathBuf,
    }

    impl<'a> TestHome<'a> {
        fn init(home: PathBuf) -> Self {
            let guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut map = HashMap::new();
            map.insert(
                b"HOME".to_vec(),
                home.to_str().expect("utf8 home").as_bytes().to_vec(),
            );
            map.insert(b"FX_DISABLE_KEYCHAIN".to_vec(), b"1".to_vec());
            io::set_environ_map(&map);
            Self {
                _guard: guard,
                path: home,
            }
        }
    }

    impl Drop for TestHome<'_> {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    struct FakeKeychain {
        value: Option<Vec<u8>>,
        store_calls: usize,
        delete_calls: usize,
        fail_store: bool,
        corrupt_store: bool,
        stall_load: bool,
        stall_store: bool,
        load_started: Arc<AtomicBool>,
        store_started: Arc<AtomicBool>,
    }

    impl FakeKeychain {
        fn new() -> Self {
            Self {
                value: None,
                store_calls: 0,
                delete_calls: 0,
                fail_store: false,
                corrupt_store: false,
                stall_load: false,
                stall_store: false,
                load_started: Arc::new(AtomicBool::new(false)),
                store_started: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    impl Keychain for FakeKeychain {
        fn load(
            &mut self,
            cancel_flag: Option<&AtomicBool>,
        ) -> Result<Option<Vec<u8>>, StoreError> {
            if self.stall_load {
                self.load_started.store(true, Ordering::Release);
                let flag = cancel_flag.ok_or(StoreError::KeychainReadFailed)?;
                while !flag.load(Ordering::Acquire) {
                    io::sleep(1_000_000);
                }
                return Err(StoreError::Cancelled);
            }
            Ok(self.value.clone())
        }

        fn store(
            &mut self,
            value: &[u8],
            cancel_flag: Option<&AtomicBool>,
        ) -> Result<(), StoreError> {
            if self.stall_store {
                self.store_started.store(true, Ordering::Release);
                let flag = cancel_flag.ok_or(StoreError::KeychainWriteFailed)?;
                while !flag.load(Ordering::Acquire) {
                    io::sleep(1_000_000);
                }
                return Err(StoreError::Cancelled);
            }
            self.store_calls += 1;
            if self.fail_store {
                return Err(StoreError::KeychainWriteFailed);
            }
            let replacement = if self.corrupt_store && !value.is_empty() {
                value[..value.len() - 1].to_vec()
            } else {
                value.to_vec()
            };
            self.value = Some(replacement);
            Ok(())
        }

        fn delete(&mut self) -> Result<bool, StoreError> {
            self.delete_calls += 1;
            if self.value.take().is_some() {
                Ok(true)
            } else {
                Ok(false)
            }
        }
    }

    fn test_credentials(endpoint: &str, access_token: &str) -> Credentials {
        Credentials {
            endpoint: endpoint.to_string(),
            resource: endpoint.to_string(),
            issuer: "https://issuer.example".into(),
            client_id: "client".into(),
            client_secret: None,
            access_token: access_token.to_string(),
            refresh_token: Some("refresh-secret".into()),
            scope: "tools.read".into(),
            token_type: "Bearer".into(),
            token_endpoint_auth_method: "none".into(),
            expires_at_ms: 123_456,
            authorization_endpoint: "https://issuer.example/authorize".into(),
            token_endpoint: "https://issuer.example/token".into(),
            revocation_endpoint: None,
        }
    }

    fn make_tmp_home() -> PathBuf {
        static UNIQUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "nyq-w26-mcp-creds-{}-{}-{}",
            std::process::id(),
            io::nano_timestamp(),
            UNIQUE.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn credential_store_parser_keeps_distinct_server_and_oauth_identities() {
        let json = br#"{"version":1,"credentials":[{"server_identity":"one","endpoint":"https://mcp.example/a","resource":"https://mcp.example/","issuer":"https://issuer.example","client_id":"client","client_secret":null,"access_token":"secret-one","refresh_token":"refresh-one","scope":"read","token_type":"Bearer","token_endpoint_auth_method":"none","expires_at_ms":123,"authorization_endpoint":"https://issuer.example/authorize","token_endpoint":"https://issuer.example/token","revocation_endpoint":null},{"server_identity":"two","endpoint":"https://mcp.example/a","resource":"https://mcp.example/","issuer":"https://issuer.example","client_id":"client","client_secret":null,"access_token":"secret-two","refresh_token":null,"scope":"read","token_type":"Bearer","token_endpoint_auth_method":"none","expires_at_ms":456,"authorization_endpoint":"https://issuer.example/authorize","token_endpoint":"https://issuer.example/token","revocation_endpoint":null}]}"#;
        let store = parse_store(json).unwrap();
        assert_eq!(2, store.credentials.len());
        assert_eq!(
            "secret-one",
            store.credentials[0].credentials.access_token
        );
        assert_eq!("two", store.credentials[1].server_identity);
    }

    #[test]
    fn credential_store_round_trip_accepts_empty_oauth_scope() {
        let mut credentials = test_credentials("https://mcp.example/no-scope", "empty-scope-secret");
        credentials.scope.clear();
        let source = Store {
            credentials: vec![StoredCredential {
                server_identity: "no-scope".into(),
                credentials,
            }],
            rejected_entries: 0,
        };
        let bytes = serialize_store(&source).unwrap();
        let decoded = parse_store(&bytes).unwrap();
        assert_eq!(1, decoded.credentials.len());
        assert_eq!("", decoded.credentials[0].credentials.scope);
    }

    #[test]
    fn credential_store_isolates_malformed_entries_from_valid_credentials() {
        let json = br#"{"version":1,"credentials":[{},{"server_identity":"valid","endpoint":"https://mcp.example/a","resource":"https://mcp.example/","issuer":"https://issuer.example","client_id":"client","client_secret":null,"access_token":"secret","refresh_token":null,"scope":"read","token_type":"Bearer","token_endpoint_auth_method":"none","expires_at_ms":456,"authorization_endpoint":"https://issuer.example/authorize","token_endpoint":"https://issuer.example/token","revocation_endpoint":null}]}"#;
        let store = parse_store(json).unwrap();
        assert_eq!(1, store.credentials.len());
        assert_eq!(1, store.rejected_entries);
        assert_eq!("valid", store.credentials[0].server_identity);
    }

    #[test]
    fn credential_store_skips_an_entry_with_missing_identity_fields() {
        let store = parse_store(br#"{"version":1,"credentials":[{}]}"#).unwrap();
        assert_eq!(0, store.credentials.len());
        assert_eq!(1, store.rejected_entries);
    }

    #[test]
    fn credential_store_keeps_top_level_schema_version_strict() {
        assert_eq!(
            Some(StoreError::InvalidMcpCredentialStore),
            parse_store(br#"{"version":2,"credentials":[]}"#).err()
        );
    }

    #[test]
    fn credential_store_loads_a_null_expiry_as_a_non_expiring_grant() {
        let json = br#"{"version":1,"credentials":[{"server_identity":"one","endpoint":"https://mcp.example/a","resource":"https://mcp.example/","issuer":"https://issuer.example","client_id":"client","client_secret":null,"access_token":"secret-one","refresh_token":"refresh-one","scope":"read","token_type":"Bearer","token_endpoint_auth_method":"none","expires_at_ms":null,"authorization_endpoint":"https://issuer.example/authorize","token_endpoint":"https://issuer.example/token","revocation_endpoint":null}]}"#;
        let store = parse_store(json).unwrap();
        assert_eq!(1, store.credentials.len());
        let credentials = &store.credentials[0].credentials;
        assert_eq!(i64::MAX, credentials.expires_at_ms);
        assert!(!credentials.needs_refresh(4_102_444_800_000));
    }

    #[test]
    fn credential_store_rejects_a_non_integer_expiry() {
        let json = br#"{"version":1,"credentials":[{"server_identity":"one","endpoint":"https://mcp.example/a","resource":"https://mcp.example/","issuer":"https://issuer.example","client_id":"client","client_secret":null,"access_token":"secret-one","refresh_token":"refresh-one","scope":"read","token_type":"Bearer","token_endpoint_auth_method":"none","expires_at_ms":"soon","authorization_endpoint":"https://issuer.example/authorize","token_endpoint":"https://issuer.example/token","revocation_endpoint":null}]}"#;
        let store = parse_store(json).unwrap();
        assert_eq!(0, store.credentials.len());
        assert_eq!(1, store.rejected_entries);
    }

    #[test]
    fn credential_store_rejects_a_missing_expiry() {
        let json = br#"{"version":1,"credentials":[{"server_identity":"one","endpoint":"https://mcp.example/a","resource":"https://mcp.example/","issuer":"https://issuer.example","client_id":"client","client_secret":null,"access_token":"secret-one","refresh_token":"refresh-one","scope":"read","token_type":"Bearer","token_endpoint_auth_method":"none","authorization_endpoint":"https://issuer.example/authorize","token_endpoint":"https://issuer.example/token","revocation_endpoint":null}]}"#;
        let store = parse_store(json).unwrap();
        assert_eq!(0, store.credentials.len());
        assert_eq!(1, store.rejected_entries);
    }

    #[test]
    fn credential_backend_selection_is_explicit_and_platform_scoped() {
        assert_eq!(
            StorageBackend::MacosKeychain,
            select_storage_backend(true, false, true)
        );
        assert_eq!(
            StorageBackend::ProfileFile,
            select_storage_backend(true, false, false)
        );
        assert_eq!(
            StorageBackend::ProfileFile,
            select_storage_backend(true, true, true)
        );
        assert_eq!(
            StorageBackend::ProfileFile,
            select_storage_backend(false, false, true)
        );
        assert_eq!(
            StorageBackend::ProfileFile,
            select_storage_backend(false, false, true)
        );
    }

    #[test]
    fn credential_read_decisions_preserve_the_portable_store_during_migration() {
        assert_eq!(
            ReadDecision::UseProfileFile,
            select_read_decision(StorageBackend::ProfileFile, false)
        );
        assert_eq!(
            ReadDecision::UseProfileFile,
            select_read_decision(StorageBackend::ProfileFile, true)
        );
        assert_eq!(
            ReadDecision::ReadKeychain,
            select_read_decision(StorageBackend::MacosKeychain, false)
        );
        assert_eq!(
            ReadDecision::MigrateProfileFile,
            select_read_decision(StorageBackend::MacosKeychain, true)
        );
    }

    #[test]
    fn keychain_migration_publishes_before_deleting_portable_credentials() {
        let home = make_tmp_home();
        let _test_home = TestHome::init(home);
        let credentials = test_credentials("https://mcp.example/service", "migration-secret");
        save("server-one", &credentials).unwrap();

        let mut fake = FakeKeychain::new();
        {
            let locked = open_existing_locked_dir().unwrap().unwrap();
            let migrated = load_store(
                &locked.dir,
                StorageBackend::MacosKeychain,
                Some(&mut fake),
            )
            .unwrap();
            assert_eq!(1, fake.store_calls);
            assert_eq!(
                "migration-secret",
                migrated.credentials[0].credentials.access_token
            );
            assert!(!locked
                .dir
                .path
                .join(profile_paths::mcp_credentials_file_name)
                .exists());
        }
        {
            let locked = open_existing_locked_dir().unwrap().unwrap();
            let loaded = load_store(
                &locked.dir,
                StorageBackend::MacosKeychain,
                Some(&mut fake),
            )
            .unwrap();
            assert_eq!(
                "migration-secret",
                loaded.credentials[0].credentials.access_token
            );
            clear_store(
                &locked.dir,
                StorageBackend::MacosKeychain,
                Some(&mut fake),
            )
            .unwrap();
            assert_eq!(1, fake.delete_calls);
            assert!(fake.value.is_none());
        }

        save("server-one", &credentials).unwrap();
        fake.fail_store = true;
        {
            let locked = open_existing_locked_dir().unwrap().unwrap();
            assert_eq!(
                Err(StoreError::KeychainWriteFailed),
                load_store(
                    &locked.dir,
                    StorageBackend::MacosKeychain,
                    Some(&mut fake),
                )
                .map(|_| ())
            );
            assert!(locked
                .dir
                .path
                .join(profile_paths::mcp_credentials_file_name)
                .exists());
        }

        fake.fail_store = false;
        fake.corrupt_store = true;
        {
            let locked = open_existing_locked_dir().unwrap().unwrap();
            assert_eq!(
                Err(StoreError::McpCredentialStoreWriteMismatch),
                load_store(
                    &locked.dir,
                    StorageBackend::MacosKeychain,
                    Some(&mut fake),
                )
                .map(|_| ())
            );
            assert!(locked
                .dir
                .path
                .join(profile_paths::mcp_credentials_file_name)
                .exists());
        }
    }

    #[test]
    fn keychain_migration_sanitizes_rejected_entries_after_verified_publication() {
        let home = make_tmp_home();
        let _test_home = TestHome::init(home);
        let json = br#"{"version":1,"credentials":[{},{"server_identity":"valid","endpoint":"https://mcp.example/a","resource":"https://mcp.example/","issuer":"https://issuer.example","client_id":"client","client_secret":null,"access_token":"secret","refresh_token":null,"scope":"","token_type":"Bearer","token_endpoint_auth_method":"none","expires_at_ms":456,"authorization_endpoint":"https://issuer.example/authorize","token_endpoint":"https://issuer.example/token","revocation_endpoint":null}]}"#;
        {
            let locked = open_or_create_locked_dir().unwrap();
            io::durable_replace_verified(
                &locked.dir,
                profile_paths::mcp_credentials_file_name,
                json,
            )
            .unwrap();
        }
        let mut fake = FakeKeychain::new();
        let locked = open_existing_locked_dir().unwrap().unwrap();
        let migrated = load_store(
            &locked.dir,
            StorageBackend::MacosKeychain,
            Some(&mut fake),
        )
        .unwrap();
        assert_eq!(1, migrated.rejected_entries);
        assert_eq!(1, fake.store_calls);
        assert!(!locked
            .dir
            .path
            .join(profile_paths::mcp_credentials_file_name)
            .exists());
        let published = parse_store(fake.value.as_ref().unwrap()).unwrap();
        assert_eq!(1, published.credentials.len());
        assert_eq!(0, published.rejected_entries);
    }

    #[test]
    fn credential_store_is_private_atomic_and_supports_restart_deletion() {
        let home = make_tmp_home();
        let _test_home = TestHome::init(home.clone());
        let credentials = test_credentials("https://mcp.example/service", "access-secret");
        save("server-one", &credentials).unwrap();
        let loaded = load("server-one", "https://mcp.example/service", None, None)
            .unwrap()
            .unwrap();
        assert_eq!("access-secret", loaded.access_token);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let root = home.join(".fx");
            let root_mode = fs::metadata(&root).unwrap().permissions().mode() & 0o777;
            assert_eq!(0o700, root_mode);
            let credentials_dir = root.join(profile_paths::mcp_credentials_dir_name);
            let dir_mode = fs::metadata(&credentials_dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(0o700, dir_mode);
            let file_mode = fs::metadata(
                credentials_dir.join(profile_paths::mcp_credentials_file_name),
            )
            .unwrap()
            .permissions()
            .mode()
                & 0o777;
            assert_eq!(0o600, file_mode);
        }

        let deleted = delete("server-one", "https://mcp.example/service").unwrap();
        assert_eq!(1, deleted.removed);
        assert!(load("server-one", "https://mcp.example/service", None, None)
            .unwrap()
            .is_none());
    }

    #[test]
    fn credential_delete_sanitizes_a_store_containing_only_rejected_entries() {
        let home = make_tmp_home();
        let _test_home = TestHome::init(home);
        {
            let locked = open_or_create_locked_dir().unwrap();
            io::durable_replace_verified(
                &locked.dir,
                profile_paths::mcp_credentials_file_name,
                br#"{"version":1,"credentials":[{}]}"#,
            )
            .unwrap();
        }
        let result = delete("fixture", "https://mcp.example/service").unwrap();
        assert_eq!(0, result.removed);
        assert_eq!(1, result.repaired_entries);
        let locked = open_existing_locked_dir().unwrap().unwrap();
        assert!(!locked
            .dir
            .path
            .join(profile_paths::mcp_credentials_file_name)
            .exists());
    }

    #[test]
    fn credential_save_sanitizes_rejected_entries_and_reports_the_repair_count() {
        let home = make_tmp_home();
        let _test_home = TestHome::init(home);
        {
            let locked = open_or_create_locked_dir().unwrap();
            io::durable_replace_verified(
                &locked.dir,
                profile_paths::mcp_credentials_file_name,
                br#"{"version":1,"credentials":[{}]}"#,
            )
            .unwrap();
        }
        let credentials = test_credentials("https://mcp.example/service", "replacement-secret");
        let result = save("fixture", &credentials).unwrap();
        assert_eq!(1, result.repaired_entries);
        let locked = open_existing_locked_dir().unwrap().unwrap();
        let store = load_store(&locked.dir, StorageBackend::ProfileFile, None).unwrap();
        assert_eq!(1, store.credentials.len());
        assert_eq!(0, store.rejected_entries);
        assert_eq!(
            "replacement-secret",
            store.credentials[0].credentials.access_token
        );
    }

    #[test]
    fn credential_load_cancellation_interrupts_a_held_advisory_lock() {
        let home = make_tmp_home();
        let _test_home = TestHome::init(home);
        let _held = open_or_create_locked_dir().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        let handle = thread::spawn({
            let cancel = Arc::clone(&cancel);
            let started = Arc::clone(&started);
            move || {
                started.store(true, Ordering::Release);
                load_cancellable(
                    "fixture",
                    "https://mcp.example/service",
                    None,
                    None,
                    &cancel,
                )
            }
        });
        while !started.load(Ordering::Acquire) {
            io::sleep(1_000_000);
        }
        io::sleep(25_000_000);
        let started_ms = io::milli_timestamp();
        cancel.store(true, Ordering::Release);
        let result = handle.join().unwrap();
        assert_eq!(Err(StoreError::Cancelled), result);
        assert!(io::milli_timestamp() - started_ms < 1_000);
    }

    #[test]
    fn credential_cancellation_interrupts_keychain_read_and_preserves_migration_source() {
        let home = make_tmp_home();
        let _test_home = TestHome::init(home);
        {
            let locked = open_or_create_locked_dir().unwrap();
            let mut fake = FakeKeychain::new();
            fake.stall_load = true;
            let cancel = Arc::new(AtomicBool::new(false));
            let started = Arc::clone(&fake.load_started);
            let handle = thread::spawn({
                let cancel = Arc::clone(&cancel);
                let started = Arc::clone(&started);
                move || {
                    while !started.load(Ordering::Acquire) {
                        io::sleep(1_000_000);
                    }
                    cancel.store(true, Ordering::Release);
                }
            });
            let started_ms = io::milli_timestamp();
            let result = load_store_controlled(
                &locked.dir,
                StorageBackend::MacosKeychain,
                Some(&mut fake),
                Some(cancel.as_ref()),
            );
            handle.join().unwrap();
            assert_eq!(Err(StoreError::Cancelled), result.map(|_| ()));
            assert!(io::milli_timestamp() - started_ms < 1_000);
        }

        let credentials = test_credentials("https://mcp.example/service", "migration-secret");
        save("fixture", &credentials).unwrap();
        {
            let locked = open_existing_locked_dir().unwrap().unwrap();
            let mut fake = FakeKeychain::new();
            fake.stall_store = true;
            let cancel = Arc::new(AtomicBool::new(false));
            let started = Arc::clone(&fake.store_started);
            let handle = thread::spawn({
                let cancel = Arc::clone(&cancel);
                let started = Arc::clone(&started);
                move || {
                    while !started.load(Ordering::Acquire) {
                        io::sleep(1_000_000);
                    }
                    cancel.store(true, Ordering::Release);
                }
            });
            let result = load_store_controlled(
                &locked.dir,
                StorageBackend::MacosKeychain,
                Some(&mut fake),
                Some(cancel.as_ref()),
            );
            handle.join().unwrap();
            assert_eq!(Err(StoreError::Cancelled), result.map(|_| ()));
            assert!(locked
                .dir
                .path
                .join(profile_paths::mcp_credentials_file_name)
                .exists());
        }
    }

    #[test]
    fn credential_load_refuses_an_ambiguous_discovered_issuer_identity() {
        let home = make_tmp_home();
        let _test_home = TestHome::init(home);
        let first = test_credentials("https://mcp.example/service", "access-one");
        save("server-one", &first).unwrap();
        let mut migrated = first.clone();
        migrated.issuer = "https://issuer-two.example".into();
        save("server-one", &migrated).unwrap();
        assert!(load("server-one", first.endpoint.as_str(), None, None)
            .unwrap()
            .is_none());
        let selected = load(
            "server-one",
            first.endpoint.as_str(),
            None,
            Some(migrated.issuer.as_str()),
        )
        .unwrap()
        .unwrap();
        assert_eq!("access-one", selected.access_token);
        assert_eq!(migrated.issuer, selected.issuer);
    }

    #[test]
    fn credential_store_serializes_concurrent_writers_without_dropping_identities() {
        let home = make_tmp_home();
        let _test_home = TestHome::init(home);
        let first = thread::spawn(|| {
            let credentials = test_credentials("https://mcp.example/one", "one-secret");
            save("one", &credentials)
        });
        let second = thread::spawn(|| {
            let credentials = test_credentials("https://mcp.example/two", "two-secret");
            save("two", &credentials)
        });
        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();
        let loaded_first = load("one", "https://mcp.example/one", None, None)
            .unwrap()
            .unwrap();
        let loaded_second = load("two", "https://mcp.example/two", None, None)
            .unwrap()
            .unwrap();
        assert_eq!("one-secret", loaded_first.access_token);
        assert_eq!("two-secret", loaded_second.access_token);
    }
}
