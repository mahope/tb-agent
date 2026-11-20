#![allow(dead_code)]

use std::process::Stdio;

use serde_json::Value;
use tb_testkit::{FakeConfig, FakeExtension, TestDaemon};
use tokio::process::Command;

pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|e| {
            panic!(
                "stdout is not JSON ({e}): {:?}\nstderr: {}",
                self.stdout, self.stderr
            )
        })
    }

    pub fn error_json(&self) -> Value {
        serde_json::from_str(self.stderr.trim())
            .unwrap_or_else(|e| panic!("stderr is not JSON ({e}): {:?}", self.stderr))
    }
}

pub struct Env {
    pub daemon: TestDaemon,
    pub ext: FakeExtension,
}

impl Env {
    pub async fn new() -> Self {
        let (daemon, ext) = TestDaemon::with_extension(FakeConfig::default())
            .await
            .unwrap();
        Self { daemon, ext }
    }

    pub fn command(&self) -> Command {
        command_for(&self.daemon.connection_file())
    }

    pub async fn tb(&self, args: &[&str]) -> Output {
        run(self.command(), args).await
    }

    /// The last (operation, params) the extension received.
    pub fn last_call(&self) -> (String, Value) {
        self.ext
            .calls()
            .pop()
            .expect("the extension received a call")
    }
}

pub fn command_for(connection_file: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tb"));
    cmd.env("TB_CONNECTION_FILE", connection_file)
        .env_remove("RUST_LOG")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    cmd
}

pub async fn run(mut cmd: Command, args: &[&str]) -> Output {
    let out = cmd.args(args).output().await.expect("tb runs");
    Output {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}
