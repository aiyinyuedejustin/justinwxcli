use super::output::{emit_warnings, print_response, OutputOpts};
use super::transport;
use crate::ipc::Request;
use anyhow::Result;

pub fn cmd_history(
    chat: String,
    limit: usize,
    offset: usize,
    since: Option<String>,
    until: Option<String>,
    after: Option<String>,
    before: Option<String>,
    msg_type: Option<String>,
    opts: OutputOpts,
) -> Result<()> {
    let since_ts = since.as_deref().map(parse_time).transpose()?;
    let until_ts = until.as_deref().map(parse_time_end).transpose()?;
    let after_ts = after.as_deref().map(parse_time_or_unix).transpose()?;
    let before_ts = before.as_deref().map(parse_time_or_unix).transpose()?;
    let type_val = match msg_type.as_deref() {
        Some(s) => Some(parse_msg_type_required(s)?),
        None => None,
    };
    let (with_meta, debug_source) = opts.request_flags();

    let req = Request::History {
        chat,
        limit,
        offset,
        since: since_ts,
        until: until_ts,
        after_ts,
        before_ts,
        msg_type: type_val,
        with_meta,
        debug_source,
    };
    let resp = transport::send(req)?;
    emit_warnings(&resp.data);
    print_response(&resp.data, &opts)
}

/// 支持 Unix 秒时间戳数字，或与 parse_time 相同的日期字符串。
pub fn parse_time_or_unix(s: &str) -> Result<i64> {
    if let Ok(n) = s.parse::<i64>() {
        if n > 1_000_000_000 {
            return Ok(n);
        }
    }
    parse_time(s)
}

pub fn parse_time(s: &str) -> Result<i64> {
    use chrono::{Local, TimeZone};
    for fmt in &["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Local
                .from_local_datetime(&dt)
                .single()
                .map(|d| d.timestamp())
                .ok_or_else(|| anyhow::anyhow!("本地时间歧义: {}", s));
        }
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let dt = d.and_hms_opt(0, 0, 0).unwrap();
        return Local
            .from_local_datetime(&dt)
            .single()
            .map(|d| d.timestamp())
            .ok_or_else(|| anyhow::anyhow!("本地时间歧义: {}", s));
    }
    anyhow::bail!(
        "无法解析时间 '{}'，支持 YYYY-MM-DD / YYYY-MM-DD HH:MM / YYYY-MM-DD HH:MM:SS",
        s
    )
}

pub fn parse_time_end(s: &str) -> Result<i64> {
    use chrono::{Local, TimeZone};
    if s.len() == 10 {
        if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
            let dt = d.and_hms_opt(23, 59, 59).unwrap();
            return Local
                .from_local_datetime(&dt)
                .single()
                .map(|d| d.timestamp())
                .ok_or_else(|| anyhow::anyhow!("本地时间歧义: {}", s));
        }
    }
    parse_time(s)
}

/// 将消息类型字符串转为 local_type 整数，未知类型返回 None
pub fn parse_msg_type(s: &str) -> Option<i64> {
    match s {
        "text" => Some(1),
        "image" => Some(3),
        "voice" => Some(34),
        "video" => Some(43),
        "card" => Some(42),
        "sticker" => Some(47),
        "location" => Some(48),
        // appmsg 总类（链接/文件/引用…）；与 type_id "appmsg" 对齐
        "link" | "file" | "appmsg" => Some(49),
        "call" => Some(50),
        "system" => Some(10000),
        "revoke" => Some(10002),
        // 允许 agent 直接传数字 type_code
        other if other.chars().all(|c| c.is_ascii_digit()) => other.parse().ok(),
        _ => None,
    }
}

/// Agent-first：未知 `--type` 必须失败，禁止静默忽略过滤器。
pub fn parse_msg_type_required(s: &str) -> Result<i64> {
    parse_msg_type(s).ok_or_else(|| {
        anyhow::anyhow!(
            "未知消息类型 '{}'。支持: text, image, voice, video, card, sticker, location, \
             link|file|appmsg, call, system, revoke，或数字 type_code",
            s
        )
    })
}

#[cfg(test)]
mod msg_type_tests {
    use super::*;

    #[test]
    fn parse_msg_type_accepts_slugs_and_codes() {
        assert_eq!(parse_msg_type("text"), Some(1));
        assert_eq!(parse_msg_type("appmsg"), Some(49));
        assert_eq!(parse_msg_type("49"), Some(49));
        assert_eq!(parse_msg_type("revoke"), Some(10002));
        assert!(parse_msg_type("not-a-type").is_none());
    }

    #[test]
    fn parse_msg_type_required_errors_on_unknown() {
        assert!(parse_msg_type_required("nope").is_err());
        assert_eq!(parse_msg_type_required("image").unwrap(), 3);
    }
}
