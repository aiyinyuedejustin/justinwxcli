//! Streaming, source-complete export for downstream tools. No model or daemon required.
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, TimeZone};
use clap::Args;
use regex::Regex;
use rusqlite::types::ValueRef;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Args)]
pub struct ExportArgs {
    /// Exact username or unambiguous contact/group display name
    pub chat: Option<String>,
    /// Additional chats (repeatable)
    #[arg(long = "in")]
    pub chats: Vec<String>,
    /// All group chats found in local message tables
    #[arg(long, conflicts_with = "all_chats")]
    pub all_groups: bool,
    /// All local message tables, including private chats
    #[arg(long)]
    pub all_chats: bool,
    /// Inclusive date/time, or RFC3339 timestamp with offset
    #[arg(long)]
    pub since: Option<String>,
    /// Exclusive end date/time (use next day to include a whole day)
    #[arg(long)]
    pub until: Option<String>,
    /// Timezone for dates without an offset
    #[arg(long, default_value = "+08:00", allow_hyphen_values = true)]
    pub timezone_offset: String,
    /// Match any keyword in decoded content (repeatable)
    #[arg(long)]
    pub keyword: Vec<String>,
    /// Match links on any domain, including subdomains (repeatable)
    #[arg(long)]
    pub domain: Vec<String>,
    /// Explicit cap; default exports all matching rows
    #[arg(short = 'n', long)]
    pub limit: Option<usize>,
    /// JSONL streams records; JSON collects a single document in memory
    #[arg(short = 'f', long, default_value = "jsonl", value_parser = ["jsonl", "json"])]
    pub format: String,
    /// Save to a new file; default writes stdout
    #[arg(short = 'o', long)]
    pub output: Option<PathBuf>,
}

fn timestamp(value: &str, zone: FixedOffset) -> Result<i64> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Ok(dt.timestamp());
    }
    let dt = NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M"))
        .or_else(|_| {
            NaiveDate::parse_from_str(value, "%Y-%m-%d").map(|d| d.and_hms_opt(0, 0, 0).unwrap())
        })
        .with_context(|| format!("无效日期或时间: {value}"))?;
    Ok(zone
        .from_local_datetime(&dt)
        .single()
        .context("时间映射失败")?
        .timestamp())
}

pub(crate) fn sqlite_value(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(n) if !(-9007199254740991..=9007199254740991).contains(&n) => {
            json!({"integer":n.to_string()})
        }
        ValueRef::Integer(n) => json!(n),
        ValueRef::Real(n) if !n.is_finite() => json!({"real_bits":n.to_bits().to_string()}),
        ValueRef::Real(n) => json!(n),
        ValueRef::Text(bytes) => std::str::from_utf8(bytes)
            .map(|s| json!(s))
            .unwrap_or_else(|_| json!({"base64":STANDARD.encode(bytes),"sqlite_type":"text"})),
        ValueRef::Blob(bytes) => json!({"base64":STANDARD.encode(bytes)}),
    }
}

fn links(text: &str) -> Vec<String> {
    static URL: OnceLock<Regex> = OnceLock::new();
    let re = URL.get_or_init(|| Regex::new(r#"https?://[^\s<>\"，。；]+"#).unwrap());
    let mut urls = Vec::new();
    for found in re.find_iter(text) {
        let url = found
            .as_str()
            .trim_end_matches(|c| matches!(c, ']' | '）' | ',' | '.' | ';'));
        let mut url = url.to_owned();
        while url.ends_with(')') && url.matches(')').count() > url.matches('(').count() {
            url.pop();
        }
        if !urls.contains(&url) {
            urls.push(url);
        }
    }
    urls
}

fn xml_text(node: roxmltree::Node<'_, '_>, tag: &str) -> Option<String> {
    node.descendants()
        .find(|n| n.has_tag_name(tag))
        .and_then(|n| n.text())
        .map(str::to_owned)
}

/// Preserve full XML alongside structured leaf fields, quotes and nested forwarded items.
fn structured_content(content: &str, parse_xml: bool) -> Value {
    let text = crate::daemon::query::legacy_sender_prefix(content)
        .map(|(_, text)| text)
        .unwrap_or(content);
    let mut result = json!({"urls":links(text)});
    if !parse_xml {
        return result;
    }
    let start = text.find('<');
    if let Some(start) = start {
        let xml = &text[start..];
        // WeChat can place an XML declaration inside <msg>. Remove declaration
        // markup only for parsing; CDATA/comments and the exported source stay intact.
        static XML_MARKUP: OnceLock<Regex> = OnceLock::new();
        let markup = XML_MARKUP.get_or_init(|| {
            Regex::new(r"(?s)<!\[CDATA\[.*?\]\]>|<!--.*?-->|<\?xml[ \t\r\n].*?\?>").unwrap()
        });
        let normalized = markup.replace_all(xml, |caps: &regex::Captures<'_>| {
            let matched = caps.get(0).unwrap().as_str();
            if matched.starts_with("<?xml") {
                String::new()
            } else {
                matched.to_owned()
            }
        });
        let fragment = format!("<wx_fragment>{normalized}</wx_fragment>");
        match roxmltree::Document::parse(&normalized)
            .or_else(|_| roxmltree::Document::parse(&fragment))
        {
            Ok(doc) => {
                let root = doc.root();
                let fields: Vec<Value> = root
                    .descendants()
                    .filter(|n| n.is_element() && !n.children().any(|c| c.is_element()))
                    .filter_map(|n| {
                        n.text()
                            .map(|text| json!({"name":n.tag_name().name(),"text":text}))
                    })
                    .collect();
                let mut urls = links(&text[..start]);
                for field in &fields {
                    for url in links(field["text"].as_str().unwrap()) {
                        if !urls.contains(&url) {
                            urls.push(url);
                        }
                    }
                }
                result["urls"] = json!(urls);
                result["xml_fields"] = json!(fields);
                if let Some(quote) = root.descendants().find(|n| n.has_tag_name("refermsg")) {
                    result["quote"] = json!({"server_id":xml_text(quote,"svrid"),"sender_username":xml_text(quote,"fromusr"),"sender_display":xml_text(quote,"displayname"),"content":xml_text(quote,"content"),"type":xml_text(quote,"type")});
                }
                if let Some(record) = root.descendants().find(|n| n.has_tag_name("recorditem")) {
                    if let Some(inner) = record.text() {
                        // recorditem is escaped XML; decode its structure without truncating it.
                        if inner != text {
                            result["forwarded"] = structured_content(inner, true);
                        }
                    }
                }
            }
            Err(error) => {
                result["parse_issue"] = json!(error.to_string());
            }
        }
    }
    result
}

fn content_values(value: &Value, texts: &mut Vec<String>, urls: &mut Vec<String>) {
    if let Some(fields) = value.get("xml_fields").and_then(Value::as_array) {
        for field in fields {
            if let Some(text) = field.get("text").and_then(Value::as_str) {
                texts.push(text.to_owned());
            }
        }
    }
    if let Some(values) = value.get("urls").and_then(Value::as_array) {
        for url in values {
            if let Some(url) = url.as_str() {
                urls.push(url.to_owned());
            }
        }
    }
    if let Some(forwarded) = value.get("forwarded") {
        content_values(forwarded, texts, urls);
    }
}

fn domain_matches(url: &str, domain: &str) -> bool {
    let host = url
        .split_once("://")
        .map(|(_, s)| s.split(['/', '?', '#']).next().unwrap_or(""))
        .unwrap_or("");
    let host = host
        .rsplit('@')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    host == domain || host.ends_with(&format!(".{domain}"))
}

struct ExportFile {
    path: Option<PathBuf>,
    complete: bool,
}
impl Drop for ExportFile {
    fn drop(&mut self) {
        if !self.complete {
            if let Some(path) = &self.path {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

pub fn cmd_export(args: ExportArgs) -> Result<()> {
    if args.chat.is_none() && args.chats.is_empty() && !args.all_groups && !args.all_chats {
        bail!("指定聊天名称/username、--in、--all-groups 或 --all-chats");
    }
    if (args.all_groups || args.all_chats) && (args.chat.is_some() || !args.chats.is_empty()) {
        bail!("全部会话范围与指定聊天二选一");
    }
    let zone: FixedOffset = args
        .timezone_offset
        .parse()
        .context("时区须为+08:00等固定偏移")?;
    let since = args
        .since
        .as_deref()
        .map(|s| timestamp(s, zone))
        .transpose()?
        .unwrap_or(i64::MIN);
    let until = args
        .until
        .as_deref()
        .map(|s| timestamp(s, zone))
        .transpose()?
        .unwrap_or(i64::MAX);
    if since >= until {
        bail!("--until 是排他上界，必须晚于 --since");
    }
    let cfg = crate::config::load_config()?;
    let keys =
        crate::daemon::extract_keys(&serde_json::from_slice(&std::fs::read(&cfg.keys_file)?)?);
    let source_id = cfg
        .db_dir
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .context("数据库目录缺少账号名称")?
        .to_owned();
    let contact_key = keys
        .get("contact/contact.db")
        .context("缺少联系人数据库密钥")?;
    let contact = crate::crypto::sqlcipher::open_encrypted_readonly(
        &cfg.db_dir.join("contact/contact.db"),
        contact_key,
    )?;
    contact.execute_batch("BEGIN")?;
    let mut names = HashMap::new();
    let mut verified = HashSet::new();
    let mut stmt = contact.prepare("SELECT username,nick_name,remark,verify_flag FROM contact")?;
    let mapped = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            row.get::<_, Option<i64>>(3)?.unwrap_or(0),
        ))
    })?;
    for row in mapped {
        let (username, nick, remark, flag) = row?;
        if flag != 0 {
            verified.insert(username.clone());
        }
        let display = if !remark.is_empty() {
            remark
        } else if !nick.is_empty() {
            nick
        } else {
            username.clone()
        };
        names.insert(username, display);
    }
    let pattern = Regex::new(r"^message_\d+\.db$")?;
    let mut paths = std::fs::read_dir(cfg.db_dir.join("message"))?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.retain(|p| {
        p.file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| pattern.is_match(s))
    });
    paths.sort();
    if paths.is_empty() {
        bail!("未找到消息分片");
    }
    let mut shards = Vec::new();
    for path in paths {
        let shard = format!(
            "message/{}",
            path.file_name()
                .unwrap()
                .to_str()
                .context("分片名称编码错误")?
        );
        let key = keys
            .get(&shard)
            .with_context(|| format!("分片缺少密钥: {shard}"))?;
        let conn = crate::crypto::sqlcipher::open_encrypted_readonly(&path, key)?;
        conn.execute_batch("BEGIN")?;
        let mut statement = conn.prepare("SELECT rowid,user_name FROM Name2Id")?;
        let senders = statement
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;
        for name in senders.values() {
            names.entry(name.clone()).or_insert_with(|| name.clone());
        }
        drop(statement);
        shards.push((shard, conn, senders));
    }
    let table_names: HashMap<String, String> = names
        .keys()
        .map(|u| (format!("Msg_{:x}", md5::compute(u)), u.clone()))
        .collect();
    let mut requested = args.chats.clone();
    if let Some(chat) = &args.chat {
        requested.push(chat.clone());
    }
    let mut selected = HashSet::new();
    for request in requested {
        if names.contains_key(&request) {
            selected.insert(request);
            continue;
        }
        let exact: Vec<_> = names
            .iter()
            .filter(|(_, n)| *n == &request)
            .map(|(u, _)| u.clone())
            .collect();
        if exact.len() != 1 {
            bail!("聊天名称未命中或重名: {request}；请通过 wx sessions 获取精确 username");
        }
        selected.insert(exact[0].clone());
    }
    let keywords: Vec<_> = args.keyword.iter().map(|s| s.to_lowercase()).collect();
    let domains: Vec<_> = args
        .domain
        .iter()
        .map(|s| s.trim_start_matches(".").to_ascii_lowercase())
        .collect();
    let mut export_file = ExportFile {
        path: None,
        complete: false,
    };
    let mut writer: Box<dyn Write> = if let Some(path) = &args.output {
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        export_file.path = Some(path.clone());
        Box::new(BufWriter::new(file))
    } else {
        Box::new(BufWriter::new(std::io::stdout().lock()))
    };
    if args.format == "jsonl" {
        serde_json::to_writer(
            &mut writer,
            &json!({"record":"metadata","schema_version":"wx-messages-v1","source_id":source_id}),
        )?;
        writer.write_all(b"\n")?;
    }
    let mut collected = Vec::new();
    let mut count = 0usize;
    let mut scanned = 0usize;
    let mut truncated = false;
    let mut by_chat = BTreeMap::<String, usize>::new();
    let mut unrecognized = Vec::new();
    let mut matched_chats = HashSet::new();
    let table_pattern = Regex::new(r"^Msg_[0-9a-fA-F]{32}$")?;
    'shards: for (shard, conn, senders) in &shards {
        let mut statement = conn.prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'Msg_%' ORDER BY name",
        )?;
        let tables = statement
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for table in tables {
            if !table_pattern.is_match(&table) {
                bail!("未知消息表名: {table}");
            }
            let username = table_names.get(&table);
            if username.is_none() {
                unrecognized.push(format!("{shard}:{table}"));
            }
            if args.all_groups && !username.is_some_and(|u| u.ends_with("@chatroom")) {
                continue;
            }
            if !selected.is_empty() && !username.is_some_and(|u| selected.contains(u)) {
                continue;
            }
            if let Some(u) = username {
                matched_chats.insert(u.clone());
            }
            let group = username.is_some_and(|u| u.ends_with("@chatroom"));
            let group_names = if group {
                crate::daemon::query::load_group_nickname_map_from_conn(
                    &contact,
                    username.unwrap(),
                    None,
                )
            } else {
                HashMap::new()
            };
            let sql=format!("SELECT * FROM \"{table}\" WHERE create_time>=?1 AND create_time<?2 ORDER BY create_time,sort_seq,local_id");
            let mut statement = conn.prepare(&sql)?;
            let columns = statement
                .column_names()
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            let index = |name: &str| {
                columns
                    .iter()
                    .position(|n| n == name)
                    .with_context(|| format!("消息表缺少字段 {name}: {shard}/{table}"))
            };
            let local = index("local_id")?;
            let server = index("server_id")?;
            let time = index("create_time")?;
            let kind = index("local_type")?;
            let sender = index("real_sender_id")?;
            let body = index("message_content")?;
            let compressed = index("WCDB_CT_message_content")?;
            let mut rows = statement.query([since, until])?;
            while let Some(row) = rows.next()? {
                scanned += 1;
                let local_id: i64 = row.get(local)?;
                let server_id: i64 = row.get(server)?;
                let ts: i64 = row.get(time)?;
                let local_type: i64 = row.get(kind)?;
                let sender_id: i64 = row.get(sender)?;
                let bytes = match row.get_ref(body)? {
                    ValueRef::Text(b) | ValueRef::Blob(b) => b.to_vec(),
                    ValueRef::Null => Vec::new(),
                    _ => bail!("message_content 既非文本也非BLOB"),
                };
                let compression: i64 = row.get(compressed)?;
                let bytes = if compression == 4 {
                    zstd::stream::decode_all(bytes.as_slice())
                        .with_context(|| format!("正文解压失败: {shard}/{table}/{local_id}"))?
                } else {
                    bytes
                };
                let text = String::from_utf8(bytes)
                    .with_context(|| format!("正文UTF-8解码失败: {shard}/{table}/{local_id}"))?;
                let structured = structured_content(&text, local_type & 0xffffffff != 1);
                let mut texts = vec![text.clone()];
                let mut urls = Vec::new();
                content_values(&structured, &mut texts, &mut urls);
                if !keywords.is_empty()
                    && !texts
                        .iter()
                        .any(|text| keywords.iter().any(|k| text.to_lowercase().contains(k)))
                {
                    continue;
                }
                if !domains.is_empty()
                    && !urls
                        .iter()
                        .any(|u| domains.iter().any(|d| domain_matches(u, d)))
                {
                    continue;
                }
                if args.limit.is_some_and(|limit| count >= limit) {
                    truncated = true;
                    break 'shards;
                }
                let sender_username = senders
                    .get(&sender_id)
                    .map(String::as_str)
                    .or_else(|| crate::daemon::query::legacy_sender_prefix(&text).map(|(u, _)| u));
                let display = sender_username
                    .map(|u| crate::daemon::query::sender_display(u, "", &names, &group_names));
                let mut raw = Map::new();
                for (i, name) in columns.iter().enumerate() {
                    raw.insert(name.clone(), sqlite_value(row.get_ref(i)?));
                }
                let message = json!({"record":"message","id":format!("{source_id}:{shard}:{table}:{local_id}"),"source_id":source_id,"shard":shard,"table":table,"local_id":local_id,"server_id":server_id.to_string(),"chat_username":username,"chat_name":username.and_then(|u|names.get(u)),"chat_type":if group{"group"}else if username.is_some_and(|u|u=="brandsessionholder" || u=="@placeholder_foldgroup"){"folded"}else if username.is_some_and(|u|verified.contains(u) || u.starts_with("gh_") || u.starts_with("biz_") || u.starts_with('@')){"official_account"}else if username.is_some(){"private"}else{"unknown"},"sender_username":sender_username,"sender_display":display,"timestamp":ts,"time":zone.timestamp_opt(ts,0).single().context("消息时间越界")?.to_rfc3339(),"local_type":local_type,"content":text,"structured":structured,"row":raw});
                if args.format == "jsonl" {
                    serde_json::to_writer(&mut writer, &message)?;
                    writer.write_all(b"\n")?;
                } else {
                    collected.push(message);
                }
                count += 1;
                *by_chat
                    .entry(username.cloned().unwrap_or_else(|| table.clone()))
                    .or_default() += 1;
            }
        }
    }
    let missing: Vec<_> = selected.difference(&matched_chats).collect();
    if !truncated && !missing.is_empty() {
        bail!(
            "指定会话没有本地消息表: {}",
            missing
                .into_iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let summary = json!({"record":"summary","schema_version":"wx-messages-v1","source_id":source_id,"count":count,"rows_scanned":scanned,"shards":shards.len(),"chats":by_chat,"unresolved_tables":unrecognized,"truncated":truncated,"scope_complete":!truncated && (!args.all_groups || unrecognized.is_empty()),"since_inclusive":args.since,"until_exclusive":args.until,"timezone":args.timezone_offset,"order":"shard,table,create_time,sort_seq,local_id","snapshot_scope":"per_database_read_transaction"});
    if args.format == "jsonl" {
        serde_json::to_writer(&mut writer, &summary)?;
    } else {
        serde_json::to_writer(&mut writer, &json!({"messages":collected,"meta":summary}))?;
    }
    writer.write_all(b"\n")?;
    writer.flush()?;
    export_file.complete = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timestamps_have_explicit_exclusive_boundaries() {
        let z: FixedOffset = "+08:00".parse().unwrap();
        assert_eq!(
            timestamp("2026-11-07", z).unwrap(),
            timestamp("2026-11-07T00:00:00+08:00", z).unwrap()
        );
    }
    #[test]
    fn quoted_and_forwarded_xml_keeps_complete_text_and_links() {
        let long = "文字".repeat(200);
        let xml=format!("<msg><appmsg><refermsg><svrid>1234567890123456789</svrid><content>{long}</content></refermsg><url>https://huggingface.co/a?x=1&amp;y=2</url></appmsg></msg>");
        let v = structured_content(&xml, true);
        assert_eq!(v["quote"]["content"], long);
        assert_eq!(v["urls"][0], "https://huggingface.co/a?x=1&y=2");
        assert!(domain_matches(
            v["urls"][0].as_str().unwrap(),
            "huggingface.co"
        ));
        assert!(!domain_matches(
            "https://evil-huggingface.co/a",
            "huggingface.co"
        ));
    }
    #[test]
    fn nested_declarations_preserve_forwarded_text_and_source() {
        let content = r#"wxid_alice:
<msg><?xml version="1.0"?><appmsg><recorditem><![CDATA[<?xml version="1.0"?><recordinfo><datalist><dataitem><datadesc><![CDATA[literal <?xml version="1.0"?> example]]]]><![CDATA[></datadesc><url>https://example.com/?a=1&amp;b=2</url></dataitem></datalist></recordinfo>]]></recorditem></appmsg></msg><msgsource/>"#;
        let original = content.to_owned();
        let parsed = structured_content(content, true);
        assert!(parsed.get("parse_issue").is_none(), "{parsed}");
        let forwarded = &parsed["forwarded"];
        assert!(forwarded.get("parse_issue").is_none(), "{forwarded}");
        assert_eq!(forwarded["urls"][0], "https://example.com/?a=1&b=2");
        assert!(forwarded["xml_fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|field| field["text"] == r#"literal <?xml version="1.0"?> example"#));
        assert_eq!(content, original);
    }

    #[test]
    fn plain_text_angle_brackets_are_not_xml() {
        let content = "说明 <窗口> 和 <think>，参考 https://example.com/a";
        let parsed = structured_content(content, false);
        assert_eq!(parsed, json!({"urls":["https://example.com/a"]}));
        let malformed = structured_content("<msg><appmsg>", true);
        assert!(malformed.get("parse_issue").is_some());
    }
}
