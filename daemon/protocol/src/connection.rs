//! Connection file: how the extension and the CLI find the daemon's port and session token.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Environment variable that overrides the connection file location.
pub const ENV_CONNECTION_FILE: &str = "TB_CONNECTION_FILE";

#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    #[error("cannot read connection file {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("connection file {path} is malformed: {source}")]
    Malformed {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("cannot write connection file {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionInfo {
    pub protocol: u32,
    pub host: String,
    pub port: u16,
    /// 64 lowercase hex characters, regenerated every time the daemon starts.
    pub token: String,
    pub pid: u32,
}

impl ConnectionInfo {
    /// WebSocket URL without credentials. Clients send the token in an
    /// `Authorization: Bearer` header.
    pub fn ws_url(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        format!("ws://{}:{}{}", host, self.port, crate::WS_PATH)
    }

    pub fn read(path: &Path) -> Result<Self, ConnectionError> {
        let text = fs::read_to_string(path).map_err(|source| ConnectionError::Read {
            path: path.to_owned(),
            source,
        })?;
        serde_json::from_str(&text).map_err(|source| ConnectionError::Malformed {
            path: path.to_owned(),
            source,
        })
    }

    /// Write the file atomically with owner-only permissions where the platform has them.
    pub fn write(&self, path: &Path) -> Result<(), ConnectionError> {
        let fail = |source| ConnectionError::Write {
            path: path.to_owned(),
            source,
        };
        if let Some(dir) = path.parent() {
            // Only a directory created here is ours to lock down; an existing one such as
            // $HOME or /tmp must keep its permissions.
            let created = !dir.exists();
            fs::create_dir_all(dir).map_err(fail)?;
            if created {
                restrict_dir(dir);
            }
        }
        let tmp = path.with_extension("json.tmp");
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp).map_err(fail)?;
        let text = serde_json::to_string_pretty(self).expect("connection info serializes");
        file.write_all(text.as_bytes()).map_err(fail)?;
        file.sync_all().map_err(fail)?;
        drop(file);
        fs::rename(&tmp, path).map_err(fail)
    }
}

#[cfg(unix)]
fn restrict_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn restrict_dir(_dir: &Path) {}

/// Default location: `$TB_CONNECTION_FILE`, else `<runtime or local data dir>/tb-agent/connection.json`.
pub fn default_path() -> PathBuf {
    if let Some(path) = std::env::var_os(ENV_CONNECTION_FILE) {
        return PathBuf::from(path);
    }
    dirs::runtime_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("tb-agent")
        .join("connection.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_round_trips() {
        let dir = std::env::temp_dir().join(format!("tb-conn-test-{}", std::process::id()));
        let path = dir.join("connection.json");
        let info = ConnectionInfo {
            protocol: 1,
            host: "127.0.0.1".into(),
            port: 4242,
            token: "ab".repeat(32),
            pid: 1,
        };
        info.write(&path).unwrap();
        assert_eq!(ConnectionInfo::read(&path).unwrap(), info);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn existing_parent_directory_keeps_its_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tb-conn-perm-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let info = ConnectionInfo {
            protocol: 1,
            host: "127.0.0.1".into(),
            port: 1,
            token: "t".into(),
            pid: 1,
        };
        info.write(&dir.join("connection.json")).unwrap();
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        let _ = fs::remove_dir_all(dir);
    }
}
