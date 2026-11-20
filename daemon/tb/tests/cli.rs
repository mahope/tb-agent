//! Snapshot tests of the CLI's JSON output against a fake extension.

mod common;

use common::{Env, command_for, run};
use serde_json::{Value, json};
use tb_testkit::{BIG_BODY_BYTES, BIG_MESSAGE, INBOX, PERSONAL_INBOX, TRASH, TestDaemon};

fn audited_json(out: common::Output) -> Value {
    let mut result = out.json();
    let audit_id = result["auditId"]
        .as_str()
        .expect("write returns an audit ID");
    assert_eq!(audit_id.len(), 64);
    assert!(audit_id.bytes().all(|b| b.is_ascii_hexdigit()));
    result["auditId"] = json!("<audit-id>");
    result
}

fn public_call(env: &Env) -> (String, Value) {
    let (op, mut params) = env.last_call();
    let params_map = params.as_object_mut().unwrap();
    assert!(params_map.remove("_scope").is_some());
    params_map.remove("_expected");
    (op, params)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accounts() {
    let env = Env::new().await;
    let out = env.tb(&["accounts"]).await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    insta::assert_json_snapshot!("accounts", out.json());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn folders() {
    let env = Env::new().await;
    insta::assert_json_snapshot!("folders_all", env.tb(&["folders"]).await.json());
    insta::assert_json_snapshot!(
        "folders_account2",
        env.tb(&["folders", "--account", "account2"]).await.json()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search() {
    let env = Env::new().await;
    insta::assert_json_snapshot!("search_report", env.tb(&["search", "report"]).await.json());
    insta::assert_json_snapshot!(
        "search_unread_in_inbox",
        env.tb(&["search", "", "--folder", INBOX, "--unread"])
            .await
            .json()
    );
    insta::assert_json_snapshot!(
        "search_paged",
        env.tb(&["search", "", "--limit", "2", "--offset", "1"])
            .await
            .json()
    );
    let (op, params) = public_call(&env);
    assert_eq!(op, "searchMessages");
    assert_eq!(params, json!({"query": "", "maxResults": 2, "offset": 1}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recent() {
    let env = Env::new().await;
    let out = env.tb(&["recent", "--days", "30", "--limit", "10"]).await;
    assert_eq!(out.json().as_array().unwrap().len(), 4);
    assert_eq!(
        public_call(&env),
        (
            "getRecentMessages".into(),
            json!({"daysBack": 30, "maxResults": 10})
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_message() {
    let env = Env::new().await;
    let out = env.tb(&["get", "m1@example.com", "--folder", INBOX]).await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    insta::assert_json_snapshot!("get_message", out.json());

    let raw = env
        .tb(&["get", "m2@example.com", "--folder", INBOX, "--raw"])
        .await
        .json();
    insta::assert_json_snapshot!("get_message_raw", raw);
    assert_eq!(env.last_call().1["rawSource"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_message_that_is_streamed_arrives_whole() {
    let env = Env::new().await;
    let out = env.tb(&["get", BIG_MESSAGE, "--folder", INBOX]).await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    let msg = out.json();
    assert_eq!(msg["body"].as_str().unwrap().len(), BIG_BODY_BYTES);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_many_and_headers() {
    let env = Env::new().await;
    let many = env
        .tb(&[
            "get-many",
            "m1@example.com",
            "m2@example.com",
            "--folder",
            INBOX,
        ])
        .await
        .json();
    assert_eq!(many.as_array().unwrap().len(), 2);
    insta::assert_json_snapshot!(
        "headers_single",
        env.tb(&["headers", "m3@example.com", "--folder", PERSONAL_INBOX])
            .await
            .json()
    );
    let batch = env
        .tb(&[
            "headers",
            "m1@example.com",
            "m2@example.com",
            "--folder",
            INBOX,
        ])
        .await
        .json();
    assert_eq!(batch.as_array().unwrap().len(), 2);
    assert_eq!(env.last_call().0, "batchGetMessageHeaders");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutations_send_the_expected_parameters() {
    let env = Env::new().await;

    let out = env
        .tb(&[
            "mark",
            "m1@example.com",
            "--folder",
            INBOX,
            "--read",
            "--flag",
        ])
        .await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    insta::assert_json_snapshot!("mark", audited_json(out));
    assert_eq!(
        public_call(&env),
        (
            "updateMessage".into(),
            json!({"messageId": "m1@example.com", "folderPath": INBOX, "read": true, "flagged": true})
        )
    );

    env.tb(&["mark", "m1@example.com", "--folder", INBOX, "--unread"])
        .await;
    assert_eq!(env.last_call().1["read"], false);

    env.tb(&[
        "tag",
        "m1@example.com",
        "m2@example.com",
        "--folder",
        INBOX,
        "--add",
        "$label1",
        "--remove",
        "x",
    ])
    .await;
    assert_eq!(
        public_call(&env).1,
        json!({"messageIds": ["m1@example.com", "m2@example.com"], "folderPath": INBOX, "addTags": ["$label1"], "removeTags": ["x"]})
    );

    env.tb(&["move", "m1@example.com", "--folder", INBOX, "--to", TRASH])
        .await;
    assert_eq!(env.last_call().1["moveTo"], TRASH);

    insta::assert_json_snapshot!(
        "trash",
        audited_json(
            env.tb(&["trash", "m2@example.com", BIG_MESSAGE, "--folder", INBOX])
                .await
        )
    );
    assert_eq!(env.last_call().1["trash"], true);

    insta::assert_json_snapshot!(
        "delete",
        audited_json(
            env.tb(&["delete", "m1@example.com", "--folder", TRASH])
                .await
        )
    );
    assert_eq!(env.last_call().0, "deleteMessages");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compose_commands() {
    let env = Env::new().await;
    // Any existing file will do; the CLI must hand the extension its absolute path.
    let attachment = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let attachment_abs = std::fs::canonicalize(&attachment)
        .unwrap()
        .to_string_lossy()
        .into_owned();

    insta::assert_json_snapshot!(
        "draft",
        audited_json(
            env.tb(&[
                "draft",
                "--to",
                "a@example.com",
                "--subject",
                "Hi",
                "--body",
                "Text",
                "--attach",
                attachment.to_str().unwrap()
            ])
            .await
        )
    );
    assert_eq!(
        public_call(&env),
        (
            "saveDraft".into(),
            json!({"to": "a@example.com", "subject": "Hi", "body": "Text", "attachments": [attachment_abs], "from": "id1"})
        )
    );

    insta::assert_json_snapshot!(
        "send_review",
        audited_json(
            env.tb(&[
                "send",
                "--to",
                "a@example.com",
                "--subject",
                "Hi",
                "--body",
                "Text"
            ])
            .await
        )
    );
    assert!(
        env.last_call().1.get("skipReview").is_none(),
        "review is the default"
    );

    insta::assert_json_snapshot!(
        "send_direct",
        audited_json(
            env.tb(&[
                "send",
                "--to",
                "a@example.com",
                "--subject",
                "Hi",
                "--body",
                "Text",
                "--yes"
            ])
            .await
        )
    );
    assert_eq!(env.last_call().1["skipReview"], true);

    let out = env
        .tb(&[
            "reply",
            "m1@example.com",
            "--folder",
            INBOX,
            "--body",
            "Thanks",
            "--all",
            "--draft",
        ])
        .await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(env.last_call().1["replyAll"], true);
    assert_eq!(env.last_call().1["saveAsDraft"], true);

    env.tb(&[
        "forward",
        "m1@example.com",
        "--folder",
        INBOX,
        "--to",
        "b@example.com",
    ])
    .await;
    assert_eq!(env.last_call().0, "forwardMessage");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_passes_raw_parameters() {
    let env = Env::new().await;
    let out = env
        .tb(&["call", "searchMessages", "--params", r#"{"query":"lunch"}"#])
        .await;
    assert_eq!(out.json()[0]["id"], "m2@example.com");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_reports_extension_capabilities() {
    let env = Env::new().await;
    let mut status = env.tb(&["status"]).await.json();
    status["daemonVersion"] = Value::Null;
    insta::assert_json_snapshot!("status", status);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ops_lists_every_operation_without_a_daemon() {
    let dir = std::env::temp_dir().join("tb-no-daemon");
    let out = run(command_for(&dir.join("missing.json")), &["ops"]).await;
    assert_eq!(out.code, 0);
    let names: Vec<String> = out
        .json()
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["name"].as_str().unwrap().to_owned())
        .collect();
    for expected in [
        "listAccounts",
        "searchMessages",
        "getMessage",
        "updateMessage",
        "deleteMessages",
        "saveDraft",
        "sendMail",
    ] {
        assert!(names.iter().any(|n| n == expected), "missing {expected}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operation_errors_exit_1_with_json_on_stderr() {
    let env = Env::new().await;
    let out = env.tb(&["get", "nope", "--folder", INBOX]).await;
    assert_eq!(out.code, 1);
    assert!(out.stdout.is_empty());
    insta::assert_json_snapshot!("error_not_found", out.error_json());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_errors_exit_2() {
    let env = Env::new().await;
    assert_eq!(env.tb(&["get"]).await.code, 2);
    let out = env.tb(&["mark", "m1", "--folder", INBOX]).await;
    assert_eq!(out.code, 2);
    assert_eq!(out.error_json()["error"]["code"], "bad_request");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_daemon_exits_3() {
    let dir = std::env::temp_dir().join("tb-no-daemon");
    let out = run(command_for(&dir.join("missing.json")), &["accounts"]).await;
    assert_eq!(out.code, 3);
    assert_eq!(out.error_json()["error"]["code"], "daemon_unavailable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_token_in_connection_file_is_rejected() {
    let env = Env::new().await;
    let mut info = env.daemon.info().clone();
    info.token = "0".repeat(64);
    let bad = env.daemon.dir.join("bad.json");
    info.write(&bad).unwrap();
    let out = run(command_for(&bad), &["accounts"]).await;
    assert_eq!(out.code, 3);
    assert!(
        out.stdout.is_empty(),
        "no data may be returned without the token"
    );
    assert!(
        out.error_json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("401")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_extension_connected_gives_a_clear_error() {
    let daemon = TestDaemon::start().await.unwrap();
    let out = run(command_for(&daemon.connection_file()), &["accounts"]).await;
    assert_eq!(out.code, 1);
    assert_eq!(out.error_json()["error"]["code"], "extension_unavailable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_attachment_fails_before_sending() {
    let env = Env::new().await;
    let out = env
        .tb(&[
            "send",
            "--to",
            "a@example.com",
            "--subject",
            "Hi",
            "--attach",
            "does-not-exist.pdf",
        ])
        .await;
    assert_ne!(out.code, 0);
    assert!(out.stderr.contains("cannot attach"), "{}", out.stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_restores_folders_and_flags() {
    let env = Env::new().await;
    let id = "m1@example.com";
    for args in [
        vec!["move", id, "--folder", INBOX, "--to", TRASH],
        vec!["tag", id, "--folder", INBOX, "--add", "$label1"],
        vec!["mark", id, "--folder", INBOX, "--read", "--flag"],
        vec!["trash", id, "--folder", INBOX],
    ] {
        let result = env.tb(&args).await.json();
        let audit_id = result["auditId"].as_str().unwrap();
        let log = std::fs::read_to_string(env.daemon.dir.join("audit.jsonl")).unwrap();
        let intent = log
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|entry| entry["auditId"] == audit_id && entry["event"] == "intent")
            .unwrap();
        assert_eq!(intent["accounts"], json!(["account1"]));
        assert_eq!(
            intent["previous"],
            json!([{
                "messageId": id, "folderPath": INBOX, "read": false, "flagged": false, "tags": []
            }])
        );
        assert!(intent["timeMs"].is_u64());
        if args[0] == "move" || args[0] == "trash" {
            assert_eq!(
                env.tb(&["headers", id, "--folder", TRASH]).await.json()["id"],
                id
            );
        }
        let undo = env.tb(&["undo", audit_id]).await;
        assert_eq!(undo.code, 0, "{}", undo.stderr);
        let header = env.tb(&["headers", id, "--folder", INBOX]).await.json();
        assert_eq!(header["id"], id);
        assert_eq!(header["read"], false);
        assert_eq!(header["flagged"], false);
        assert_eq!(header["tags"], json!([]));
        assert_eq!(
            env.tb(&["undo", audit_id]).await.error_json()["error"]["code"],
            "undo_unavailable"
        );
    }
}
