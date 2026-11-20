//! A throwaway daemon for tests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use anyhow::Result;
use tb_protocol::ConnectionInfo;

use crate::{FakeConfig, FakeExtension};

/// A daemon on a free loopback port with its own connection file in a temporary directory.
pub struct TestDaemon {
    pub server: Option<tbd::RunningServer>,
    pub dir: PathBuf,
}

impl TestDaemon {
    pub async fn start() -> Result<Self> {
        Self::start_with(Duration::from_secs(30)).await
    }

    pub async fn start_with(request_timeout: Duration) -> Result<Self> {
        Self::start_configured(
            request_timeout,
            Some(
                r#"
[accounts.account1]
access = "full"
permissions = ["read", "move", "tag", "trash", "send"]
[accounts.account2]
access = "full"
permissions = ["read", "move", "tag", "trash", "send"]
"#,
            ),
        )
        .await
    }

    pub async fn with_policy(policy: Option<&str>) -> Result<Self> {
        Self::start_configured(Duration::from_secs(30), policy).await
    }

    async fn start_configured(request_timeout: Duration, policy: Option<&str>) -> Result<Self> {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("tb-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let policy_file = dir.join("policy.toml");
        std::fs::write(&policy_file, policy.unwrap_or(""))?;
        let server = tbd::start(tbd::Config {
            connection_file: Some(dir.join("connection.json")),
            request_timeout,
            audit_file: Some(dir.join("audit.jsonl")),
            policy_file: Some(policy_file),
            ..tbd::Config::default()
        })
        .await?;
        Ok(Self {
            server: Some(server),
            dir,
        })
    }

    pub fn info(&self) -> &ConnectionInfo {
        &self.server.as_ref().expect("server is running").info
    }

    pub fn extension_info(&self) -> &ConnectionInfo {
        &self
            .server
            .as_ref()
            .expect("server is running")
            .extension_info
    }

    pub fn connection_file(&self) -> PathBuf {
        self.dir.join("connection.json")
    }

    /// Daemon plus a connected fake extension.
    pub async fn with_extension(config: FakeConfig) -> Result<(Self, FakeExtension)> {
        let daemon = Self::start().await?;
        let ext = FakeExtension::connect(daemon.extension_info(), config).await?;
        Ok((daemon, ext))
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
