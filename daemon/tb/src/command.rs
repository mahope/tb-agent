//! Command line definition and the mapping from each command to a protocol operation.

use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{Map, Value, json};

/// Work in all Thunderbird mail accounts from the command line. Every command prints JSON.
#[derive(Parser)]
#[command(name = "tb", version)]
pub struct Cli {
    /// Connection file written by tbd (default: platform runtime directory).
    #[arg(long, global = true, env = "TB_CONNECTION_FILE")]
    pub connection_file: Option<PathBuf>,
    /// Seconds to wait for an operation before giving up.
    #[arg(long, global = true)]
    pub timeout: Option<u64>,
    /// Indent the JSON output (default when stdout is a terminal).
    #[arg(long, global = true)]
    pub pretty: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Show daemon status and the connected extension's capabilities.
    Status,
    /// List the protocol operations (works without a daemon).
    Ops,
    /// List mail accounts and identities.
    Accounts,
    /// List folders.
    Folders(FoldersArgs),
    /// Search messages.
    Search(SearchArgs),
    /// List recent messages.
    Recent(RecentArgs),
    /// Read one message.
    Get(GetArgs),
    /// Read several messages from one folder.
    GetMany(GetManyArgs),
    /// Read message headers only.
    Headers(HeadersArgs),
    /// Find messages by attachment name or type.
    Attachments(AttachmentsArgs),
    /// Change read/flag/tag state, move or copy messages.
    Update(UpdateArgs),
    /// Mark messages read, unread, flagged or unflagged.
    Mark(MarkArgs),
    /// Add or remove tags.
    Tag(TagArgs),
    /// Move messages to another folder.
    Move(MoveArgs),
    /// Move messages to Trash.
    Trash(IdsFolder),
    /// Undo a move, tag, mark or trash operation by audit ID.
    Undo { audit_id: String },
    /// Delete messages (drafts go to Trash).
    Delete(IdsFolder),
    /// Save a draft without sending.
    Draft(DraftArgs),
    /// Compose a new message; opens a review window unless --yes is given.
    Send(SendArgs),
    /// Reply to a message.
    Reply(ReplyArgs),
    /// Forward a message.
    Forward(ForwardArgs),
    /// Refresh a folder from the server.
    Refresh(RefreshArgs),
    /// Call any operation with raw JSON parameters.
    Call(CallArgs),
    /// Run the MCP server (stdio by default).
    Mcp(McpArgs),
}

#[derive(Args)]
pub struct FoldersArgs {
    /// Only this account (ID from `tb accounts`).
    #[arg(long)]
    pub account: Option<String>,
    /// Only this folder URI and its subfolders.
    #[arg(long)]
    pub folder: Option<String>,
    /// Only favorite folders.
    #[arg(long)]
    pub favorites: bool,
    /// Compact table output.
    #[arg(long)]
    pub table: bool,
}

#[derive(Args)]
pub struct SearchArgs {
    /// Text to search; prefix a word with from:, subject:, to: or cc: to restrict the field.
    pub query: String,
    #[arg(long)]
    pub folder: Option<String>,
    /// On or after this ISO 8601 date.
    #[arg(long)]
    pub since: Option<String>,
    /// On or before this ISO 8601 date.
    #[arg(long)]
    pub until: Option<String>,
    #[arg(long)]
    pub limit: Option<u32>,
    /// Skip this many results; switches output to a paged object.
    #[arg(long)]
    pub offset: Option<u32>,
    #[arg(long, value_enum)]
    pub sort: Option<Sort>,
    #[arg(long)]
    pub unread: bool,
    #[arg(long)]
    pub flagged: bool,
    #[arg(long)]
    pub tag: Option<String>,
    /// Do not search subfolders.
    #[arg(long)]
    pub no_subfolders: bool,
    /// Only count matches.
    #[arg(long)]
    pub count: bool,
    /// Search message bodies too (slower).
    #[arg(long)]
    pub body: bool,
    /// Keep every copy of a message that lives in several folders.
    #[arg(long)]
    pub all_copies: bool,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Sort {
    Asc,
    Desc,
}

#[derive(Args)]
pub struct RecentArgs {
    #[arg(long)]
    pub folder: Option<String>,
    /// Only messages from the last N days.
    #[arg(long)]
    pub days: Option<u32>,
    #[arg(long)]
    pub limit: Option<u32>,
    #[arg(long)]
    pub offset: Option<u32>,
    #[arg(long)]
    pub unread: bool,
    #[arg(long)]
    pub flagged: bool,
    #[arg(long)]
    pub no_subfolders: bool,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum BodyFormat {
    Markdown,
    Text,
    Html,
}

impl BodyFormat {
    fn as_str(self) -> &'static str {
        match self {
            BodyFormat::Markdown => "markdown",
            BodyFormat::Text => "text",
            BodyFormat::Html => "html",
        }
    }
}

#[derive(Args)]
pub struct GetArgs {
    /// Message ID from search results.
    pub id: String,
    /// Folder URI the message lives in.
    #[arg(long)]
    pub folder: String,
    #[arg(long, value_enum)]
    pub format: Option<BodyFormat>,
    /// Return the raw RFC 2822 source.
    #[arg(long)]
    pub raw: bool,
    /// Save attachments to a temp directory and report their paths.
    #[arg(long)]
    pub save_attachments: bool,
}

#[derive(Args)]
pub struct GetManyArgs {
    #[arg(required = true)]
    pub ids: Vec<String>,
    #[arg(long)]
    pub folder: String,
    #[arg(long, value_enum)]
    pub format: Option<BodyFormat>,
    #[arg(long)]
    pub raw: bool,
    #[arg(long)]
    pub save_attachments: bool,
}

#[derive(Args)]
pub struct HeadersArgs {
    #[arg(required = true)]
    pub ids: Vec<String>,
    #[arg(long)]
    pub folder: String,
}

#[derive(Args)]
pub struct AttachmentsArgs {
    /// Attachment filename contains this text.
    #[arg(long)]
    pub name: Option<String>,
    /// Content-Type starts with this, for example application/pdf.
    #[arg(long = "type")]
    pub content_type: Option<String>,
    #[arg(long)]
    pub folder: Option<String>,
    #[arg(long)]
    pub limit: Option<u32>,
    #[arg(long)]
    pub scan_cap: Option<u32>,
}

#[derive(Args)]
pub struct IdsFolder {
    #[arg(required = true)]
    pub ids: Vec<String>,
    #[arg(long)]
    pub folder: String,
}

#[derive(Args)]
pub struct UpdateArgs {
    #[arg(required = true)]
    pub ids: Vec<String>,
    #[arg(long)]
    pub folder: String,
    #[arg(long, conflicts_with = "unread")]
    pub read: bool,
    #[arg(long)]
    pub unread: bool,
    #[arg(long, conflicts_with = "unflag")]
    pub flag: bool,
    #[arg(long)]
    pub unflag: bool,
    #[arg(long = "add-tag")]
    pub add_tags: Vec<String>,
    #[arg(long = "remove-tag")]
    pub remove_tags: Vec<String>,
    #[arg(long, conflicts_with_all = ["copy_to", "trash"])]
    pub move_to: Option<String>,
    #[arg(long, conflicts_with_all = ["move_to", "trash"])]
    pub copy_to: Option<String>,
    #[arg(long)]
    pub trash: bool,
}

#[derive(Args)]
pub struct MarkArgs {
    #[arg(required = true)]
    pub ids: Vec<String>,
    #[arg(long)]
    pub folder: String,
    #[arg(long, conflicts_with = "unread")]
    pub read: bool,
    #[arg(long)]
    pub unread: bool,
    #[arg(long, conflicts_with = "unflag")]
    pub flag: bool,
    #[arg(long)]
    pub unflag: bool,
}

#[derive(Args)]
pub struct TagArgs {
    #[arg(required = true)]
    pub ids: Vec<String>,
    #[arg(long)]
    pub folder: String,
    /// Tag keyword to add, for example $label1.
    #[arg(long = "add")]
    pub add: Vec<String>,
    #[arg(long = "remove")]
    pub remove: Vec<String>,
}

#[derive(Args)]
pub struct MoveArgs {
    #[arg(required = true)]
    pub ids: Vec<String>,
    #[arg(long)]
    pub folder: String,
    /// Destination folder URI.
    #[arg(long)]
    pub to: String,
}

#[derive(Args)]
pub struct ComposeCommon {
    #[arg(long)]
    pub cc: Option<String>,
    #[arg(long)]
    pub bcc: Option<String>,
    /// Sender identity: email address or identity ID.
    #[arg(long)]
    pub from: Option<String>,
    /// Message body.
    #[arg(long, conflicts_with = "body_file")]
    pub body: Option<String>,
    /// Read the body from this file, or `-` for stdin.
    #[arg(long)]
    pub body_file: Option<String>,
    /// Treat the body as HTML.
    #[arg(long)]
    pub html: bool,
    /// Attach a file (repeatable).
    #[arg(long = "attach")]
    pub attach: Vec<String>,
}

#[derive(Args)]
pub struct DraftArgs {
    #[arg(long)]
    pub to: Option<String>,
    #[arg(long)]
    pub subject: Option<String>,
    #[command(flatten)]
    pub common: ComposeCommon,
    /// Bracketed Message-ID this draft replies to.
    #[arg(long)]
    pub in_reply_to: Option<String>,
    #[arg(long)]
    pub references: Option<String>,
    /// Replace this existing draft (needs --replace-folder).
    #[arg(long, requires = "replace_folder")]
    pub replace: Option<String>,
    #[arg(long, requires = "replace")]
    pub replace_folder: Option<String>,
    /// Do not append the identity signature.
    #[arg(long)]
    pub no_signature: bool,
}

#[derive(Args)]
pub struct SendArgs {
    #[arg(long)]
    pub to: String,
    #[arg(long)]
    pub subject: String,
    #[command(flatten)]
    pub common: ComposeCommon,
    /// Send without a review window (the extension may still refuse).
    #[arg(long)]
    pub yes: bool,
    #[arg(long)]
    pub no_signature: bool,
}

#[derive(Args)]
pub struct ReplyArgs {
    /// Message ID to reply to.
    pub id: String,
    #[arg(long)]
    pub folder: String,
    #[command(flatten)]
    pub common: ComposeCommon,
    #[arg(long)]
    pub all: bool,
    /// Override the recipient.
    #[arg(long)]
    pub to: Option<String>,
    /// Save to Drafts instead of opening a compose window.
    #[arg(long, conflicts_with = "yes")]
    pub draft: bool,
    #[arg(long)]
    pub yes: bool,
}

#[derive(Args)]
pub struct ForwardArgs {
    pub id: String,
    #[arg(long)]
    pub folder: String,
    #[arg(long)]
    pub to: String,
    #[command(flatten)]
    pub common: ComposeCommon,
    #[arg(long)]
    pub yes: bool,
}

#[derive(Args)]
pub struct RefreshArgs {
    /// Folder URI.
    pub folder: String,
    #[arg(long)]
    pub timeout_ms: Option<u64>,
}

#[derive(Args)]
pub struct CallArgs {
    /// Operation name, see `tb ops`.
    pub op: String,
    /// Parameters as a JSON object.
    #[arg(long, conflicts_with = "params_file")]
    pub params: Option<String>,
    /// Read the parameters from this file, or `-` for stdin.
    #[arg(long)]
    pub params_file: Option<String>,
}

#[derive(Args)]
pub struct McpArgs {
    /// Serve streamable HTTP on this address instead of stdio. Loopback by default.
    #[arg(
        long,
        value_name = "ADDR",
        num_args = 0..=1,
        default_missing_value = "127.0.0.1:8787"
    )]
    pub http: Option<std::net::SocketAddr>,
    /// Require this bearer token on the HTTP endpoint. Mandatory for non-loopback addresses.
    #[arg(long, env = "TB_MCP_TOKEN", hide_env_values = true)]
    pub token: Option<String>,
    /// Extra Host header values to accept in HTTP mode (needed when binding a non-loopback
    /// address). Repeatable.
    #[arg(long = "allow-host")]
    pub allow_hosts: Vec<String>,
}

/// Collects parameters, skipping unset ones.
#[derive(Default)]
struct Params(Map<String, Value>);

impl Params {
    fn set(&mut self, key: &str, value: impl Into<Value>) -> &mut Self {
        self.0.insert(key.to_owned(), value.into());
        self
    }
    fn opt<T: Into<Value>>(&mut self, key: &str, value: Option<T>) -> &mut Self {
        if let Some(v) = value {
            self.set(key, v);
        }
        self
    }
    fn flag(&mut self, key: &str, on: bool) -> &mut Self {
        if on {
            self.set(key, true);
        }
        self
    }
    fn into_value(self) -> Value {
        Value::Object(self.0)
    }
}

fn read_text(source: &str) -> Result<String> {
    if source == "-" {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .context("cannot read stdin")?;
        Ok(text)
    } else {
        std::fs::read_to_string(source).with_context(|| format!("cannot read {source}"))
    }
}

fn compose(p: &mut Params, c: &ComposeCommon) -> Result<()> {
    let body = match (&c.body, &c.body_file) {
        (Some(b), _) => Some(b.clone()),
        (None, Some(f)) => Some(read_text(f)?),
        (None, None) => None,
    };
    p.opt("body", body)
        .opt("cc", c.cc.clone())
        .opt("bcc", c.bcc.clone())
        .opt("from", c.from.clone())
        .flag("isHtml", c.html);
    if !c.attach.is_empty() {
        let paths = c
            .attach
            .iter()
            .map(|a| {
                std::fs::canonicalize(a)
                    .map(|p| p.to_string_lossy().into_owned())
                    .with_context(|| format!("cannot attach {a}"))
            })
            .collect::<Result<Vec<_>>>()?;
        p.set("attachments", paths);
    }
    Ok(())
}

fn state(p: &mut Params, read: bool, unread: bool, flag: bool, unflag: bool) {
    if read || unread {
        p.set("read", read);
    }
    if flag || unflag {
        p.set("flagged", flag);
    }
}

fn ids(p: &mut Params, ids: &[String], folder: &str) {
    if let [one] = ids {
        p.set("messageId", one.clone());
    } else {
        p.set("messageIds", ids.to_vec());
    }
    p.set("folderPath", folder);
}

impl Command {
    /// The protocol operation and parameters for this command. `None` for commands that are not
    /// a single operation (`ops`, `mcp`).
    pub fn to_request(&self) -> Result<Option<(String, Value)>> {
        let mut p = Params::default();
        let op = match self {
            Command::Ops | Command::Mcp(_) => return Ok(None),
            Command::Status => "status",
            Command::Undo { audit_id } => {
                p.set("auditId", audit_id.clone());
                "undo"
            }
            Command::Accounts => "listAccounts",
            Command::Folders(a) => {
                p.opt("accountId", a.account.clone())
                    .opt("folderPath", a.folder.clone())
                    .flag("favoritesOnly", a.favorites);
                if a.table {
                    p.set("format", "table");
                }
                "listFolders"
            }
            Command::Search(a) => {
                p.set("query", a.query.clone())
                    .opt("folderPath", a.folder.clone())
                    .opt("startDate", a.since.clone())
                    .opt("endDate", a.until.clone())
                    .opt("maxResults", a.limit)
                    .opt("offset", a.offset)
                    .opt(
                        "sortOrder",
                        a.sort.map(|s| match s {
                            Sort::Asc => "asc",
                            Sort::Desc => "desc",
                        }),
                    )
                    .flag("unreadOnly", a.unread)
                    .flag("flaggedOnly", a.flagged)
                    .opt("tag", a.tag.clone())
                    .flag("countOnly", a.count)
                    .flag("searchBody", a.body);
                if a.no_subfolders {
                    p.set("includeSubfolders", false);
                }
                if a.all_copies {
                    p.set("dedupByMessageId", false);
                }
                "searchMessages"
            }
            Command::Recent(a) => {
                p.opt("folderPath", a.folder.clone())
                    .opt("daysBack", a.days)
                    .opt("maxResults", a.limit)
                    .opt("offset", a.offset)
                    .flag("unreadOnly", a.unread)
                    .flag("flaggedOnly", a.flagged);
                if a.no_subfolders {
                    p.set("includeSubfolders", false);
                }
                "getRecentMessages"
            }
            Command::Get(a) => {
                p.set("messageId", a.id.clone())
                    .set("folderPath", a.folder.clone())
                    .opt("bodyFormat", a.format.map(BodyFormat::as_str))
                    .flag("rawSource", a.raw)
                    .flag("saveAttachments", a.save_attachments);
                "getMessage"
            }
            Command::GetMany(a) => {
                let messages: Vec<Value> = a
                    .ids
                    .iter()
                    .map(|id| json!({"messageId": id, "folderPath": a.folder}))
                    .collect();
                p.set("messages", messages)
                    .opt("bodyFormat", a.format.map(BodyFormat::as_str))
                    .flag("rawSource", a.raw)
                    .flag("saveAttachments", a.save_attachments);
                "getMessages"
            }
            Command::Headers(a) => {
                p.set("folderPath", a.folder.clone());
                if let [one] = a.ids.as_slice() {
                    p.set("messageId", one.clone());
                    "getMessageHeaders"
                } else {
                    p.set("messageIds", a.ids.clone());
                    "batchGetMessageHeaders"
                }
            }
            Command::Attachments(a) => {
                if a.name.is_none() && a.content_type.is_none() {
                    bail!("give --name and/or --type");
                }
                p.opt("nameContains", a.name.clone())
                    .opt("contentType", a.content_type.clone())
                    .opt("folderPath", a.folder.clone())
                    .opt("maxResults", a.limit)
                    .opt("scanCap", a.scan_cap);
                "searchAttachments"
            }
            Command::Update(a) => {
                ids(&mut p, &a.ids, &a.folder);
                state(&mut p, a.read, a.unread, a.flag, a.unflag);
                if !a.add_tags.is_empty() {
                    p.set("addTags", a.add_tags.clone());
                }
                if !a.remove_tags.is_empty() {
                    p.set("removeTags", a.remove_tags.clone());
                }
                p.opt("moveTo", a.move_to.clone())
                    .opt("copyTo", a.copy_to.clone())
                    .flag("trash", a.trash);
                "updateMessage"
            }
            Command::Mark(a) => {
                if !(a.read || a.unread || a.flag || a.unflag) {
                    bail!("give at least one of --read, --unread, --flag, --unflag");
                }
                ids(&mut p, &a.ids, &a.folder);
                state(&mut p, a.read, a.unread, a.flag, a.unflag);
                "updateMessage"
            }
            Command::Tag(a) => {
                if a.add.is_empty() && a.remove.is_empty() {
                    bail!("give at least one --add or --remove");
                }
                ids(&mut p, &a.ids, &a.folder);
                if !a.add.is_empty() {
                    p.set("addTags", a.add.clone());
                }
                if !a.remove.is_empty() {
                    p.set("removeTags", a.remove.clone());
                }
                "updateMessage"
            }
            Command::Move(a) => {
                ids(&mut p, &a.ids, &a.folder);
                p.set("moveTo", a.to.clone());
                "updateMessage"
            }
            Command::Trash(a) => {
                ids(&mut p, &a.ids, &a.folder);
                p.set("trash", true);
                "updateMessage"
            }
            Command::Delete(a) => {
                p.set("messageIds", a.ids.clone())
                    .set("folderPath", a.folder.clone());
                "deleteMessages"
            }
            Command::Draft(a) => {
                p.opt("to", a.to.clone())
                    .opt("subject", a.subject.clone())
                    .opt("inReplyTo", a.in_reply_to.clone())
                    .opt("references", a.references.clone())
                    .opt("replaceMessageId", a.replace.clone())
                    .opt("replaceFolderPath", a.replace_folder.clone());
                compose(&mut p, &a.common)?;
                if a.no_signature {
                    p.set("includeSignature", false);
                }
                "saveDraft"
            }
            Command::Send(a) => {
                p.set("to", a.to.clone())
                    .set("subject", a.subject.clone())
                    .flag("skipReview", a.yes);
                compose(&mut p, &a.common)?;
                if !p.0.contains_key("body") {
                    bail!("give --body or --body-file");
                }
                if a.no_signature {
                    p.set("includeSignature", false);
                }
                "sendMail"
            }
            Command::Reply(a) => {
                p.set("messageId", a.id.clone())
                    .set("folderPath", a.folder.clone())
                    .flag("replyAll", a.all)
                    .opt("to", a.to.clone())
                    .flag("saveAsDraft", a.draft)
                    .flag("skipReview", a.yes);
                compose(&mut p, &a.common)?;
                if !p.0.contains_key("body") {
                    bail!("give --body or --body-file");
                }
                "replyToMessage"
            }
            Command::Forward(a) => {
                p.set("messageId", a.id.clone())
                    .set("folderPath", a.folder.clone())
                    .set("to", a.to.clone())
                    .flag("skipReview", a.yes);
                compose(&mut p, &a.common)?;
                "forwardMessage"
            }
            Command::Refresh(a) => {
                p.set("folderPath", a.folder.clone())
                    .opt("timeoutMs", a.timeout_ms);
                "refreshFolder"
            }
            Command::Call(a) => {
                let text = match (&a.params, &a.params_file) {
                    (Some(t), _) => t.clone(),
                    (None, Some(f)) => read_text(f)?,
                    (None, None) => "{}".to_owned(),
                };
                let params: Value =
                    serde_json::from_str(&text).context("params are not valid JSON")?;
                return Ok(Some((a.op.clone(), params)));
            }
        };
        Ok(Some((op.to_owned(), p.into_value())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(args: &[&str]) -> (String, Value) {
        let cli = Cli::try_parse_from(std::iter::once("tb").chain(args.iter().copied())).unwrap();
        cli.command.to_request().unwrap().unwrap()
    }

    #[test]
    fn every_command_maps_to_a_known_operation_with_valid_params() {
        let cases: &[&[&str]] = &[
            &["status"],
            &["accounts"],
            &["folders", "--account", "a1", "--table"],
            &[
                "search",
                "report",
                "--limit",
                "5",
                "--unread",
                "--no-subfolders",
            ],
            &["recent", "--days", "3"],
            &["get", "m1", "--folder", "f", "--format", "text"],
            &["get-many", "m1", "m2", "--folder", "f"],
            &["headers", "m1", "--folder", "f"],
            &["headers", "m1", "m2", "--folder", "f"],
            &["attachments", "--name", "invoice"],
            &[
                "update",
                "m1",
                "--folder",
                "f",
                "--read",
                "--add-tag",
                "$label1",
            ],
            &["mark", "m1", "m2", "--folder", "f", "--unflag"],
            &["tag", "m1", "--folder", "f", "--add", "x"],
            &["move", "m1", "--folder", "f", "--to", "g"],
            &["trash", "m1", "--folder", "f"],
            &["delete", "m1", "--folder", "f"],
            &["draft", "--to", "a@b.c", "--body", "hi"],
            &["send", "--to", "a@b.c", "--subject", "s", "--body", "hi"],
            &["reply", "m1", "--folder", "f", "--body", "hi", "--draft"],
            &["forward", "m1", "--folder", "f", "--to", "a@b.c"],
            &["refresh", "f"],
        ];
        for args in cases {
            let (op, params) = request(args);
            let spec =
                tb_protocol::find_op(&op).unwrap_or_else(|| panic!("{args:?}: unknown op {op}"));
            tb_protocol::validate_params(spec, &params)
                .unwrap_or_else(|e| panic!("{args:?}: {e} in {params}"));
        }
    }

    #[test]
    fn single_id_uses_message_id_and_several_use_message_ids() {
        let (_, one) = request(&["mark", "m1", "--folder", "f", "--read"]);
        assert_eq!(one["messageId"], "m1");
        let (_, many) = request(&["mark", "m1", "m2", "--folder", "f", "--read"]);
        assert_eq!(many["messageIds"], json!(["m1", "m2"]));
    }

    #[test]
    fn unread_sets_read_false() {
        let (_, p) = request(&["mark", "m1", "--folder", "f", "--unread"]);
        assert_eq!(p["read"], false);
    }
}
