//! Mode 2 · brain. ChatGPT drives local tools in an agent loop:
//!   1. seed the conversation with protocol::system_prompt(tools, task)
//!   2. channel.send → protocol::parse_reply
//!   3. if Tools: tools::execute each, protocol::render_results, send back; loop
//!   4. if Text: that's the final answer — print and stop
//! Respect --max-steps and --approve. Context accumulates inside ChatGPT, so
//! each turn only sends the new tool results, not the whole history.
//!
//! Owned by the MODES-1-2 agent.

use crate::cli::RunArgs;
use anyhow::Result;

pub fn run(_args: &RunArgs) -> Result<()> {
    todo!("MODE 2: agent loop over channel + protocol + tools")
}
