//! Tool declarations and the names they travel under (ported from OpenCodex
//! `responses/namespace-tool-compat.ts` / `tool-name-aliases.ts`, reduced to what typed items need).
//!
//! A namespaced tool is declared to the wire as `<namespace>__<name>`; calls coming back are
//! split again using the declared set, so a name the caller never declared is not authorised.

use crate::ir::ToolDecl;
use serde_json::Value;
use std::collections::HashMap;

/// The wire name of a (possibly namespaced) tool.
pub fn wire_name(namespace: Option<&str>, name: &str) -> String {
    match namespace {
        Some(ns) if !ns.is_empty() => {
            if ns.ends_with("__") {
                format!("{ns}{name}")
            } else {
                format!("{ns}__{name}")
            }
        }
        _ => name.to_string(),
    }
}

/// Reads the Responses `tools` array. Only `function` tools (and the functions inside a
/// `namespace`) are kept; hosted tools (`web_search`, image generation, ...) have no wire form.
pub fn declared_tools(tools: &Value) -> Vec<ToolDecl> {
    let mut out = Vec::new();
    let Some(list) = tools.as_array() else {
        return out;
    };
    for tool in list {
        match tool.get("type").and_then(Value::as_str) {
            Some("function") => {
                if let Some(decl) = function_decl(tool, None) {
                    out.push(decl);
                }
            }
            Some("namespace") => {
                let ns = tool.get("name").and_then(Value::as_str).unwrap_or("");
                let inner = tool.get("tools").and_then(Value::as_array);
                for tool in inner.into_iter().flatten() {
                    if tool.get("type").and_then(Value::as_str) == Some("function")
                        && let Some(decl) = function_decl(tool, Some(ns))
                    {
                        out.push(decl);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn function_decl(tool: &Value, namespace: Option<&str>) -> Option<ToolDecl> {
    let name = tool.get("name")?.as_str()?.to_string();
    let namespace = namespace.filter(|ns| !ns.is_empty()).map(str::to_string);
    Some(ToolDecl {
        wire_name: wire_name(namespace.as_deref(), &name),
        namespace,
        name,
        description: tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        parameters: tool
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}})),
        strict: tool.get("strict").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// Wire name → declared tool, for splitting names on the way back.
#[derive(Clone, Debug, Default)]
pub struct ToolRegistry {
    by_wire: HashMap<String, (Option<String>, String)>,
}

impl ToolRegistry {
    pub fn new(tools: &[ToolDecl]) -> Self {
        Self {
            by_wire: tools
                .iter()
                .map(|t| (t.wire_name.clone(), (t.namespace.clone(), t.name.clone())))
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.by_wire.is_empty()
    }

    /// `None` when the model called a tool that was never declared.
    pub fn resolve(&self, wire: &str) -> Option<(Option<String>, String)> {
        self.by_wire.get(wire).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn namespaces_flatten_and_split_back() {
        let tools = json!([
            {"type": "function", "name": "shell", "description": "run", "strict": false,
             "parameters": {"type": "object", "properties": {}}},
            {"type": "namespace", "name": "mcp__files__", "description": "",
             "tools": [{"type": "function", "name": "read", "description": "r", "strict": false,
                        "parameters": {"type": "object", "properties": {}}}]},
            {"type": "web_search"}
        ]);
        let decls = declared_tools(&tools);
        assert_eq!(decls.len(), 2);
        assert_eq!(decls[1].wire_name, "mcp__files__read");
        let registry = ToolRegistry::new(&decls);
        assert_eq!(
            registry.resolve("mcp__files__read"),
            Some((Some("mcp__files__".to_string()), "read".to_string()))
        );
        assert_eq!(registry.resolve("shell"), Some((None, "shell".to_string())));
        assert_eq!(registry.resolve("undeclared"), None);
        assert_eq!(wire_name(Some("a"), "b"), "a__b");
    }
}
