use anyhow::Context;
use harness_agents::{claude::ClaudeCodeAgent, codex::CodexAgent, AgentRegistry};
use harness_core::{prompts, AgentRequest, HarnessConfig, ThreadId};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::RwLock;

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const JSONRPC_PARSE_ERROR: i32 = -32700;
const JSONRPC_INVALID_REQUEST: i32 = -32600;
const JSONRPC_METHOD_NOT_FOUND: i32 = -32601;
const JSONRPC_INVALID_PARAMS: i32 = -32602;

#[async_trait::async_trait]
trait PromptExecutor: Send + Sync {
    async fn execute(
        &self,
        agent: &str,
        project_root: PathBuf,
        prompt: String,
    ) -> anyhow::Result<String>;
}

struct RegistryExecutor {
    agent_registry: Arc<AgentRegistry>,
}

impl RegistryExecutor {
    fn new(agent_registry: Arc<AgentRegistry>) -> Self {
        Self { agent_registry }
    }
}

#[async_trait::async_trait]
impl PromptExecutor for RegistryExecutor {
    async fn execute(
        &self,
        agent: &str,
        project_root: PathBuf,
        prompt: String,
    ) -> anyhow::Result<String> {
        let code_agent = self.agent_registry.get(agent).with_context(|| {
            let available = self.agent_registry.list().join(", ");
            format!("unknown agent `{agent}` (available: [{available}])")
        })?;

        let response = code_agent
            .execute(AgentRequest {
                prompt,
                project_root,
                ..Default::default()
            })
            .await?;
        Ok(response.output)
    }
}

#[derive(Debug, Clone)]
struct SessionTurn {
    user_prompt: String,
    assistant_output: String,
}

#[derive(Debug, Clone)]
struct SessionState {
    project_root: PathBuf,
    agent: String,
    turns: Vec<SessionTurn>,
}

struct McpServer {
    default_agent: String,
    sessions: Arc<RwLock<HashMap<String, SessionState>>>,
    executor: Arc<dyn PromptExecutor>,
}

impl McpServer {
    fn new(default_agent: String, executor: Arc<dyn PromptExecutor>) -> Self {
        Self {
            default_agent,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            executor,
        }
    }

    async fn serve_stdio(&self) -> anyhow::Result<()> {
        let stdin = tokio::io::stdin();
        let mut lines = BufReader::new(stdin).lines();
        let mut stdout = tokio::io::stdout();

        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }

            let request: Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(error) => {
                    let response = jsonrpc_response(
                        Some(Value::Null),
                        None,
                        Some(jsonrpc_error_payload(
                            JSONRPC_PARSE_ERROR,
                            format!("parse error: {error}"),
                        )),
                    );
                    write_json_line(&mut stdout, &response).await?;
                    continue;
                }
            };

            if let Some(response) = self.handle_request(request).await {
                write_json_line(&mut stdout, &response).await?;
            }
        }

        Ok(())
    }

    async fn handle_request(&self, request: Value) -> Option<Value> {
        let id = request.get("id").cloned();
        let method = match request.get("method").and_then(Value::as_str) {
            Some(method) => method,
            None => {
                return jsonrpc_error_response(
                    id,
                    JSONRPC_INVALID_REQUEST,
                    "missing `method` in request",
                );
            }
        };

        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));

        match method {
            "initialize" => jsonrpc_success_response(
                id,
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {
                        "tools": {
                            "listChanged": false
                        }
                    },
                    "serverInfo": {
                        "name": "harness",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }),
            ),
            "notifications/initialized" => {
                if id.is_some() {
                    jsonrpc_success_response(id, json!({}))
                } else {
                    None
                }
            }
            "ping" => jsonrpc_success_response(id, json!({})),
            "tools/list" => jsonrpc_success_response(id, json!({ "tools": mcp_tools() })),
            "tools/call" => {
                let call_params: ToolCallParams = match serde_json::from_value(params) {
                    Ok(value) => value,
                    Err(error) => {
                        return jsonrpc_error_response(
                            id,
                            JSONRPC_INVALID_PARAMS,
                            format!("invalid tools/call params: {error}"),
                        );
                    }
                };

                let arguments = call_params.arguments.unwrap_or_else(|| json!({}));
                let result = self.call_tool(call_params.name, arguments).await;
                jsonrpc_success_response(id, result)
            }
            other if other.starts_with("notifications/") => None,
            _ => jsonrpc_error_response(
                id,
                JSONRPC_METHOD_NOT_FOUND,
                format!("method not found: {method}"),
            ),
        }
    }

    async fn call_tool(&self, tool_name: String, arguments: Value) -> Value {
        match tool_name.as_str() {
            "harness" => self.run_harness_tool(arguments).await,
            "harness-reply" => self.run_harness_reply_tool(arguments).await,
            _ => tool_error_result(format!("unknown tool `{tool_name}`")),
        }
    }

    async fn run_harness_tool(&self, arguments: Value) -> Value {
        let args: HarnessToolArgs = match serde_json::from_value(arguments) {
            Ok(value) => value,
            Err(error) => return tool_error_result(format!("invalid `harness` args: {error}")),
        };

        if args.prompt.trim().is_empty() {
            return tool_error_result("`prompt` must not be empty");
        }

        let project_root = match resolve_project_root(args.project_root) {
            Ok(path) => path,
            Err(error) => return tool_error_result(error.to_string()),
        };

        let agent = args
            .agent
            .as_deref()
            .unwrap_or(&self.default_agent)
            .to_string();

        let prompt = prompts::wrap_external_data(&args.prompt);
        let output = match self
            .executor
            .execute(&agent, project_root.clone(), prompt)
            .await
        {
            Ok(value) => value,
            Err(error) => return tool_error_result(format!("`harness` execution failed: {error}")),
        };

        let thread_id = ThreadId::new().to_string();
        let session = SessionState {
            project_root: project_root.clone(),
            agent: agent.clone(),
            turns: vec![SessionTurn {
                user_prompt: args.prompt,
                assistant_output: output.clone(),
            }],
        };
        self.sessions
            .write()
            .await
            .insert(thread_id.clone(), session);

        tool_success_result(
            format!("thread_id={thread_id}\n\n{output}"),
            json!({
                "thread_id": thread_id,
                "output": output,
                "agent": agent,
                "project_root": project_root.display().to_string(),
            }),
        )
    }

    async fn run_harness_reply_tool(&self, arguments: Value) -> Value {
        let args: HarnessReplyToolArgs = match serde_json::from_value(arguments) {
            Ok(value) => value,
            Err(error) => {
                return tool_error_result(format!("invalid `harness-reply` args: {error}"));
            }
        };

        if args.prompt.trim().is_empty() {
            return tool_error_result("`prompt` must not be empty");
        }

        let existing = {
            let sessions = self.sessions.read().await;
            sessions.get(&args.thread_id).cloned()
        };

        let Some(existing) = existing else {
            return tool_error_result(format!("thread `{}` not found", args.thread_id));
        };

        let prompt = compose_reply_prompt(&existing.turns, &args.prompt);
        let output = match self
            .executor
            .execute(&existing.agent, existing.project_root.clone(), prompt)
            .await
        {
            Ok(value) => value,
            Err(error) => {
                return tool_error_result(format!("`harness-reply` execution failed: {error}"));
            }
        };

        {
            let mut sessions = self.sessions.write().await;
            if let Some(session) = sessions.get_mut(&args.thread_id) {
                session.turns.push(SessionTurn {
                    user_prompt: args.prompt.clone(),
                    assistant_output: output.clone(),
                });
            }
        }

        tool_success_result(
            format!("thread_id={}\n\n{}", args.thread_id, output),
            json!({
                "thread_id": args.thread_id,
                "output": output,
                "agent": existing.agent,
                "project_root": existing.project_root.display().to_string(),
            }),
        )
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCallParams {
    name: String,
    #[serde(default)]
    arguments: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessToolArgs {
    prompt: String,
    #[serde(default)]
    project_root: Option<PathBuf>,
    #[serde(default)]
    agent: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessReplyToolArgs {
    thread_id: String,
    prompt: String,
}

fn mcp_tools() -> Vec<Value> {
    vec![
        json!({
            "name": "harness",
            "description": "Start a new harness session and execute a prompt.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "User prompt to execute.",
                    },
                    "project_root": {
                        "type": "string",
                        "description": "Project directory path. Defaults to server cwd.",
                    },
                    "agent": {
                        "type": "string",
                        "description": "Agent name (for example: claude, codex). Defaults to configured default agent.",
                    }
                },
                "required": ["prompt"],
            }
        }),
        json!({
            "name": "harness-reply",
            "description": "Continue an existing harness session by thread ID.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "thread_id": {
                        "type": "string",
                        "description": "Thread ID returned by the harness tool.",
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Follow-up user prompt for this thread.",
                    }
                },
                "required": ["thread_id", "prompt"],
            }
        }),
    ]
}

fn compose_reply_prompt(history: &[SessionTurn], next_prompt: &str) -> String {
    if history.is_empty() {
        return prompts::wrap_external_data(next_prompt);
    }

    let mut transcript = String::from(
        "Continue the conversation using this transcript. Keep prior context consistent.\n\n",
    );
    for (index, turn) in history.iter().enumerate() {
        let step = index + 1;
        transcript.push_str(&format!("User #{step}:\n{}\n\n", turn.user_prompt));
        transcript.push_str(&format!(
            "Assistant #{step}:\n{}\n\n",
            turn.assistant_output
        ));
    }
    transcript.push_str(&format!("User #{}:\n{}", history.len() + 1, next_prompt));
    prompts::wrap_external_data(&transcript)
}

fn resolve_project_root(project_root: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to resolve current working directory")?;
    let path = project_root.unwrap_or_else(|| cwd.clone());
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(cwd.join(path))
    }
}

fn jsonrpc_success_response(id: Option<Value>, result: Value) -> Option<Value> {
    match id {
        Some(request_id) => Some(jsonrpc_response(Some(request_id), Some(result), None)),
        None => None,
    }
}

fn jsonrpc_error_response(
    id: Option<Value>,
    code: i32,
    message: impl Into<String>,
) -> Option<Value> {
    match id {
        Some(request_id) => Some(jsonrpc_response(
            Some(request_id),
            None,
            Some(jsonrpc_error_payload(code, message)),
        )),
        None => None,
    }
}

fn jsonrpc_response(id: Option<Value>, result: Option<Value>, error: Option<Value>) -> Value {
    let mut response = serde_json::Map::new();
    response.insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
    response.insert("id".to_string(), id.unwrap_or(Value::Null));
    if let Some(result) = result {
        response.insert("result".to_string(), result);
    }
    if let Some(error) = error {
        response.insert("error".to_string(), error);
    }
    Value::Object(response)
}

fn jsonrpc_error_payload(code: i32, message: impl Into<String>) -> Value {
    json!({
        "code": code,
        "message": message.into(),
    })
}

fn tool_success_result(text: String, structured_content: Value) -> Value {
    json!({
        "content": [
            {
                "type": "text",
                "text": text,
            }
        ],
        "structuredContent": structured_content,
        "isError": false,
    })
}

fn tool_error_result(message: impl Into<String>) -> Value {
    json!({
        "content": [
            {
                "type": "text",
                "text": message.into(),
            }
        ],
        "isError": true,
    })
}

async fn write_json_line(stdout: &mut tokio::io::Stdout, value: &Value) -> anyhow::Result<()> {
    let line = serde_json::to_string(value)?;
    stdout.write_all(line.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(())
}

pub async fn run(config: HarnessConfig) -> anyhow::Result<()> {
    let mut agent_registry = AgentRegistry::new(&config.agents.default_agent);
    agent_registry.register(
        "claude",
        Arc::new(ClaudeCodeAgent::new(
            config.agents.claude.cli_path.clone(),
            config.agents.claude.default_model.clone(),
            config.agents.sandbox_mode,
        )),
    );
    agent_registry.register(
        "codex",
        Arc::new(CodexAgent::new(
            config.agents.codex.cli_path.clone(),
            config.agents.sandbox_mode,
        )),
    );

    let executor = Arc::new(RegistryExecutor::new(Arc::new(agent_registry)));
    let server = McpServer::new(config.agents.default_agent.clone(), executor);
    server.serve_stdio().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tokio::sync::Mutex;

    #[derive(Debug, Clone)]
    struct MockExecutionCall {
        agent: String,
        project_root: PathBuf,
        prompt: String,
    }

    #[derive(Default)]
    struct MockExecutor {
        calls: Mutex<Vec<MockExecutionCall>>,
    }

    #[async_trait::async_trait]
    impl PromptExecutor for MockExecutor {
        async fn execute(
            &self,
            agent: &str,
            project_root: PathBuf,
            prompt: String,
        ) -> anyhow::Result<String> {
            self.calls.lock().await.push(MockExecutionCall {
                agent: agent.to_string(),
                project_root,
                prompt: prompt.clone(),
            });
            Ok(format!("mock-output::{agent}::{prompt}"))
        }
    }

    fn make_request(id: i64, method: &str, params: Value) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })
    }

    fn extract_result(response: Value) -> Value {
        response
            .get("result")
            .cloned()
            .expect("response has result")
    }

    #[tokio::test]
    async fn tools_list_returns_harness_and_reply() {
        let executor = Arc::new(MockExecutor::default());
        let server = McpServer::new("mock-default".to_string(), executor);

        let response = server
            .handle_request(make_request(1, "tools/list", json!({})))
            .await
            .expect("tools/list should respond");
        let tools = extract_result(response)
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .expect("tools array");

        let names = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["harness", "harness-reply"]);
    }

    #[tokio::test]
    async fn harness_then_reply_reuses_thread_and_history() {
        let executor = Arc::new(MockExecutor::default());
        let server = McpServer::new("mock-default".to_string(), executor.clone());

        let first = server
            .handle_request(make_request(
                1,
                "tools/call",
                json!({
                    "name": "harness",
                    "arguments": {
                        "prompt": "hello",
                        "project_root": ".",
                        "agent": "mock-agent",
                    }
                }),
            ))
            .await
            .expect("harness call should respond");
        let first_result = extract_result(first);
        assert_eq!(first_result["isError"], Value::Bool(false));
        let thread_id = first_result["structuredContent"]["thread_id"]
            .as_str()
            .expect("thread_id in structuredContent")
            .to_string();

        let second = server
            .handle_request(make_request(
                2,
                "tools/call",
                json!({
                    "name": "harness-reply",
                    "arguments": {
                        "thread_id": thread_id,
                        "prompt": "continue",
                    }
                }),
            ))
            .await
            .expect("harness-reply should respond");
        let second_result = extract_result(second);
        assert_eq!(second_result["isError"], Value::Bool(false));
        assert!(second_result["structuredContent"]["output"]
            .as_str()
            .expect("output text")
            .contains("continue"));

        let calls = executor.calls.lock().await.clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].agent, "mock-agent");
        assert!(calls[1].prompt.contains("User #1"));
        assert!(calls[1].prompt.contains("continue"));
        assert!(!calls[0].project_root.as_os_str().is_empty());
    }

    #[tokio::test]
    async fn unknown_tool_returns_tool_error() {
        let executor = Arc::new(MockExecutor::default());
        let server = McpServer::new("mock-default".to_string(), executor);

        let response = server
            .handle_request(make_request(
                1,
                "tools/call",
                json!({
                    "name": "missing-tool",
                    "arguments": {},
                }),
            ))
            .await
            .expect("tools/call should respond");
        let result = extract_result(response);
        assert_eq!(result["isError"], Value::Bool(true));
        assert!(result["content"][0]["text"]
            .as_str()
            .expect("error text")
            .contains("unknown tool"));
    }
}
