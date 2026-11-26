//! What MCP servers offer beside their tools: resources, resource templates
//! and subscriptions to resource updates, prompts (run as slash commands)
//! and completions; for the model as tools, for the person as commands,
//! and for a client as requests. Design and sources:
//! docs/evidence/mcp-client.md.

use std::path::Path;

use serde_json::{json, Value};

use super::{connection, loaded, tool_name, Asking, Call, Fault, ToolSpec, Tools};
use crate::store::Store;
use crate::ErrorInfo;

/// The model's tools for the resources of the servers (Codex's names).
pub(crate) const LIST_RESOURCES: &str = "list_mcp_resources";
pub(crate) const LIST_TEMPLATES: &str = "list_mcp_resource_templates";
pub(crate) const READ_RESOURCE: &str = "read_mcp_resource";

/// The tools that list and read the servers' resources.
pub(super) fn resource_specs() -> Vec<ToolSpec> {
    let server = json!({"type": "string",
        "description": "The MCP server's name; every server offering resources if left out."});
    vec![
        ToolSpec {
            name: LIST_RESOURCES.into(),
            description: "Lists the resources the MCP servers offer: data such as files, \
                          records or documents, each with its server and uri for \
                          read_mcp_resource."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {"server": server},
                "additionalProperties": false,
            }),
        },
        ToolSpec {
            name: LIST_TEMPLATES.into(),
            description: "Lists the resource templates the MCP servers offer: URI templates \
                          (RFC 6570) whose {placeholders} you fill to make the uri of a \
                          resource for read_mcp_resource."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {"server": server},
                "additionalProperties": false,
            }),
        },
        ToolSpec {
            name: READ_RESOURCE.into(),
            description: "Reads a resource of an MCP server by its uri: one listed by \
                          list_mcp_resources, or one made from a resource template."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "server": {"type": "string", "description": "The MCP server's name."},
                    "uri": {"type": "string", "description": "The resource's URI."},
                },
                "required": ["server", "uri"],
                "additionalProperties": false,
            }),
        },
    ]
}

/// Runs one of the resource tools; `None` when `name` is not one. Images a
/// resource holds are saved in `images` and returned to be shown.
pub(crate) async fn resource_tool(
    store: &Store,
    name: &str,
    arguments: &str,
    asking: Option<Asking<'_>>,
    cancel: tokio::sync::watch::Receiver<bool>,
    images: &Path,
) -> Option<Result<(String, Vec<crate::conversation::Image>), String>> {
    #[derive(serde::Deserialize)]
    struct Arguments {
        server: Option<String>,
        uri: Option<String>,
    }
    let (method, key) = match name {
        LIST_RESOURCES => ("resources/list", "resources"),
        LIST_TEMPLATES => ("resources/templates/list", "resourceTemplates"),
        READ_RESOURCE => ("resources/read", "contents"),
        _ => return None,
    };
    let arguments: Arguments = match serde_json::from_str(match arguments.trim() {
        "" => "{}",
        text => text,
    }) {
        Ok(arguments) => arguments,
        Err(error) => return Some(Err(format!("invalid arguments: {error}"))),
    };
    let tools = loaded(store).await;
    if method != "resources/read" {
        let listed = listed(&tools, arguments.server.as_deref(), method, key).await;
        return Some(listed.map(|listed| (listed.to_string(), Vec::new())));
    }
    let (Some(server), Some(uri)) = (arguments.server, arguments.uri) else {
        return Some(Err("give server and uri".into()));
    };
    let result = read(store, &tools, &server, &uri, asking.as_ref(), &cancel).await;
    Some(result.map(|result| contents_for_model(&result, images)))
}

/// Every item of the list `method` of `server`, or of every started server
/// offering resources, each with its server's name; servers whose list
/// failed are named under `problems`.
async fn listed(
    tools: &Tools,
    server: Option<&str>,
    method: &'static str,
    key: &str,
) -> Result<Value, String> {
    let offering: Vec<&super::Served> = tools
        .servers
        .iter()
        .filter(|served| server.is_none_or(|server| served.name == server))
        .filter(|served| served.slot.current().offers("/resources"))
        .collect();
    if let (Some(server), true) = (server, offering.is_empty()) {
        return Err(format!(
            "no started MCP server named {server} offers resources"
        ));
    }
    let lists =
        futures_util::future::join_all(offering.iter().map(|served| served.slot.list(method, key)))
            .await;
    let mut items = Vec::new();
    let mut problems = Vec::new();
    for (served, list) in offering.iter().zip(lists) {
        match list {
            Ok(list) => items.extend(list.into_iter().map(|mut item| {
                if let Some(fields) = item.as_object_mut() {
                    fields.remove("icons");
                    fields.insert("server".into(), json!(served.name));
                }
                item
            })),
            Err(fault) => problems.push(format!("server {}: {fault}", served.name)),
        }
    }
    let mut listed = json!({ key: items });
    if !problems.is_empty() {
        listed["problems"] = json!(problems);
    }
    Ok(listed)
}

/// `resources/read` of `uri` on `server`: the complete result.
async fn read(
    store: &Store,
    tools: &Tools,
    server: &str,
    uri: &str,
    asking: Option<&Asking<'_>>,
    cancel: &tokio::sync::watch::Receiver<bool>,
) -> Result<Value, String> {
    let connection = offering(tools, server, "/resources", "resources")?;
    let call = Call {
        store,
        connection: &connection,
        method: "resources/read",
        name: uri,
        asking,
    };
    call.result(json!({"uri": uri}), cancel)
        .await
        .map_err(|fault| fault.to_string())
}

/// The connection of the started server `server` when it offers what its
/// capabilities hold at `pointer`; `what` names it in the error.
fn offering(
    tools: &Tools,
    server: &str,
    pointer: &str,
    what: &str,
) -> Result<std::sync::Arc<connection::Connection>, String> {
    let served = tools
        .served(server)
        .ok_or_else(|| format!("no MCP server named {server} started"))?;
    let connection = served.slot.current();
    match connection.offers(pointer) {
        true => Ok(connection),
        false => Err(format!("the MCP server {server} offers no {what}")),
    }
}

/// A resource's contents for the model: text as it is, images saved in
/// `images` and shown, other binary data named with its type and size.
fn contents_for_model(result: &Value, images: &Path) -> (String, Vec<crate::conversation::Image>) {
    let mut parts = Vec::new();
    let mut shown = Vec::new();
    for content in result
        .get("contents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let field = |key: &str| content.get(key).and_then(Value::as_str).unwrap_or("");
        let (uri, mime) = (field("uri"), field("mimeType"));
        if let Some(text) = content.get("text").and_then(Value::as_str) {
            parts.push(format!("Resource {uri} ({mime}):\n{text}"));
            continue;
        }
        let Some(blob) = content.get("blob").and_then(Value::as_str) else {
            parts.push(format!("(resource {uri} has no contents)"));
            continue;
        };
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(blob)
            .unwrap_or_default();
        let picture = mime
            .starts_with("image/")
            .then(|| image::guess_format(&bytes).ok())
            .flatten()
            .and_then(|format| {
                let picture = image::load_from_memory_with_format(&bytes, format).ok()?;
                crate::picture::prepare(&picture, format, None, images, uri).ok()
            });
        match picture {
            Some((image, line)) => {
                parts.push(line);
                shown.push(image);
            }
            None => parts.push(format!(
                "(resource {uri}: {mime}, {} bytes, not shown)",
                bytes.len()
            )),
        }
    }
    (parts.join("\n\n"), shown)
}

/// A prompt of an MCP server, run as the slash command `/<name>`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PromptCommand {
    /// `mcp__<server>__<prompt>`, made like a tool's name.
    pub name: String,
    pub server: String,
    pub prompt: String,
    pub description: String,
    pub arguments: Vec<PromptArgument>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PromptArgument {
    pub name: String,
    pub description: Option<String>,
    pub required: bool,
}

/// The prompts of the servers that offer prompts, as commands: of the
/// servers already started, or, with `start`, of all, started for it. A
/// prompt whose command name is taken is left out.
pub(crate) async fn prompt_commands(store: &Store, start: bool) -> Vec<PromptCommand> {
    let tools = match start {
        true => Some(loaded(store).await),
        false => store.mcp().lock().await.clone(),
    };
    let Some(tools) = tools else {
        return Vec::new();
    };
    let offering: Vec<&super::Served> = tools
        .servers
        .iter()
        .filter(|served| served.slot.current().offers("/prompts"))
        .collect();
    let lists = futures_util::future::join_all(
        offering
            .iter()
            .map(|served| served.slot.list("prompts/list", "prompts")),
    )
    .await;
    let mut commands: Vec<PromptCommand> = Vec::new();
    for (served, list) in offering.iter().zip(lists) {
        let prompts = match list {
            Ok(prompts) => prompts,
            Err(fault) => {
                eprintln!(
                    "MCP: the prompts of server {} could not be listed: {fault}",
                    served.name
                );
                continue;
            }
        };
        for prompt in prompts {
            let Some(original) = prompt.get("name").and_then(Value::as_str) else {
                continue;
            };
            let name = tool_name(&served.name, original);
            if commands.iter().any(|command| command.name == name) {
                eprintln!(
                    "MCP: prompt {original} of server {} is left out: the name {name} is taken",
                    served.name
                );
                continue;
            }
            let text = |key: &str| prompt.get(key).and_then(Value::as_str);
            let arguments = prompt
                .get("arguments")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|argument| {
                    Some(PromptArgument {
                        name: argument.get("name")?.as_str()?.to_string(),
                        description: argument
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        required: argument.get("required") == Some(&json!(true)),
                    })
                })
                .collect();
            commands.push(PromptCommand {
                name,
                server: served.name.clone(),
                prompt: original.to_string(),
                description: text("description")
                    .or_else(|| text("title"))
                    .unwrap_or("")
                    .to_string(),
                arguments,
            });
        }
    }
    commands
}

/// The prompt's arguments from the text typed after the command: one word
/// each, in the order they are declared, the last taking the rest.
fn bind_arguments(
    declared: &[PromptArgument],
    text: &str,
) -> Result<serde_json::Map<String, Value>, String> {
    let mut bound = serde_json::Map::new();
    let mut rest = text.trim();
    for (index, argument) in declared.iter().enumerate() {
        if rest.is_empty() {
            break;
        }
        let value = match index + 1 == declared.len() {
            true => std::mem::take(&mut rest),
            false => {
                let (word, after) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                rest = after.trim_start();
                word
            }
        };
        bound.insert(argument.name.clone(), json!(value));
    }
    if !rest.is_empty() {
        return Err(format!("the prompt takes no arguments, not \"{rest}\""));
    }
    let missing: Vec<String> = declared
        .iter()
        .filter(|argument| argument.required && !bound.contains_key(&argument.name))
        .map(|argument| format!("<{}>", argument.name))
        .collect();
    match missing.is_empty() {
        true => Ok(bound),
        false => Err(format!("needs {}", missing.join(" "))),
    }
}

/// Gets the prompt of `command` with the arguments typed as `text`; its
/// messages as the text of the person's message that starts the turn.
pub(crate) async fn get_prompt(
    store: &Store,
    command: &PromptCommand,
    text: &str,
) -> Result<String, ErrorInfo> {
    let arguments = bind_arguments(&command.arguments, text)
        .map_err(|message| ErrorInfo::new("invalid_arguments", message))?;
    let tools = loaded(store).await;
    let connection = offering(&tools, &command.server, "/prompts", "prompts")
        .map_err(|message| ErrorInfo::new("mcp_error", message))?;
    let call = Call {
        store,
        connection: &connection,
        method: "prompts/get",
        name: &command.prompt,
        asking: None,
    };
    let (_keep, cancel) = tokio::sync::watch::channel(false);
    let result = call
        .result(
            json!({"name": command.prompt, "arguments": arguments}),
            &cancel,
        )
        .await
        .map_err(|fault| ErrorInfo::new("mcp_error", fault.to_string()))?;
    Ok(prompt_text(&result))
}

/// A prompt's messages as text: the person's message can hold only text,
/// so images, audio and binary resources are named with their type and
/// size, and messages the prompt gives as the assistant's are marked so.
fn prompt_text(result: &Value) -> String {
    let mut messages = Vec::new();
    for message in result
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let content = message.get("content").unwrap_or(&Value::Null);
        let text = block_text(content);
        messages.push(match message.get("role").and_then(Value::as_str) {
            Some("assistant") => format!("(the prompt's assistant message)\n{text}"),
            _ => text,
        });
    }
    messages.join("\n\n")
}

/// One content block of a prompt message as text.
fn block_text(block: &Value) -> String {
    let field = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    match field(block, "type").as_str() {
        "text" => field(block, "text"),
        kind @ ("image" | "audio") => format!(
            "({kind} of type {}, {} bytes, not shown)",
            field(block, "mimeType"),
            super::decoded_len(&field(block, "data"))
        ),
        "resource_link" => format!(
            "(resource {}: {})",
            field(block, "name"),
            field(block, "uri")
        ),
        "resource" => {
            let resource = block.get("resource").unwrap_or(&Value::Null);
            let uri = field(resource, "uri");
            match resource.get("text").and_then(Value::as_str) {
                Some(text) => format!("Resource {uri}:\n{text}"),
                None => format!(
                    "(resource {uri}: {}, {} bytes, not shown)",
                    field(resource, "mimeType"),
                    super::decoded_len(&field(resource, "blob"))
                ),
            }
        }
        kind => format!("(content of type {kind} not shown)"),
    }
}

/// The requests a client sends about the servers' resources and
/// completions: `mcp/resources/list` and `mcp/resources/templates/list`
/// (`server` optional), `mcp/resources/read` (`server`, `uri`) and
/// `mcp/complete` (`server`, `ref`, `argument`, `context` as in
/// `completion/complete`). Servers not started yet are started.
pub async fn request(store: &Store, method: &str, params: &Value) -> Result<Value, ErrorInfo> {
    let text = |key: &str| params.get(key).and_then(Value::as_str);
    let needs = |key: &str| {
        text(key)
            .ok_or_else(|| ErrorInfo::new("invalid_params", format!("requires a \"{key}\" string")))
    };
    let failed = |message: String| ErrorInfo::new("mcp_error", message);
    let tools = loaded(store).await;
    match method {
        "mcp/resources/list" => listed(&tools, text("server"), "resources/list", "resources")
            .await
            .map_err(failed),
        "mcp/resources/templates/list" => listed(
            &tools,
            text("server"),
            "resources/templates/list",
            "resourceTemplates",
        )
        .await
        .map_err(failed),
        "mcp/resources/read" => {
            let (server, uri) = (needs("server")?, needs("uri")?);
            let (_keep, cancel) = tokio::sync::watch::channel(false);
            let mut result = read(store, &tools, server, uri, None, &cancel)
                .await
                .map_err(failed)?;
            result["server"] = json!(server);
            Ok(result)
        }
        "mcp/complete" => {
            let server = needs("server")?;
            let reference = params.get("ref").filter(|reference| reference.is_object());
            let argument = params
                .get("argument")
                .filter(|argument| argument.is_object());
            let (Some(reference), Some(argument)) = (reference, argument) else {
                return Err(ErrorInfo::new(
                    "invalid_params",
                    "requires \"ref\" and \"argument\" objects",
                ));
            };
            let connection =
                offering(&tools, server, "/completions", "completions").map_err(failed)?;
            let name = reference
                .get("name")
                .or_else(|| reference.get("uri"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let mut asked = json!({"ref": reference, "argument": argument});
            if let Some(context) = params.get("context").filter(|context| context.is_object()) {
                asked["context"] = context.clone();
            }
            let call = Call {
                store,
                connection: &connection,
                method: "completion/complete",
                name,
                asking: None,
            };
            let (_keep, cancel) = tokio::sync::watch::channel(false);
            let result = call
                .result(asked, &cancel)
                .await
                .map_err(|fault: Fault| failed(fault.to_string()))?;
            Ok(json!({"server": server, "completion": result.get("completion")}))
        }
        _ => Err(ErrorInfo::new(
            "unknown_method",
            format!("no method {method}"),
        )),
    }
}

/// The updates of one resource of one server, until dropped.
pub struct ResourceUpdates {
    server: String,
    subscription: connection::Subscription,
}

impl ResourceUpdates {
    /// The next update, as `{server, uri}`; `None` once the server stopped.
    pub async fn next(&mut self) -> Option<Value> {
        let uri = self.subscription.updates.recv().await?;
        Some(json!({"server": self.server, "uri": uri}))
    }
}

/// Subscribes to the updates of the resource `uri` of `server`, for
/// `mcp/resources/subscribe`.
pub async fn subscribe(store: &Store, params: &Value) -> Result<ResourceUpdates, ErrorInfo> {
    let text = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| ErrorInfo::new("invalid_params", format!("requires a \"{key}\" string")))
    };
    let (server, uri) = (text("server")?, text("uri")?);
    let tools = loaded(store).await;
    let connection = offering(&tools, server, "/resources", "resources")
        .map_err(|message| ErrorInfo::new("mcp_error", message))?;
    let subscription = connection
        .subscribe(uri)
        .await
        .map_err(|fault| ErrorInfo::new("mcp_error", fault.to_string()))?;
    Ok(ResourceUpdates {
        server: server.to_string(),
        subscription,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argument(name: &str, required: bool) -> PromptArgument {
        PromptArgument {
            name: name.into(),
            description: None,
            required,
        }
    }

    #[test]
    fn typed_words_become_the_prompts_arguments() {
        let declared = [argument("file", true), argument("focus", false)];
        assert_eq!(
            bind_arguments(&declared, "src/a.rs  error handling and tests"),
            Ok(serde_json::Map::from_iter([
                ("file".to_string(), json!("src/a.rs")),
                ("focus".to_string(), json!("error handling and tests")),
            ]))
        );
        assert_eq!(
            bind_arguments(&declared, "src/a.rs"),
            Ok(serde_json::Map::from_iter([(
                "file".to_string(),
                json!("src/a.rs")
            )]))
        );
        let both = [argument("a", true), argument("b", true)];
        assert_eq!(bind_arguments(&both, ""), Err("needs <a> <b>".into()));
        assert_eq!(bind_arguments(&both, "x"), Err("needs <b>".into()));
        assert_eq!(
            bind_arguments(&[], "more"),
            Err("the prompt takes no arguments, not \"more\"".into())
        );
        assert_eq!(bind_arguments(&[], " "), Ok(serde_json::Map::new()));
    }

    #[test]
    fn prompt_messages_become_text() {
        let result = json!({"messages": [
            {"role": "user", "content": {"type": "text", "text": "Review this."}},
            {"role": "user", "content": {"type": "resource", "resource": {
                "uri": "file:///a.rs", "mimeType": "text/x-rust", "text": "fn a() {}"}}},
            {"role": "user", "content": {"type": "image", "data": "AAAA", "mimeType": "image/png"}},
            {"role": "assistant", "content": {"type": "text", "text": "I will."}},
        ]});
        assert_eq!(
            prompt_text(&result),
            "Review this.\n\nResource file:///a.rs:\nfn a() {}\n\n\
             (image of type image/png, 3 bytes, not shown)\n\n\
             (the prompt's assistant message)\nI will."
        );
    }

    #[test]
    fn a_resources_text_and_images_reach_the_model() {
        use base64::Engine;
        let dir = tempfile::tempdir().expect("dir");
        let mut png = Vec::new();
        image::DynamicImage::new_rgb8(4, 3)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("png");
        let blob = base64::engine::general_purpose::STANDARD.encode(&png);
        let result = json!({"contents": [
            {"uri": "file:///a.txt", "mimeType": "text/plain", "text": "hello"},
            {"uri": "file:///a.png", "mimeType": "image/png", "blob": blob},
            {"uri": "file:///a.bin", "mimeType": "application/octet-stream", "blob": "AAAAAA=="},
        ]});
        let (text, images) = contents_for_model(&result, dir.path());
        assert_eq!(images.len(), 1);
        assert_eq!((images[0].width, images[0].height), (4, 3));
        assert_eq!(
            text,
            "Resource file:///a.txt (text/plain):\nhello\n\n\
             Image file:///a.png: 4×3 pixels; shown at 4×3.\n\n\
             (resource file:///a.bin: application/octet-stream, 4 bytes, not shown)"
        );
    }
}
