use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolEnvironments;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::LiveEnvironmentResolver;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolExecutor;
use crate::unified_exec::terminal_status_report;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;

use super::super::shell_spec::create_terminal_status_tool;

#[derive(Debug, Deserialize)]
struct TerminalStatusArgs {
    #[serde(default)]
    kind: Option<String>,
}

pub struct TerminalStatusHandler;

impl ToolExecutor<ToolInvocation> for TerminalStatusHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("terminal_status")
    }

    fn spec(&self) -> ToolSpec {
        create_terminal_status_tool()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolInvocation {
                environments,
                payload,
                ..
            } = invocation;
            let ToolEnvironments::Terminal(terminal) = environments else {
                return Err(FunctionCallError::Fatal(
                    "terminal_status requires a terminal session".to_string(),
                ));
            };
            let ToolPayload::Function { arguments } = &payload else {
                return Err(FunctionCallError::RespondToModel(
                    "terminal_status handler received unsupported payload".to_string(),
                ));
            };
            let args: TerminalStatusArgs = parse_arguments(arguments)?;
            let report = terminal_status_report(&terminal, args.kind.as_deref())
                .await
                .map_err(FunctionCallError::RespondToModel)?;
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                report,
                Some(true),
            )))
        })
    }
}

impl CoreToolRuntime for TerminalStatusHandler {
    fn live_environment_resolver(&self) -> Option<LiveEnvironmentResolver> {
        Some(super::terminal_session_for_input)
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    fn pre_tool_use_payload(&self, _invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        // Read-only status query of an existing session; nothing runs.
        None
    }
}
