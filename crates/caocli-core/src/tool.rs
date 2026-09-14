//! What a tool is, on the way to a model that takes a tool list.
//!
//! Both providers (Anthropic, OpenAI) and MCP offer tools in this shape, and
//! the shape is the same whichever side the tool came from: a name, a short
//! description, and the JSON Schema of the arguments. Keeping the type in
//! `caocli-core` is what lets the MCP hub build a `ToolDef` the agent's
//! request builder can consume without knowing which crate it was built in.

use serde::{Deserialize, Serialize};

/// One tool as a backend describes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
}

/// One tool in the shape every backend expects: a typed wrapper around
/// [`FunctionDef`] so that a future `r#type` (e.g. Anthropic's `tool_use_2024`)
/// has somewhere to live without breaking the call sites.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub r#type: String,
    pub function: FunctionDef,
}
