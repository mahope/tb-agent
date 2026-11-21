use serde_json::{Value, json};
use tb_client::Client;
use tb_testkit::{FakeConfig, FakeExtension, INBOX, PERSONAL_INBOX, TestDaemon};

#[tokio::test]
async fn headers_default_hides_body_and_send_needs_permission() {
    let daemon = TestDaemon::with_policy(Some(
        r#"
[accounts.account1]
access = "full"
permissions = ["read"]
"#,
    ))
    .await
    .unwrap();
    let extension = FakeExtension::connect(daemon.extension_info(), FakeConfig::default())
        .await
        .unwrap();
    let client = Client::connect(daemon.info(), "policy test").await.unwrap();
    let header = client
        .request(
            "getMessage",
            json!({
                "messageId": "m3@example.com",
                "folderPath": PERSONAL_INBOX,
                "rawSource": true,
                "saveAttachments": true
            }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(header["subject"], "Invoice 2026-11");
    for field in ["body", "attachments", "rawSource", "recipients"] {
        assert!(header.get(field).is_none());
    }
    let calls = extension.calls();
    assert_eq!(calls[0].0, "getMessageHeaders");
    assert!(calls[0].1.get("saveAttachments").is_none());

    let err = client
        .request(
            "sendMail",
            json!({
                "from": "id1", "to": "a@example.com", "subject": "test", "body": "test"
            }),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.to_body().code, "permission_denied");
    assert_eq!(extension.calls().len(), 1);

    let full = client
        .request(
            "getMessage",
            json!({
                "messageId": "m1@example.com", "folderPath": INBOX
            }),
            None,
        )
        .await
        .unwrap();
    assert!(full["body"].is_string());
    let _ = client.request("status", Value::Null, None).await.unwrap();
}

#[tokio::test]
async fn off_accounts_cannot_be_queried_or_used_as_destinations() {
    let daemon = TestDaemon::with_policy(Some(
        r#"
[accounts.account1]
access = "full"
permissions = ["read", "move", "send"]
[accounts.account2]
access = "off"
permissions = ["read", "move", "send"]
"#,
    ))
    .await
    .unwrap();
    let extension = FakeExtension::connect(daemon.extension_info(), FakeConfig::default())
        .await
        .unwrap();
    let client = Client::connect(daemon.info(), "policy test").await.unwrap();
    for (op, params) in [
        (
            "getMessage",
            json!({"messageId": "m3@example.com", "folderPath": PERSONAL_INBOX,
            "accountId": "account1", "_scope": {"accounts": ["account2"]}}),
        ),
        (
            "updateMessage",
            json!({"messageId": "m1@example.com", "folderPath": INBOX, "moveTo": PERSONAL_INBOX}),
        ),
        (
            "sendMail",
            json!({"from": "me@home.example.net", "to": "a@example.com", "subject": "test", "body": "test"}),
        ),
    ] {
        let err = client.request(op, params, None).await.unwrap_err();
        assert_eq!(err.to_body().code, "permission_denied");
    }
    assert!(extension.calls().is_empty());
    let result = client
        .request(
            "searchMessages",
            json!({
                "query": "", "countOnly": true, "_scope": {"accounts": ["account2"]}
            }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(result["count"], 3);
    assert_eq!(
        extension.calls()[0].1["_scope"]["accounts"],
        json!(["account1"])
    );
}

#[tokio::test]
async fn streamed_mixed_reads_project_only_restricted_messages() {
    let daemon = TestDaemon::with_policy(Some(
        r#"
[accounts.account1]
access = "full"
permissions = ["read"]
"#,
    ))
    .await
    .unwrap();
    let extension = FakeExtension::connect(
        daemon.extension_info(),
        FakeConfig {
            stream_threshold: 25,
            chunk_size: 20,
            include_sensitive_batch_headers: true,
            ..FakeConfig::default()
        },
    )
    .await
    .unwrap();
    let client = Client::connect(daemon.info(), "policy test").await.unwrap();
    let results = client.request("getMessages", json!({"messages": [
        {"messageId": "m1@example.com", "folderPath": INBOX},
        {"messageId": "m3@example.com", "folderPath": PERSONAL_INBOX, "saveAttachments": true}
    ]}), None).await.unwrap();
    assert!(results[0]["body"].is_string());
    assert_eq!(
        results[1],
        json!({
            "id": "m3@example.com", "author": "Billing <billing@example.org>",
            "subject": "Invoice 2026-11", "folderPath": PERSONAL_INBOX,
            "date": "2026-11-16T08:15:00.000Z", "read": false, "flagged": false, "tags": []
        })
    );
    assert!(
        extension.calls()[0].1["messages"][1]
            .get("saveAttachments")
            .is_none()
    );
}

#[tokio::test]
async fn client_token_cannot_register_an_extension() {
    let daemon = TestDaemon::start().await.unwrap();
    assert!(
        FakeExtension::connect(daemon.info(), FakeConfig::default())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn undo_refuses_to_overwrite_a_later_change() {
    let (daemon, _extension) = TestDaemon::with_extension(FakeConfig::default())
        .await
        .unwrap();
    let client = Client::connect(daemon.info(), "undo test").await.unwrap();
    let id = "m1@example.com";
    let original = client
        .request(
            "updateMessage",
            json!({
                "messageId": id, "folderPath": INBOX, "read": true
            }),
            None,
        )
        .await
        .unwrap();
    client
        .request(
            "updateMessage",
            json!({
                "messageId": id, "folderPath": INBOX, "flagged": true
            }),
            None,
        )
        .await
        .unwrap();
    let err = client
        .request("undo", json!({"auditId": original["auditId"]}), None)
        .await
        .unwrap_err();
    assert_eq!(err.to_body().code, "conflict");
    let current = client
        .request(
            "getMessageHeaders",
            json!({
                "messageId": id, "folderPath": INBOX
            }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(current["read"], true);
    assert_eq!(current["flagged"], true);
    client
        .request(
            "updateMessage",
            json!({
                "messageId": id, "folderPath": INBOX, "flagged": false
            }),
            None,
        )
        .await
        .unwrap();
    client
        .request("undo", json!({"auditId": original["auditId"]}), None)
        .await
        .unwrap();
    let restored = client
        .request(
            "getMessageHeaders",
            json!({
                "messageId": id, "folderPath": INBOX
            }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(restored["read"], false);
}

#[tokio::test]
async fn moving_to_registered_trash_requires_trash_permission() {
    let daemon = TestDaemon::with_policy(Some(
        "[accounts.account1]\naccess = 'full'\npermissions = ['move']",
    ))
    .await
    .unwrap();
    let extension = FakeExtension::connect(daemon.extension_info(), FakeConfig::default())
        .await
        .unwrap();
    let client = Client::connect(daemon.info(), "policy test").await.unwrap();
    let err = client
        .request(
            "updateMessage",
            json!({
                "messageId": "m1@example.com", "folderPath": INBOX, "moveTo": tb_testkit::TRASH
            }),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.to_body().code, "permission_denied");
    assert!(extension.calls().is_empty());
}

#[tokio::test]
async fn undo_refuses_a_replacement_copy() {
    let (daemon, _extension) = TestDaemon::with_extension(FakeConfig::default())
        .await
        .unwrap();
    let client = Client::connect(daemon.info(), "undo test").await.unwrap();
    let id = "m1@example.com";
    let original = client
        .request(
            "updateMessage",
            json!({
                "messageId": id, "folderPath": INBOX, "moveTo": tb_testkit::TRASH
            }),
            None,
        )
        .await
        .unwrap();
    client
        .request(
            "updateMessage",
            json!({
                "messageId": id, "folderPath": tb_testkit::TRASH, "moveTo": tb_testkit::DRAFTS
            }),
            None,
        )
        .await
        .unwrap();
    client
        .request(
            "updateMessage",
            json!({
                "messageId": id, "folderPath": tb_testkit::DRAFTS, "copyTo": tb_testkit::TRASH
            }),
            None,
        )
        .await
        .unwrap();
    let err = client
        .request("undo", json!({"auditId": original["auditId"]}), None)
        .await
        .unwrap_err();
    assert_eq!(err.to_body().code, "conflict");
}

#[tokio::test]
async fn restricted_batches_keep_error_rows_in_place() {
    for batch_envelope in [false, true] {
        let daemon = TestDaemon::with_policy(Some(
            "[accounts.account1]\naccess = 'full'\npermissions = ['read']",
        ))
        .await
        .unwrap();
        let _extension = FakeExtension::connect(
            daemon.extension_info(),
            FakeConfig {
                batch_envelope,
                ..FakeConfig::default()
            },
        )
        .await
        .unwrap();
        let client = Client::connect(daemon.info(), "policy test").await.unwrap();
        let batch = client
            .request(
                "getMessages",
                json!({"messages": [
                    {"messageId": "missing@example.com", "folderPath": PERSONAL_INBOX},
                    {"messageId": "m1@example.com", "folderPath": INBOX}
                ]}),
                None,
            )
            .await
            .unwrap();
        let rows = if batch_envelope {
            assert_eq!(batch["requested"], 2);
            assert_eq!(batch["failed"], 1);
            &batch["messages"]
        } else {
            &batch
        };
        assert_eq!(rows.as_array().unwrap().len(), 2);
        assert!(rows[0].get("error").is_some());
        assert_eq!(rows[1]["id"], "m1@example.com");
        assert!(rows[1]["body"].is_string());
    }
}

#[tokio::test]
async fn undo_restores_deleted_drafts_but_not_permanent_deletions() {
    let (daemon, _extension) = TestDaemon::with_extension(FakeConfig::default())
        .await
        .unwrap();
    let client = Client::connect(daemon.info(), "undo test").await.unwrap();
    let id = "m1@example.com";
    client
        .request(
            "updateMessage",
            json!({
                "messageId": id, "folderPath": INBOX, "moveTo": tb_testkit::DRAFTS
            }),
            None,
        )
        .await
        .unwrap();
    let deleted = client
        .request(
            "deleteMessages",
            json!({
                "messageIds": [id], "folderPath": tb_testkit::DRAFTS
            }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(deleted["movedToTrash"], true);
    let trashed = client
        .request(
            "getMessageHeaders",
            json!({
                "messageId": id, "folderPath": tb_testkit::TRASH
            }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(trashed["id"], id);
    client
        .request("undo", json!({"auditId": deleted["auditId"]}), None)
        .await
        .unwrap();
    let restored = client
        .request(
            "getMessageHeaders",
            json!({
                "messageId": id, "folderPath": tb_testkit::DRAFTS
            }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(restored["id"], id);

    client
        .request(
            "deleteMessages",
            json!({
                "messageIds": [id], "folderPath": tb_testkit::DRAFTS
            }),
            None,
        )
        .await
        .unwrap();
    let permanent = client
        .request(
            "deleteMessages",
            json!({
                "messageIds": [id], "folderPath": tb_testkit::TRASH
            }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(permanent["movedToTrash"], false);
    let error = client
        .request("undo", json!({"auditId": permanent["auditId"]}), None)
        .await
        .unwrap_err();
    assert_eq!(error.to_body().code, "undo_unavailable");
}

#[tokio::test]
async fn timed_out_write_can_still_be_confirmed_and_undone() {
    use std::time::Duration;

    let (daemon, _extension) = TestDaemon::with_extension(FakeConfig {
        write_reply_delay: Duration::from_millis(500),
        ..FakeConfig::default()
    })
    .await
    .unwrap();
    let client = Client::connect(daemon.info(), "undo test").await.unwrap();
    let id = "m1@example.com";
    let error = client
        .request(
            "updateMessage",
            json!({
                "messageId": id, "folderPath": INBOX, "trash": true
            }),
            Some(Duration::from_millis(200)),
        )
        .await
        .unwrap_err();
    assert_eq!(error.to_body().code, "timeout");

    // The fake answers this read after sending the delayed write reply.
    let trashed = client
        .request(
            "getMessageHeaders",
            json!({
                "messageId": id, "folderPath": tb_testkit::TRASH
            }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(trashed["id"], id);
    let log = std::fs::read_to_string(daemon.dir.join("audit.jsonl")).unwrap();
    let mut records = log
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap());
    let intent = records.next().unwrap();
    let finished = records.next().unwrap();
    assert_eq!(finished["auditId"], intent["auditId"]);
    assert_eq!(finished["success"], true);
    client
        .request("undo", json!({"auditId": intent["auditId"]}), None)
        .await
        .unwrap();
    let restored = client
        .request(
            "getMessageHeaders",
            json!({
                "messageId": id, "folderPath": INBOX
            }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(restored["id"], id);
}

#[tokio::test]
async fn audited_send_preserves_explicit_extension_error() {
    let (daemon, _extension) = TestDaemon::with_extension(FakeConfig {
        reject_send: true,
        ..FakeConfig::default()
    })
    .await
    .unwrap();
    let client = Client::connect(daemon.info(), "audit test").await.unwrap();
    let error = client
        .request(
            "sendMail",
            json!({
                "from": "id1", "to": "a@example.com", "subject": "test", "body": "test"
            }),
            None,
        )
        .await
        .unwrap_err()
        .to_body();
    assert_eq!(error.code, "send_rejected");
    assert_eq!(error.message, "message rejected before sending");
    let log = std::fs::read_to_string(daemon.dir.join("audit.jsonl")).unwrap();
    let finished: Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
    assert_eq!(finished["event"], "finished");
    assert_eq!(finished["success"], false);
}

#[tokio::test]
async fn extension_operations_cannot_bypass_account_permissions() {
    let daemon = TestDaemon::with_policy(Some(
        r#"
[accounts.account1]
access = "full"
permissions = ["read"]
"#,
    ))
    .await
    .unwrap();
    let extension = FakeExtension::connect(
        daemon.extension_info(),
        FakeConfig {
            modules: vec![
                "core".into(),
                "experiment.filters".into(),
                "experiment.folders".into(),
                "experiment.account-setup".into(),
                "experiment.advanced-mail".into(),
            ],
            ..FakeConfig::default()
        },
    )
    .await
    .unwrap();
    let client = Client::connect(daemon.info(), "extension policy test")
        .await
        .unwrap();
    let compose =
        json!({"from": "id1", "to": "a@example.invalid", "subject": "test", "body": "test"});
    let message = json!({"messageId": "m1@example.com", "folderPath": INBOX});
    for (op, params) in [
        ("sendMail", compose.clone()),
        ("saveDraft", compose.clone()),
        ("dryRunCompose", compose),
        (
            "replyToMessage",
            json!({"messageId": message["messageId"], "folderPath": INBOX, "body": "reply"}),
        ),
        (
            "forwardMessage",
            json!({"messageId": message["messageId"], "folderPath": INBOX, "to": "a@example.invalid"}),
        ),
        (
            "deleteMessages",
            json!({"messageIds": [message["messageId"]], "folderPath": INBOX}),
        ),
        (
            "updateMessage",
            json!({"messageId": message["messageId"], "folderPath": INBOX, "trash": true}),
        ),
        (
            "updateMessage",
            json!({"messageId": message["messageId"], "folderPath": INBOX, "read": false}),
        ),
        (
            "updateMessage",
            json!({"messageId": message["messageId"], "folderPath": INBOX, "addTags": ["test"]}),
        ),
        (
            "updateMessage",
            json!({"messageId": message["messageId"], "folderPath": INBOX, "moveTo": PERSONAL_INBOX}),
        ),
        (
            "updateMessage",
            json!({"messageId": message["messageId"], "folderPath": INBOX, "copyTo": PERSONAL_INBOX}),
        ),
        ("displayMessage", message),
        ("emptyTrash", json!({})),
        ("openAccountSetup", json!({})),
    ] {
        let denied = client
            .request(op, params, None)
            .await
            .unwrap_err()
            .to_body();
        assert_eq!(denied.code, "permission_denied", "{op}");
    }
    assert!(extension.calls().is_empty());
}
