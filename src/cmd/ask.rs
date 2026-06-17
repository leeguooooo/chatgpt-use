//! Mode 1 · sidekick. One-shot: gather optional file context, send the prompt
//! through a ChatGPT web channel, print the reply. No tool loop.
//!
//! Owned by the MODES-1-2 agent.

use crate::cli::AskArgs;
use anyhow::Result;

pub fn run(_args: &AskArgs) -> Result<()> {
    todo!("MODE 1: build context from --file, channel.connect, channel.send, print reply")
}
