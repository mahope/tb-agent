//! Authorize resolved Thunderbird resources, then shape restricted read results.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tb_protocol::frame::Inventory;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Access {
    Off,
    Headers,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Permission {
    Read,
    Move,
    Tag,
    Trash,
    Send,
    Filters,
    Rules,
    Accounts,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountPolicy {
    access: Access,
    permissions: BTreeSet<Permission>,
}

impl AccountPolicy {
    fn allows(&self, permission: Permission) -> bool {
        self.permissions.contains(&permission)
            && match self.access {
                Access::Off => false,
                Access::Headers => permission == Permission::Read,
                Access::Full => true,
            }
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Policy {
    #[serde(default)]
    accounts: BTreeMap<String, AccountPolicy>,
}

#[derive(Default)]
pub(crate) struct Scope {
    pub accounts: BTreeSet<String>,
    headers: BTreeSet<String>,
}

pub(crate) struct Route {
    pub op: String,
    pub params: Value,
    pub scope: Scope,
    pub permissions: BTreeSet<Permission>,
    pub message_ids: Vec<String>,
    pub snapshot_folder: Option<String>,
    pub reversible: bool,
}

const DENIED: &str = "operation is outside the account policy or its account could not be resolved";

impl Policy {
    pub fn load(path: &Path, required: bool) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|_| anyhow::anyhow!("invalid account policy in {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && !required => {
                Ok(Self::default())
            }
            Err(err) => Err(err).with_context(|| format!("read account policy {}", path.display())),
        }
    }

    fn allows(&self, account: &str, permission: Permission) -> bool {
        self.accounts
            .get(account)
            .map_or(permission == Permission::Read, |p| p.allows(permission))
    }

    pub fn allows_on_all(&self, accounts: &BTreeSet<String>, permission: Permission) -> bool {
        accounts.iter().all(|id| self.allows(id, permission))
    }

    fn headers(&self, account: &str) -> bool {
        self.accounts
            .get(account)
            .is_none_or(|p| p.access == Access::Headers)
    }

    pub fn undo_scope(
        &self,
        inventory: &Inventory,
        accounts: &BTreeSet<String>,
        permissions: &BTreeSet<Permission>,
        folders: impl IntoIterator<Item = String>,
    ) -> Result<Scope, &'static str> {
        let resolved = folders
            .into_iter()
            .map(|path| owner(inventory, &path).map(str::to_owned))
            .collect::<Result<BTreeSet<_>, _>>()?;
        if resolved != *accounts
            || !accounts.iter().all(|account| {
                permissions
                    .iter()
                    .all(|permission| self.allows(account, *permission))
            })
        {
            return Err(DENIED);
        }
        Ok(Scope {
            accounts: accounts.clone(),
            headers: BTreeSet::new(),
        })
    }

    pub fn route(
        &self,
        inventory: &Inventory,
        op: &str,
        mut params: Value,
        independent: bool,
    ) -> Result<Route, &'static str> {
        use Permission::*;
        let mut accounts = BTreeSet::new();
        let mut permissions = BTreeSet::new();
        let mut message_ids = Vec::new();
        let mut snapshot_folder = None;
        let mut reversible = false;
        let mut forwarded = op.to_owned();

        match op {
            "listAccounts" | "getAccountAccess" | "listFolders" | "searchMessages"
            | "getRecentMessages" | "searchAttachments" => {
                if let Some(folder) = params["folderPath"].as_str() {
                    accounts.insert(owner(inventory, folder)?.to_owned());
                }
                if let Some(account) = params["accountId"].as_str() {
                    if !inventory.accounts.iter().any(|id| id == account)
                        || accounts.iter().any(|id| id != account)
                    {
                        return Err(DENIED);
                    }
                    accounts.insert(account.to_owned());
                }
                let needs_full = op == "searchAttachments" || params["searchBody"] == true;
                let permitted =
                    |id: &str| self.allows(id, Read) && (!needs_full || !self.headers(id));
                if accounts.is_empty() {
                    accounts.extend(
                        inventory
                            .accounts
                            .iter()
                            .filter(|id| permitted(id))
                            .cloned(),
                    );
                } else if !accounts.iter().all(|id| permitted(id)) {
                    return Err(DENIED);
                }
            }
            "getMessage"
            | "getMessageHeaders"
            | "batchGetMessageHeaders"
            | "refreshFolder"
            | "displayMessage" => {
                let account = folder_account(inventory, &params)?;
                if !self.allows(account, Read) {
                    return Err(DENIED);
                }
                if op == "displayMessage" {
                    permissions.extend([Read, Tag]);
                }
                if self.headers(account) {
                    if matches!(op, "refreshFolder" | "displayMessage") {
                        return Err(DENIED);
                    }
                    if op == "getMessage" {
                        forwarded = "getMessageHeaders".into();
                        params = json!({"messageId": params["messageId"], "folderPath": params["folderPath"]});
                    }
                }
                accounts.insert(account.to_owned());
            }
            "getMessages" => {
                let items = params["messages"].as_array_mut().ok_or(DENIED)?;
                if items.is_empty() || items.len() > 20 {
                    return Err(DENIED);
                }
                for item in items {
                    let id = item["messageId"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .ok_or(DENIED)?;
                    let account = folder_account(inventory, item)?;
                    if !self.allows(account, Read) {
                        return Err(DENIED);
                    }
                    accounts.insert(account.to_owned());
                    *item = json!({"messageId": id, "folderPath": item["folderPath"]});
                }
                if accounts.iter().any(|account| self.headers(account)) {
                    for key in ["saveAttachments", "rawSource", "includeInlineImages"] {
                        params
                            .as_object_mut()
                            .expect("normalized params")
                            .remove(key);
                    }
                }
            }
            "updateMessage" | "deleteMessages" => {
                let account = folder_account(inventory, &params)?;
                accounts.insert(account.to_owned());
                message_ids = ids(&params)?;
                snapshot_folder = params["folderPath"].as_str().map(str::to_owned);
                let trash = params["trash"] == true;
                let move_to = params["moveTo"].as_str();
                let copy_to = params["copyTo"].as_str();
                if usize::from(trash)
                    + usize::from(move_to.is_some())
                    + usize::from(copy_to.is_some())
                    > 1
                {
                    return Err(DENIED);
                }
                if params.get("read").is_some()
                    || params.get("flagged").is_some()
                    || params.get("addTags").is_some()
                    || params.get("removeTags").is_some()
                {
                    permissions.insert(Tag);
                }
                if let Some(target) = move_to.or(copy_to) {
                    accounts.insert(owner(inventory, target)?.to_owned());
                    permissions.insert(Move);
                    if move_to.is_some()
                        && inventory.trash_folders.values().any(|trash| {
                            target == trash
                                || target
                                    .strip_prefix(trash)
                                    .is_some_and(|suffix| suffix.starts_with('/'))
                        })
                    {
                        permissions.insert(Trash);
                    }
                }
                let is_copy = copy_to.is_some();
                if trash || op == "deleteMessages" {
                    permissions.insert(Trash);
                    if let Some(target) = inventory.trash_folders.get(account) {
                        accounts.insert(owner(inventory, target)?.to_owned());
                        params["_trashFolder"] = json!(target);
                    } else if trash {
                        return Err(DENIED);
                    }
                }
                params["_allowTrashMove"] = json!(self.allows_on_all(&accounts, Trash));
                reversible = if op == "deleteMessages" {
                    inventory.trash_folders.contains_key(account)
                } else {
                    !is_copy
                };
                if permissions.is_empty() {
                    return Err(DENIED);
                }
            }
            "saveDraft" | "sendMail" | "replyToMessage" | "forwardMessage" | "dryRunCompose" => {
                let identity_id = match params["from"].as_str() {
                    Some(from) if inventory.identities.contains_key(from) => inventory
                        .identities
                        .get_key_value(from)
                        .ok_or(DENIED)?
                        .0
                        .as_str(),
                    Some(from) => {
                        let mut matches = inventory
                            .identities
                            .iter()
                            .filter(|(_, identity)| identity.email.eq_ignore_ascii_case(from));
                        let (id, _) = matches.next().ok_or(DENIED)?;
                        if matches.next().is_some() {
                            return Err(DENIED);
                        }
                        id
                    }
                    None => inventory.default_identity.as_deref().ok_or(DENIED)?,
                };
                let identity = inventory.identities.get(identity_id).ok_or(DENIED)?;
                if !inventory.accounts.contains(&identity.account_id) {
                    return Err(DENIED);
                }
                accounts.insert(identity.account_id.clone());
                let draft = op == "saveDraft" || params["saveAsDraft"] == true;
                if draft {
                    let folder = identity.drafts_folder.as_deref().ok_or(DENIED)?;
                    accounts.insert(owner(inventory, folder)?.to_owned());
                    params["_draftFolder"] = json!(folder);
                    params["_draftAccounts"] = json!(
                        inventory
                            .accounts
                            .iter()
                            .filter(|id| self.allows(id, Send))
                            .collect::<Vec<_>>()
                    );
                } else if let Some(folder) = &identity.sent_folder {
                    accounts.insert(owner(inventory, folder)?.to_owned());
                    params["_sentFolder"] = json!(folder);
                }
                if op == "replyToMessage" || op == "forwardMessage" {
                    let account = folder_account(inventory, &params)?;
                    if !self.allows(account, Read) || self.headers(account) {
                        return Err(DENIED);
                    }
                    accounts.insert(account.to_owned());
                    message_ids = ids(&params)?;
                    snapshot_folder = params["folderPath"].as_str().map(str::to_owned);
                }
                if params.get("replaceMessageId").is_some()
                    || params.get("replaceFolderPath").is_some()
                {
                    let folder = params["replaceFolderPath"]
                        .as_str()
                        .ok_or(DENIED)?
                        .to_owned();
                    let id = params["replaceMessageId"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .ok_or(DENIED)?;
                    let account = owner(inventory, &folder)?;
                    accounts.insert(account.to_owned());
                    message_ids.push(id.to_owned());
                    if let Some(trash) = inventory.trash_folders.get(account) {
                        accounts.insert(owner(inventory, trash)?.to_owned());
                        params["_trashFolder"] = json!(trash);
                    }
                    permissions.insert(Trash);
                    snapshot_folder = Some(folder);
                }
                params["_identityAccount"] = json!(identity.account_id);
                params["from"] = json!(identity_id);
                permissions.insert(Send);
            }
            _ if independent => {}
            _ => return Err(DENIED),
        }
        if !accounts
            .iter()
            .all(|id| permissions.iter().all(|p| self.allows(id, *p)))
        {
            return Err(DENIED);
        }
        let headers = accounts
            .iter()
            .filter(|id| self.headers(id))
            .cloned()
            .collect();
        Ok(Route {
            op: forwarded,
            params,
            scope: Scope { accounts, headers },
            permissions,
            message_ids,
            snapshot_folder,
            reversible,
        })
    }
}

fn owner<'a>(inventory: &'a Inventory, path: &str) -> Result<&'a str, &'static str> {
    inventory
        .folders
        .get(path)
        .filter(|account| inventory.accounts.contains(account))
        .map(String::as_str)
        .ok_or(DENIED)
}

fn folder_account<'a>(inventory: &'a Inventory, params: &Value) -> Result<&'a str, &'static str> {
    owner(inventory, params["folderPath"].as_str().ok_or(DENIED)?)
}

fn ids(params: &Value) -> Result<Vec<String>, &'static str> {
    let strings = match (&params["messageId"], &params["messageIds"]) {
        (Value::String(id), Value::Null) => vec![id.as_str()],
        (Value::Null, Value::Array(ids)) => ids
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()
            .ok_or(DENIED)?,
        _ => return Err(DENIED),
    };
    let unique: BTreeSet<_> = strings.iter().copied().collect();
    if strings.is_empty() || unique.len() != strings.len() || unique.contains("") {
        return Err(DENIED);
    }
    Ok(strings.into_iter().map(str::to_owned).collect())
}

impl Scope {
    pub fn wire(&self) -> Value {
        json!({"accounts": self.accounts, "headers": self.headers})
    }

    pub fn restricts_results(&self) -> bool {
        !self.headers.is_empty()
    }

    pub fn project(&self, inventory: &Inventory, op: &str, result: Value) -> Value {
        let project_item = |item: Value| {
            let account = match op {
                "listAccounts" | "getAccountAccess" => item["id"].as_str(),
                "listFolders" => item["accountId"].as_str(),
                _ => folder_account(inventory, &item).ok(),
            };
            let Some(account) = account.filter(|id| self.accounts.contains(*id)) else {
                return Value::Null;
            };
            if !self.headers.contains(account) || op == "listFolders" {
                return item;
            }
            let keys: &[&str] = if matches!(op, "listAccounts" | "getAccountAccess") {
                &["id"]
            } else {
                &[
                    "id",
                    "author",
                    "subject",
                    "date",
                    "folderPath",
                    "read",
                    "flagged",
                    "tags",
                ]
            };
            let mut header = serde_json::Map::new();
            for key in keys {
                if let Some(value) = item.get(key).filter(|v| match *key {
                    "read" | "flagged" => v.is_boolean(),
                    "tags" => v
                        .as_array()
                        .is_some_and(|tags| tags.iter().all(Value::is_string)),
                    _ => v.is_string(),
                }) {
                    header.insert((*key).into(), value.clone());
                }
            }
            Value::Object(header)
        };
        let project_list = |items: Vec<Value>| {
            Value::Array(
                items
                    .into_iter()
                    .map(project_item)
                    .filter(|v| !v.is_null())
                    .collect(),
            )
        };
        if op == "getMessages" {
            let project_batch = |items: Vec<Value>| {
                Value::Array(
                    items
                        .into_iter()
                        .enumerate()
                        .map(|(index, item)| {
                            if item.get("error").is_none() {
                                let projected = project_item(item);
                                if !projected.is_null() {
                                    return projected;
                                }
                            }
                            json!({"index": index, "error": "message could not be read"})
                        })
                        .collect(),
                )
            };
            return match result {
                Value::Array(items) => project_batch(items),
                Value::Object(mut fields) => {
                    let Some(Value::Array(items)) = fields.remove("messages") else {
                        return Value::Null;
                    };
                    fields.retain(|key, value| {
                        matches!(key.as_str(), "requested" | "succeeded" | "failed" | "max")
                            && value.is_u64()
                    });
                    fields.insert("messages".into(), project_batch(items));
                    Value::Object(fields)
                }
                _ => Value::Null,
            };
        }
        match result {
            Value::Object(mut fields) if op == "getAccountAccess" => {
                let accounts = fields
                    .remove("accounts")
                    .and_then(|value| value.as_array().cloned())
                    .unwrap_or_default();
                json!({"mode": "restricted", "accounts": project_list(accounts)})
            }
            Value::Object(mut fields) if op == "batchGetMessageHeaders" => {
                let Some(Value::Object(headers)) = fields.remove("headers") else {
                    return Value::Null;
                };
                let headers: serde_json::Map<_, _> = headers
                    .into_iter()
                    .map(|(id, row)| {
                        let row = project_item(row);
                        let row = if row.is_null() {
                            json!({"error": "message could not be read"})
                        } else {
                            row
                        };
                        (id, row)
                    })
                    .collect();
                json!({"headers": headers, "total": fields["total"], "failed": fields["failed"]})
            }
            Value::Object(fields) if op == "listFolders" => {
                let Some(columns) = fields.get("columns").and_then(Value::as_array) else {
                    return Value::Null;
                };
                let Some(index) = columns.iter().position(|column| column == "accountId") else {
                    return Value::Null;
                };
                let Some(rows) = fields.get("rows").and_then(Value::as_array) else {
                    return Value::Null;
                };
                let rows: Vec<_> = rows
                    .iter()
                    .filter(|row| {
                        row.get(index)
                            .and_then(Value::as_str)
                            .is_some_and(|id| self.accounts.contains(id))
                    })
                    .collect();
                json!({"columns": columns, "rows": rows})
            }
            Value::Array(items) => project_list(items),
            Value::Object(mut fields) if matches!(op, "searchMessages" | "getRecentMessages") => {
                fields.retain(|key, value| match key.as_str() {
                    "messages" => value.is_array(),
                    "hasMore" => value.is_boolean(),
                    "count" | "totalMatches" | "offset" | "limit" => value.is_number(),
                    _ => false,
                });
                if let Some(Value::Array(items)) = fields.remove("messages") {
                    fields.insert("messages".into(), project_list(items));
                }
                Value::Object(fields)
            }
            value => project_item(value),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_and_permission_matrix() {
        use Permission::*;
        let permissions = [Read, Move, Tag, Trash, Send, Filters, Rules, Accounts];
        for access in [Access::Off, Access::Headers, Access::Full] {
            for configured in [None].into_iter().chain(permissions.map(Some)) {
                let policy = AccountPolicy {
                    access,
                    permissions: configured.into_iter().collect(),
                };
                for permission in permissions {
                    assert_eq!(
                        policy.allows(permission),
                        configured == Some(permission)
                            && (access == Access::Full
                                || access == Access::Headers && permission == Read),
                        "{access:?} with {configured:?} for {permission:?}"
                    );
                }
            }
        }
    }
}
