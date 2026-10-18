//! Read-only bridge to Codex's file-backed MCP OAuth access tokens.
//!
//! Codex alone owns authorization, refresh and persistence. We take the same
//! aggregate-store lock for each request and pick up a freshly rotated access
//! token without restarting the MCP connection.

use std::{
    borrow::Cow,
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::Read,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures::stream::BoxStream;
use http::{HeaderName, HeaderValue};
use rmcp::{
    model::ClientJsonRpcMessage,
    transport::streamable_http_client::{
        SseError, StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
    },
};
use serde::Deserialize;
use sse_stream::Sse;
use zeroize::Zeroize;

use crate::codex_config::codex_config_path;

#[derive(Clone)]
pub(crate) struct SharedOAuthClient {
    client: reqwest::Client,
    server_name: String,
    server_url: String,
}

impl SharedOAuthClient {
    pub(crate) fn new(client: reqwest::Client, server_name: &str, server_url: &str) -> Self {
        Self {
            client,
            server_name: server_name.to_owned(),
            server_url: server_url.to_owned(),
        }
    }

    async fn token(&self) -> Result<Option<String>, StreamableHttpError<reqwest::Error>> {
        let name = self.server_name.clone();
        let url = self.server_url.clone();
        tokio::task::spawn_blocking(move || read_access_token(&name, &url))
            .await
            .map_err(|_| auth_error())?
            .map_err(|_| auth_error())
    }
}

fn auth_error() -> StreamableHttpError<reqwest::Error> {
    StreamableHttpError::UnexpectedServerResponse(Cow::Borrowed(
        "shared Codex MCP OAuth credential unavailable or expired; authorize this source in Codex",
    ))
}

#[derive(Deserialize)]
struct Credential {
    server_name: String,
    server_url: String,
    access_token: String,
    expires_at: Option<u64>,
    #[serde(default)]
    executor_owned: bool,
}

impl Drop for Credential {
    fn drop(&mut self) {
        self.access_token.zeroize();
    }
}

fn codex_home() -> Result<PathBuf, &'static str> {
    codex_config_path()
        .map_err(|_| "Codex home unavailable")?
        .parent()
        .map(std::path::Path::to_path_buf)
        .ok_or("Codex home unavailable")
}

fn read_access_token(server_name: &str, server_url: &str) -> Result<Option<String>, &'static str> {
    let home = codex_home()?;
    read_access_token_in(&home, server_name, server_url)
}

fn read_access_token_in(
    home: &std::path::Path,
    server_name: &str,
    server_url: &str,
) -> Result<Option<String>, &'static str> {
    let lock_path = home.join("mcp-oauth-locks/file-store.lock");
    fs::create_dir_all(lock_path.parent().ok_or("Codex lock path unavailable")?)
        .map_err(|_| "Codex lock unavailable")?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .map_err(|_| "Codex lock unavailable")?;
    let started = Instant::now();
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock)
                if started.elapsed() < Duration::from_secs(2) =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return Err("Codex credential store is busy"),
        }
    }

    let path = home.join(".credentials.json");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("Codex credential store unavailable"),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("Codex credential store must be a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err("Codex credential store ownership or permissions are unsafe");
        }
    }
    let mut file = File::open(path).map_err(|_| "Codex credential store unavailable")?;
    if file
        .metadata()
        .map_err(|_| "Codex credential store unavailable")?
        .len()
        > 4 * 1024 * 1024
    {
        return Err("Codex credential store is too large");
    }
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .map_err(|_| "Codex credential store unavailable")?;
    let parsed: Result<HashMap<String, Credential>, _> = serde_json::from_str(&contents);
    contents.zeroize();
    let store = parsed.map_err(|_| "Codex credential store is invalid")?;
    let local_name = server_name.strip_prefix("local:").unwrap_or(server_name);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock unavailable")?
        .as_millis() as u64;
    let mut matched = None;
    for entry in store.values() {
        if !entry.executor_owned
            && entry.server_name == local_name
            && entry.server_url == server_url
        {
            if matched.is_some() {
                return Err("multiple matching Codex MCP OAuth credentials");
            }
            if entry
                .expires_at
                .is_some_and(|expiry| expiry <= now.saturating_add(30_000))
            {
                return Err("Codex MCP OAuth credential expired");
            }
            if entry.access_token.is_empty() {
                return Err("Codex MCP OAuth credential is empty");
            }
            matched = Some(entry.access_token.clone());
        }
    }
    Ok(matched)
}

impl StreamableHttpClient for SharedOAuthClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.client
            .post_message(
                uri,
                message,
                session_id,
                self.token().await?,
                custom_headers,
            )
            .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.client
            .post_message_with_max_sse_event_size(
                uri,
                message,
                session_id,
                self.token().await?,
                custom_headers,
                max_sse_event_size,
            )
            .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        self.client
            .delete_session(uri, session_id, self.token().await?, custom_headers)
            .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        self.client
            .get_stream(
                uri,
                session_id,
                last_event_id,
                self.token().await?,
                custom_headers,
            )
            .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        self.client
            .get_stream_with_max_sse_event_size(
                uri,
                session_id,
                last_event_id,
                self.token().await?,
                custom_headers,
                max_sse_event_size,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_credentials(home: &std::path::Path, token: &str, expiry: u64) {
        let path = home.join(".credentials.json");
        let content = serde_json::json!({"key": {
            "server_name": "remote", "server_url": "https://example.test/mcp",
            "access_token": token, "refresh_token": "must-never-be-used", "expires_at": expiry
        }});
        fs::write(&path, content.to_string()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn rereads_rotated_token_and_rejects_expired_or_wrong_source() {
        let home = tempfile::tempdir().unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        write_credentials(home.path(), "first", now + 120_000);
        let read = || read_access_token_in(home.path(), "remote", "https://example.test/mcp");
        assert_eq!(read().unwrap().as_deref(), Some("first"));
        assert_eq!(
            read_access_token_in(home.path(), "remote", "https://other.test/mcp").unwrap(),
            None
        );
        write_credentials(home.path(), "second", now + 120_000);
        assert_eq!(read().unwrap().as_deref(), Some("second"));
        write_credentials(home.path(), "expired", now - 1);
        assert!(read().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_world_readable_or_symlinked_store() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let home = tempfile::tempdir().unwrap();
        write_credentials(home.path(), "secret", u64::MAX);
        let path = home.path().join(".credentials.json");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_access_token_in(home.path(), "remote", "https://example.test/mcp").is_err());
        fs::remove_file(&path).unwrap();
        symlink("/dev/null", path).unwrap();
        assert!(read_access_token_in(home.path(), "remote", "https://example.test/mcp").is_err());
    }
}
