//! 微信原始采集出口：完整SQL列、只读事务、无数量上限，不做内容筛选。
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::{FixedOffset, NaiveDate, TimeZone};
use clap::Args;
use rusqlite::{types::ValueRef, Connection};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

#[derive(Args)]
pub struct RawExportArgs {
    /// db_storage目录，显式配置避免误用另一个账号
    #[arg(long)]
    pub db_dir: PathBuf,
    /// 已有all_keys.json；不扫描进程、不注入微信
    #[arg(long)]
    pub keys_file: PathBuf,
    /// 精确微信username（不是昵称），与--table二选一
    #[arg(long, required_unless_present = "table", conflicts_with = "table")]
    pub chat: Option<String>,
    /// 精确Msg_<md5>表名
    #[arg(long, required_unless_present = "chat")]
    pub table: Option<String>,
    #[arg(long)]
    pub since: String,
    /// 排他上界；2026-10-01表示整个9月
    #[arg(long)]
    pub until_exclusive: String,
    /// 明确日期所在时区，避免Mac与VPS的本地时区差异
    #[arg(long, default_value = "+08:00", allow_hyphen_values = true)]
    pub timezone_offset: String,
    /// 稳定来源命名空间，影响消息ID，不包含密钥
    #[arg(long)]
    pub source_id: String,
    /// 输出目录；拒绝覆盖现有快照
    #[arg(short, long)]
    pub output: PathBuf,
}

struct Shard {
    id: String,
    conn: Connection,
    columns: Vec<String>,
    names: HashMap<i64, String>,
    expected: i64,
}
struct Message {
    shard: usize,
    row: Map<String, Value>,
    role: &'static str,
}

fn raw_value(value: ValueRef<'_>) -> Result<Value> {
    Ok(match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(n) if !(-9007199254740991..=9007199254740991).contains(&n) => {
            json!({"integer":n.to_string()})
        }
        ValueRef::Integer(n) => json!(n),
        ValueRef::Real(n) if !n.is_finite() => json!({"real_bits":n.to_bits().to_string()}),
        ValueRef::Real(n) => json!(n),
        ValueRef::Text(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) => json!(text),
            Err(_) => json!({"base64":STANDARD.encode(bytes),"sqlite_type":"text"}),
        },
        ValueRef::Blob(bytes) => json!({"base64":STANDARD.encode(bytes)}),
    })
}
fn number(value: &Value) -> Result<i64> {
    value.as_i64().map(Ok).unwrap_or_else(|| {
        value
            .get("integer")
            .and_then(Value::as_str)
            .context("原始字段不是整数")?
            .parse()
            .context("原始整数编码无效")
    })
}
fn date_timestamp(day: &str, offset: &str) -> Result<i64> {
    let zone: FixedOffset = offset.parse().context("时区须为+08:00或-05:00等固定偏移")?;
    let date = NaiveDate::parse_from_str(day, "%Y-%m-%d").context("日期须为YYYY-MM-DD")?;
    Ok(zone
        .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .context("日期无法映射到指定时区")?
        .timestamp())
}
fn rows(
    conn: &Connection,
    table: &str,
    predicate: &str,
    parameters: &[&dyn rusqlite::ToSql],
) -> Result<Vec<Map<String, Value>>> {
    let sql = format!(
        "SELECT * FROM \"{table}\" WHERE {predicate} ORDER BY create_time, sort_seq, local_id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let names = stmt
        .column_names()
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>();
    let mut cursor = stmt.query(parameters)?;
    let mut result = Vec::new();
    while let Some(row) = cursor.next()? {
        let mut item = Map::new();
        for (index, name) in names.iter().enumerate() {
            item.insert(name.clone(), raw_value(row.get_ref(index)?)?);
        }
        result.push(item);
    }
    Ok(result)
}
fn reference(row: &Map<String, Value>) -> Result<Option<String>> {
    if number(&row["local_type"])? & 0xffffffff != 49 {
        return Ok(None);
    }
    let value = &row["message_content"];
    let bytes = if let Some(encoded) = value.get("base64").and_then(Value::as_str) {
        STANDARD.decode(encoded)?
    } else {
        value.as_str().unwrap_or_default().as_bytes().to_vec()
    };
    let bytes = if row.get("WCDB_CT_message_content").and_then(Value::as_i64) == Some(4) {
        zstd::stream::decode_all(bytes.as_slice()).context("引用正文zstd解压失败")?
    } else {
        bytes
    };
    let text = std::str::from_utf8(&bytes).context("引用正文不是UTF-8")?;
    let Some(start) = text.find('<') else {
        return Ok(None);
    };
    let text = &text[start..];
    // 微信正文可由msg和msgsource两个并列XML片段组成。
    let fragment = format!("<source_fragments>{}</source_fragments>", text);
    let document = roxmltree::Document::parse(text)
        .or_else(|_| roxmltree::Document::parse(&fragment))
        .context("引用正文XML解析失败")?;
    Ok(document
        .descendants()
        .find(|node| node.has_tag_name("refermsg"))
        .and_then(|node| node.children().find(|child| child.has_tag_name("svrid")))
        .and_then(|node| node.text())
        .filter(|v| *v != "0" && !v.is_empty())
        .map(str::to_owned))
}
fn private_file(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}
fn sha_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let len = file.read(&mut buffer)?;
        if len == 0 {
            break;
        }
        digest.update(&buffer[..len]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub fn cmd_raw_export(args: RawExportArgs) -> Result<()> {
    let table = args.table.clone().unwrap_or_else(|| {
        format!(
            "Msg_{:x}",
            md5::compute(args.chat.as_deref().unwrap().as_bytes())
        )
    });
    if table.len() != 36
        || !table.starts_with("Msg_")
        || !table[4..].bytes().all(|c| c.is_ascii_hexdigit())
    {
        bail!("无效Msg表名");
    }
    let start = date_timestamp(&args.since, &args.timezone_offset)?;
    let end = date_timestamp(&args.until_exclusive, &args.timezone_offset)?;
    if start >= end {
        bail!("排他上界必须晚于起始日期");
    }
    if args.source_id.trim().is_empty() {
        bail!("source-id不得为空");
    }
    if args.output.exists() {
        bail!("输出目录已存在，拒绝覆盖快照");
    }
    let keys =
        crate::daemon::extract_keys(&serde_json::from_slice(&std::fs::read(&args.keys_file)?)?);
    // 显示名是联系人/群名片信息，不改写消息原始SQL字段。
    let mut contact_names = HashMap::new();
    let mut group_nicknames = HashMap::new();
    if let Some(key) = keys.get("contact/contact.db") {
        let contact_path = args.db_dir.join("contact/contact.db");
        let contact = crate::crypto::sqlcipher::open_encrypted_readonly(&contact_path, key)?;
        contact.execute_batch("BEGIN")?;
        let mut statement = contact.prepare("SELECT username,nick_name,remark FROM contact")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            ))
        })?;
        for row in rows {
            let (username, nick, remark) = row?;
            let display = if !remark.is_empty() {
                remark
            } else if !nick.is_empty() {
                nick
            } else {
                username.clone()
            };
            contact_names.insert(username, display);
        }
        let chat = args.chat.as_deref().or_else(|| {
            contact_names
                .keys()
                .find(|username| {
                    username.ends_with("@chatroom")
                        && format!("Msg_{:x}", md5::compute(username.as_bytes())) == table
                })
                .map(String::as_str)
        });
        if let Some(chat) = chat {
            group_nicknames =
                crate::daemon::query::load_group_nickname_map_from_conn(&contact, chat, None);
        }
    }
    let pattern = regex::Regex::new(r"^message_\d+\.db$")?;
    let mut disk = std::fs::read_dir(args.db_dir.join("message"))?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    disk.retain(|path| {
        path.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| pattern.is_match(n))
    });
    disk.sort();
    if disk.is_empty() {
        bail!("未找到message_N.db分片");
    }
    let mut shards = Vec::new();
    let mut absent = Vec::new();
    for path in disk {
        let id = format!(
            "message/{}",
            path.file_name()
                .unwrap()
                .to_str()
                .context("分片名不是UTF-8")?
        );
        let key = keys
            .get(&id)
            .with_context(|| format!("分片缺少密钥: {id}，未生成不完整快照"))?;
        let conn = crate::crypto::sqlcipher::open_encrypted_readonly(&path, key)
            .with_context(|| format!("分片无法只读打开: {id}"))?;
        conn.execute_batch("BEGIN")?;
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [&table],
            |row| row.get(0),
        )?;
        if !exists {
            absent.push(id);
            continue;
        }
        let columns = conn
            .prepare(&format!("PRAGMA table_info(\"{table}\")"))?
            .query_map([], |row| row.get(1))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        for column in [
            "local_id",
            "server_id",
            "create_time",
            "sort_seq",
            "local_type",
            "real_sender_id",
            "message_content",
            "source",
            "packed_info_data",
        ] {
            if !columns.iter().any(|v| v == column) {
                bail!("原始消息表缺少列: {column}");
            }
        }
        let names = conn
            .prepare("SELECT rowid,user_name FROM Name2Id")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<HashMap<i64, String>>>()?;
        let expected = conn.query_row(
            &format!("SELECT count(*) FROM \"{table}\" WHERE create_time>=?1 AND create_time<?2"),
            [start, end],
            |row| row.get(0),
        )?;
        shards.push(Shard {
            id,
            conn,
            columns,
            names,
            expected,
        });
    }
    if shards.is_empty() {
        bail!("所有分片均无该消息表，请核对精确username或table");
    }
    let mut messages = Vec::new();
    let mut known = HashSet::new();
    let mut needed = BTreeSet::new();
    let mut reference_issues = Vec::new();
    for (index, shard) in shards.iter().enumerate() {
        let found = rows(
            &shard.conn,
            &table,
            "create_time>=?1 AND create_time<?2",
            &[&start, &end],
        )?;
        if found.len() as i64 != shard.expected {
            bail!("事务内COUNT与扫描不符: {}", shard.id);
        }
        for row in found {
            let server_id = number(&row["server_id"])?;
            if server_id != 0 {
                known.insert(server_id.to_string());
            }
            match reference(&row) {
                Ok(Some(id)) => {
                    needed.insert(id);
                }
                Ok(None) => {}
                Err(error) => reference_issues.push(
                    json!({"shard":shard.id,"local_id":row["local_id"],"issue":error.to_string()}),
                ),
            }
            messages.push(Message {
                shard: index,
                row,
                role: "window",
            });
        }
    }
    let window_rows = messages.len();
    let mut missing = Vec::new();
    while let Some(id) = needed.pop_first() {
        if !known.insert(id.clone()) {
            continue;
        }
        let server_id: i64 = match id.parse() {
            Ok(v) => v,
            Err(_) => {
                missing.push(id);
                continue;
            }
        };
        let mut found = Vec::new();
        for (index, shard) in shards.iter().enumerate() {
            for row in rows(
                &shard.conn,
                &table,
                "server_id=?1 AND create_time<?2",
                &[&server_id, &end],
            )? {
                found.push((index, row));
            }
        }
        if found.is_empty() {
            missing.push(id);
        }
        for (index, row) in found {
            match reference(&row) { Ok(Some(id)) => { needed.insert(id); }, Ok(None) => {}, Err(error) => reference_issues.push(json!({"shard":shards[index].id,"local_id":row["local_id"],"issue":error.to_string()})) }
            messages.push(Message {
                shard: index,
                row,
                role: "context",
            });
        }
    }
    // 跨分片顺序明确；同秒消息不会因单秒游标被跳过。
    messages.sort_by_key(|message| {
        (
            number(&message.row["create_time"]).unwrap(),
            number(&message.row["sort_seq"]).unwrap(),
            shards[message.shard].id.clone(),
            number(&message.row["local_id"]).unwrap(),
        )
    });
    std::fs::create_dir_all(&args.output)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&args.output, std::fs::Permissions::from_mode(0o700))?;
    }
    let raw_path = args.output.join("raw.jsonl");
    let mut writer = BufWriter::new(private_file(&raw_path)?);
    for message in &messages {
        let shard = &shards[message.shard];
        let identity = format!(
            "{}:{}:{}:{}",
            args.source_id,
            shard.id,
            table,
            number(&message.row["local_id"])?
        );
        let id = format!("{:x}", Sha256::digest(identity.as_bytes()))[..24].to_string();
        let sender = number(&message.row["real_sender_id"])?;
        let username = shard.names.get(&sender).cloned().unwrap_or_default();
        let display =
            crate::daemon::query::sender_display(&username, "", &contact_names, &group_nicknames);
        let sample = json!({"id":id,"role":message.role,"shard_id":shard.id,"sender_username":username,
            "sender_display":display,"sender_group_nickname":group_nicknames.get(&username),"row":message.row});
        serde_json::to_writer(&mut writer, &sample)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    let manifest = json!({"schema_version":"wechat-sql-rows-v1","timezone":args.timezone_offset,"since_inclusive":args.since,"until_exclusive":args.until_exclusive,"source_id":args.source_id,"table":table,"window_rows":window_rows,"context_rows":messages.len()-window_rows,"missing_reference_ids":missing,"reference_decode_issues":reference_issues,"raw_file":"raw.jsonl","raw_sha256":sha_file(&raw_path)?,"complete_count_match":true,"reader":"native_wx_sqlcipher_readonly_transaction","native_wx_raw_endpoint":"raw-export","snapshot_scope":"per_shard_read_transaction","absent_table_shards":absent,"shards":shards.iter().map(|s| json!({"shard_id":s.id,"columns":s.columns,"window_rows":s.expected})).collect::<Vec<_>>()});
    let mut file = private_file(&args.output.join("manifest.json"))?;
    serde_json::to_writer_pretty(&mut file, &manifest)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    println!(
        "{}",
        json!({"window_rows":window_rows,"context_rows":messages.len()-window_rows,"shards":shards.len(),"complete_count_match":true})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raw_encoding_preserves_blob_and_large_integer() {
        assert_eq!(
            raw_value(ValueRef::Integer(i64::MAX)).unwrap(),
            json!({"integer":i64::MAX.to_string()})
        );
        assert_eq!(
            raw_value(ValueRef::Blob(&[0, 255])).unwrap(),
            json!({"base64":"AP8="})
        );
        assert_eq!(
            raw_value(ValueRef::Real(f64::INFINITY)).unwrap(),
            json!({"real_bits":f64::INFINITY.to_bits().to_string()})
        );
        assert_eq!(
            raw_value(ValueRef::Text(&[255])).unwrap()["sqlite_type"],
            "text"
        );
    }
    #[test]
    fn fixed_timezone_is_independent_of_host() {
        assert_eq!(date_timestamp("2026-09-01", "+08:00").unwrap(), 1788192000);
    }
    #[test]
    fn scan_keeps_same_second_messages_and_extra_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE Msg_test(local_id INTEGER, create_time INTEGER, sort_seq INTEGER, future_extra BLOB); INSERT INTO Msg_test VALUES(1,10,1,x'00'),(2,10,2,x'FF'),(3,11,3,NULL);").unwrap();
        let found = rows(
            &conn,
            "Msg_test",
            "create_time>=?1 AND create_time<?2",
            &[&10, &11],
        )
        .unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[1]["future_extra"], json!({"base64":"/w=="}));
    }
    #[test]
    fn quote_from_parallel_xml_fragments_is_detected() {
        let row = json!({"local_type":49,"message_content":"<msg><appmsg><refermsg><svrid>1234567890123456789</svrid></refermsg></appmsg></msg><msgsource/>"});
        assert_eq!(
            reference(row.as_object().unwrap()).unwrap(),
            Some("1234567890123456789".to_string())
        );
    }
    #[test]
    fn export_joins_shards_and_recurses_reference_context() {
        let root = std::env::temp_dir().join(format!(
            "wx-raw-fixture-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let db_dir = root.join("db_storage");
        std::fs::create_dir_all(db_dir.join("message")).unwrap();
        let key = "0000000000000000000000000000000000000000000000000000000000000000";
        let table = "Msg_00000000000000000000000000000000";
        for index in 0..2 {
            let conn =
                Connection::open(db_dir.join(format!("message/message_{index}.db"))).unwrap();
            conn.execute_batch(&format!("PRAGMA key=\"x'{key}'\"; CREATE TABLE Name2Id(user_name TEXT); INSERT INTO Name2Id(rowid,user_name) VALUES(1,'fixture_sender'); CREATE TABLE {table}(local_id INTEGER PRIMARY KEY,server_id INTEGER,create_time INTEGER,sort_seq INTEGER,local_type INTEGER,real_sender_id INTEGER,message_content BLOB,source BLOB,packed_info_data BLOB, future_extra BLOB);")).unwrap();
            let sql =
                format!("INSERT INTO {table} VALUES(?1,?2,?3,?4,?5,1,?6,NULL,x'00FF',x'FF00')");
            if index == 0 {
                let xml = b"<msg><appmsg><refermsg><svrid>20</svrid></refermsg></appmsg></msg>";
                conn.execute(
                    &sql,
                    rusqlite::params![1, 30, 1788192010, 10, 49, xml.as_slice()],
                )
                .unwrap();
                conn.execute(
                    &sql,
                    rusqlite::params![2, 10, 1788191970, 1, 1, b"early ancestor".as_slice()],
                )
                .unwrap();
            } else {
                conn.execute(
                    &sql,
                    rusqlite::params![
                        1,
                        40,
                        1788192010,
                        10,
                        1,
                        b"same second across shards".as_slice()
                    ],
                )
                .unwrap();
                let xml = b"<msg><appmsg><refermsg><svrid>10</svrid></refermsg></appmsg></msg>";
                conn.execute(
                    &sql,
                    rusqlite::params![2, 20, 1788191990, 5, 49, xml.as_slice()],
                )
                .unwrap();
            }
        }
        std::fs::create_dir_all(db_dir.join("contact")).unwrap();
        let contact = Connection::open(db_dir.join("contact/contact.db")).unwrap();
        contact.execute_batch(&format!("PRAGMA key=\"x'{key}'\"; CREATE TABLE contact(username TEXT,nick_name TEXT,remark TEXT); INSERT INTO contact VALUES('fixture_sender','测试昵称','测试备注');")).unwrap();
        drop(contact);
        let keys_file = root.join("keys.json");
        std::fs::write(
            &keys_file,
            serde_json::to_vec(
                &json!({"message/message_0.db":key,"message/message_1.db":{"enc_key":key},"contact/contact.db":key}),
            )
            .unwrap(),
        )
        .unwrap();
        let output = root.join("output");
        cmd_raw_export(RawExportArgs {
            db_dir,
            keys_file,
            chat: None,
            table: Some(table.to_owned()),
            since: "2026-09-01".to_owned(),
            until_exclusive: "2026-09-02".to_owned(),
            timezone_offset: "+08:00".to_owned(),
            source_id: "test-fixture".to_owned(),
            output: output.clone(),
        })
        .unwrap();
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(output.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["window_rows"], 2);
        assert_eq!(manifest["context_rows"], 2);
        assert_eq!(manifest["missing_reference_ids"], json!([]));
        let samples = std::fs::read_to_string(output.join("raw.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        let ids = samples
            .iter()
            .map(|v| v["id"].as_str().unwrap())
            .collect::<HashSet<_>>();
        assert_eq!(ids.len(), 4);
        assert!(samples
            .iter()
            .all(|v| v["row"]["future_extra"] == json!({"base64":"/wA="})));
        assert!(samples
            .iter()
            .all(|v| v["sender_username"] == "fixture_sender"));
        assert!(samples.iter().all(|v| v["sender_display"] == "测试备注"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
