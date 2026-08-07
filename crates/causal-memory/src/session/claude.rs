//! Claude Code session parser.
//!
//! WIP — the interface is in place so `--agent claude` resolves cleanly, but
//! the actual parsing of `~/.claude/projects/**/<session>.jsonl` (tool_use
//! blocks inside `assistant.message.content`, tool_result blocks inside
//! `user.message.content`) is the next step.

use anyhow::Result;

use super::{ParsedSession, SessionParser, SessionSource};

/// Parser for Claude Code session jsonl files.
pub struct ClaudeParser;

impl SessionParser for ClaudeParser {
    fn parse(&self, _source: &SessionSource) -> Result<ParsedSession> {
        anyhow::bail!("ClaudeCode parser not implemented yet — planned as next step")
    }
}
