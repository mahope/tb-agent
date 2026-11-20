//! Write-ahead JSONL journal. An intent survives timeouts and daemon crashes.

use std::collections::{BTreeSet, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::auth::new_token;
use crate::policy::Permission;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MessageState {
    pub message_id: String,
    pub folder_path: String,
    pub read: bool,
    pub flagged: bool,
    pub tags: BTreeSet<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteIntent {
    pub accounts: BTreeSet<String>,
    pub operation: String,
    pub permissions: BTreeSet<Permission>,
    pub message_ids: Vec<String>,
    pub previous: Vec<MessageState>,
    pub reversible: bool,
    pub undo_of: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(
    tag = "event",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum Event {
    Intent {
        audit_id: String,
        time_ms: u64,
        #[serde(flatten)]
        write: WriteIntent,
    },
    Finished {
        audit_id: String,
        time_ms: u64,
        success: bool,
        after: Vec<MessageState>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_message_id: Option<String>,
    },
}

#[derive(Clone)]
pub struct Undo {
    pub write: WriteIntent,
    pub after: Vec<MessageState>,
}

pub struct Audit {
    file: File,
    intents: HashMap<String, WriteIntent>,
    completed: HashMap<String, Undo>,
    failed: bool,
}

impl Audit {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let mut options = OpenOptions::new();
        options.read(true).append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(path)
            .with_context(|| format!("open audit log {}", path.display()))?;
        file.try_lock()
            .with_context(|| format!("lock audit log {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        let mut audit = Self {
            file,
            intents: HashMap::new(),
            completed: HashMap::new(),
            failed: false,
        };
        let mut reader = BufReader::new(audit.file.try_clone()?);
        let mut line = String::new();
        let mut line_number = 0;
        while reader.read_line(&mut line)? != 0 {
            line_number += 1;
            ensure!(
                line.ends_with('\n'),
                "incomplete audit log record on line {line_number}"
            );
            let event = serde_json::from_str(&line)
                .map_err(|_| anyhow::anyhow!("invalid audit log record on line {line_number}"))?;
            audit.apply(event)?;
            line.clear();
        }
        Ok(audit)
    }

    fn apply(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Intent {
                audit_id, write, ..
            } => {
                if let Some(original) = &write.undo_of {
                    ensure!(
                        self.completed.remove(original).is_some(),
                        "invalid undo reference in audit log"
                    );
                }
                ensure!(
                    self.intents.insert(audit_id, write).is_none(),
                    "duplicate audit intent"
                );
            }
            Event::Finished {
                audit_id,
                success,
                after,
                ..
            } => {
                let write = self
                    .intents
                    .remove(&audit_id)
                    .context("audit outcome has no intent")?;
                if success && write.reversible && !after.is_empty() {
                    self.completed.insert(audit_id, Undo { write, after });
                }
            }
        }
        Ok(())
    }

    fn append(&mut self, event: Event) -> Result<()> {
        ensure!(!self.failed, "audit log is unavailable");
        let mut line = serde_json::to_vec(&event)?;
        line.push(b'\n');
        let result = self
            .file
            .write_all(&line)
            .and_then(|()| self.file.sync_data())
            .context("persist audit record")
            .and_then(|()| self.apply(event));
        self.failed = result.is_err();
        result
    }

    pub fn begin(&mut self, write: WriteIntent) -> Result<String> {
        let audit_id = new_token();
        self.append(Event::Intent {
            audit_id: audit_id.clone(),
            time_ms: now(),
            write,
        })?;
        Ok(audit_id)
    }

    pub fn finish(
        &mut self,
        audit_id: String,
        success: bool,
        after: Vec<MessageState>,
        created_message_id: Option<String>,
    ) -> Result<()> {
        self.append(Event::Finished {
            audit_id,
            time_ms: now(),
            success,
            after,
            created_message_id,
        })
    }

    pub fn undo(&self, audit_id: &str) -> Option<Undo> {
        self.completed.get(audit_id).cloned()
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
