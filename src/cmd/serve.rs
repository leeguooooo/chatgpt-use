//! Mode 3 · drop-in model. A local Anthropic-compatible endpoint (tiny_http)
//! so Claude Code (ANTHROPIC_BASE_URL=http://host:port) spends no model tokens:
//! its model calls are served by the ChatGPT web subscription instead.
//!
//! Per POST /v1/messages:
//!   - translate Anthropic {system, messages[], tools[]} into a prompt + the
//!     protocol tool catalog
//!   - drive it through the ChatGPT web channel
//!   - protocol::parse_reply, then re-encode as an Anthropic response:
//!       Text  -> {content:[{type:"text",...}]}
//!       Tools -> {content:[{type:"tool_use",id,name,input}], stop_reason:"tool_use"}
//!   - emit the SSE event sequence Claude Code expects when stream:true
//!
//! This is the most fragile mode (giant prompts, concurrency 1, rate limits,
//! tool-schema fidelity). Aim for a correct single-turn PoC; mark experimental.
//!
//! Owned by the MODE-3 agent.

use crate::cli::ServeArgs;
use anyhow::Result;

pub fn run(_args: &ServeArgs) -> Result<()> {
    todo!("MODE 3: tiny_http server, Anthropic <-> protocol translation over the channel")
}
