//! `wx timeline` — 跨会话按时间拉取消息

use super::history::{parse_time, parse_time_end};
use super::output::{emit_warnings, print_response, OutputOpts};
use super::transport;
use crate::ipc::Request;
use anyhow::Result;

pub fn cmd_timeline(
    limit: usize,
    offset: usize,
    since: Option<String>,
    until: Option<String>,
    after: Option<String>,
    msg_type: Option<String>,
    opts: OutputOpts,
) -> Result<()> {
    let since_ts = since.as_deref().map(parse_time).transpose()?;
    let until_ts = until.as_deref().map(parse_time_end).transpose()?;
    let after_ts = after
        .as_deref()
        .map(super::history::parse_time_or_unix)
        .transpose()?;
    let type_val = match msg_type.as_deref() {
        Some(s) => Some(super::history::parse_msg_type_required(s)?),
        None => None,
    };
    let (with_meta, debug_source) = opts.request_flags();

    let resp = transport::send(Request::Timeline {
        limit,
        offset,
        since: since_ts,
        until: until_ts,
        after_ts,
        msg_type: type_val,
        with_meta,
        debug_source,
    })?;
    emit_warnings(&resp.data);
    print_response(&resp.data, &opts)
}
