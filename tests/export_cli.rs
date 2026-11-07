use rusqlite::Connection;
use serde_json::{json, Value};
use std::{fs, process::Command};

#[test]
fn full_export_preserves_wal_rows_compression_identity_and_explicit_cap() {
    let root = std::env::temp_dir().join(format!("wx-export-cli-{}", std::process::id()));
    fs::create_dir_all(root.join("account/db_storage/message")).unwrap();
    fs::create_dir_all(root.join("account/db_storage/contact")).unwrap();
    let db = root.join("account/db_storage");
    let key = "11".repeat(32);
    let contact = Connection::open(db.join("contact/contact.db")).unwrap();
    contact.execute_batch(&format!("PRAGMA key=\"x'{key}'\";CREATE TABLE contact(username TEXT,nick_name TEXT,remark TEXT,verify_flag INTEGER);INSERT INTO contact VALUES('group@chatroom','Group','',0),('wxid_alice','Alice','',0),('wxid_self','Self','',0);")).unwrap();
    let conn = Connection::open(db.join("message/message_0.db")).unwrap();
    conn.execute_batch(&format!("PRAGMA key=\"x'{key}'\"; PRAGMA journal_mode=WAL;PRAGMA wal_autocheckpoint=0; CREATE TABLE Name2Id(user_name TEXT);INSERT INTO Name2Id VALUES('group@chatroom'),('wxid_alice'),('wxid_self');")).unwrap();
    let table = format!("Msg_{:x}", md5::compute("group@chatroom"));
    let private = format!("Msg_{:x}", md5::compute("wxid_alice"));
    for t in [&table, &private] {
        conn.execute_batch(&format!("CREATE TABLE {t}(local_id INTEGER,server_id INTEGER,create_time INTEGER,sort_seq INTEGER,local_type INTEGER,real_sender_id INTEGER,message_content BLOB,WCDB_CT_message_content INTEGER,future_extra BLOB);")).unwrap();
    }
    let text = "说明:\ncomputer use https://huggingface.co/test";
    let compressed = zstd::stream::encode_all(text.as_bytes(), 1).unwrap();
    for id in 1..=601 {
        conn.execute(
            &format!("INSERT INTO {table} VALUES(?1,?2,1793980800,?1,1,2,?3,4,x'00FF')"),
            rusqlite::params![id, 9007199254740991i64 + id, &compressed],
        )
        .unwrap();
    }
    for (id, sender) in [(1, 2), (2, 3)] {
        conn.execute(
            &format!("INSERT INTO {private} VALUES(?1,?1,1793980800,?1,1,?2,'hello',0,x'FF')"),
            rusqlite::params![id, sender],
        )
        .unwrap();
    }
    fs::write(
        root.join("keys.json"),
        json!({"contact/contact.db":key,"message/message_0.db":key}).to_string(),
    )
    .unwrap();
    fs::write(
        root.join("config.json"),
        json!({"db_dir":db,"keys_file":"keys.json"}).to_string(),
    )
    .unwrap();
    let run = |extra: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_wx"))
            .current_dir(&root)
            .args([
                "export",
                "--all-chats",
                "--since",
                "2026-11-07",
                "--until",
                "2026-11-08",
                "--format",
                "json",
            ])
            .args(extra)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };
    let full = run(&[]);
    let rows = full["messages"].as_array().unwrap();
    assert_eq!(rows.len(), 603);
    assert_eq!(full["meta"]["truncated"], false);
    let group = rows.iter().find(|r| r["chat_type"] == "group").unwrap();
    assert_eq!(group["content"], text);
    assert!(group["server_id"].is_string());
    assert_eq!(group["row"]["future_extra"]["base64"], "AP8=");
    let private_rows: Vec<_> = rows
        .iter()
        .filter(|r| r["chat_username"] == "wxid_alice")
        .collect();
    assert_eq!(private_rows.len(), 2);
    assert_ne!(
        private_rows[0]["sender_username"],
        private_rows[1]["sender_username"]
    );
    let capped = run(&["--limit", "10"]);
    assert_eq!(capped["messages"].as_array().unwrap().len(), 10);
    assert_eq!(capped["meta"]["truncated"], true);
    let filtered = run(&["--domain", "huggingface.co", "--keyword", "computer"]);
    assert_eq!(filtered["messages"].as_array().unwrap().len(), 601);
    drop(conn);
    drop(contact);
    fs::remove_dir_all(root).unwrap();
}
