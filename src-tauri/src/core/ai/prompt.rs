use crate::config::AiSettings;
use std::collections::HashMap;
use std::sync::OnceLock;

use super::types::{AiAction, AiChatRequest, CommandObservation};

const SYSTEM_PROMPT_ZH: &str = r#"你是一个专业、谨慎、安全优先的 Linux / DevOps / 云原生终端助手。
你的任务是帮助用户解释终端输出、生成 Shell 命令、分析错误、提供排查步骤。

必须遵守：
1. 不要建议不可逆高危操作，除非明确说明风险和安全替代方案。
2. 默认生成只读诊断命令。
3. 对任何删除、格式化、重启、停服务、改权限、批量变更命令标记风险。
4. 命令必须适配用户当前系统、架构、shell 和权限上下文。
5. 输出必须结构化，包含命令、说明、风险等级、影响范围和回滚建议。
6. 不要编造当前系统不存在的信息；不确定时给出验证命令。
7. 不要要求用户粘贴密码、私钥、token。

只返回一个 JSON 对象，不要使用 Markdown 代码块。格式：
{
  "text": "给用户看的说明",
  "commandCards": [
    {
      "id": "cmd-uuid",
      "title": "标题",
      "command": "shell command",
      "explanation": "命令说明",
      "riskLevel": "low|medium|high|critical",
      "riskReason": "风险原因",
      "expectedEffect": "预计影响",
      "rollback": "回滚方式或无需回滚",
      "category": "Linux 性能"
    }
  ]
}"#;

const SYSTEM_PROMPT_EN: &str = r#"You are a professional, careful, safety-first Linux / DevOps / cloud-native terminal assistant.
Your job is to explain terminal output, generate Shell commands, analyze errors, and suggest next troubleshooting steps.

You must follow these rules:
1. Do not suggest irreversible high-risk actions unless you clearly explain the risk and provide safer alternatives.
2. Prefer read-only diagnostic commands by default.
3. Mark any delete, format, restart, stop-service, permission-change, or bulk-change command with the appropriate risk.
4. Commands must fit the user's current system, architecture, shell, and privilege context.
5. Output must be structured and include commands, explanations, risk level, expected effect, and rollback guidance.
6. Do not invent facts about the current system. If uncertain, provide verification commands.
7. Do not ask the user to paste passwords, private keys, or tokens.

Return exactly one JSON object and do not use Markdown code fences. Format:
{
  "text": "user-facing explanation",
  "commandCards": [
    {
      "id": "cmd-uuid",
      "title": "title",
      "command": "shell command",
      "explanation": "command explanation",
      "riskLevel": "low|medium|high|critical",
      "riskReason": "why this risk applies",
      "expectedEffect": "expected effect",
      "rollback": "rollback steps or state that rollback is unnecessary",
      "category": "Linux performance"
    }
  ]
}"#;

const AGENT_SYSTEM_PROMPT_ZH: &str = r#"你是一个终端自动化 Agent，通过"思考—执行—观察"循环完成用户的任务。

每一轮你只能做一件事：调用 execute_command 工具执行一条命令，或调用 final_answer 工具给出最终回答。

规则：
1. 每轮必须且只能调用一个工具，不要在普通正文里输出 JSON。
2. 如果需要执行命令，调用 execute_command。
3. 任务完成或无需执行命令时，调用 final_answer。
4. thought 和 answer 尽量使用用户请求指定的目标语言。
5. 优先使用只读命令收集信息，再做修改操作。
6. 不要执行不可逆高危命令（如 rm -rf /、mkfs、停止 SSH 等），改为在 thought 中说明风险并调用 final_answer。
7. 不要编造信息；不确定时先用验证命令确认。
8. 不要要求用户提供密码、私钥、token。
9. 命令必须适配用户当前的系统和 shell 环境。
10. riskLevel 规则：只读命令 → low，普通写操作 → medium，删除/重启/权限修改 → high，不可逆破坏 → critical。
11. 调用 execute_command 时必须同时提供 riskLevel 和 riskReason；riskReason 要简短说明为什么这样分级。"#;

const AGENT_SYSTEM_PROMPT_EN: &str = r"You are a terminal automation agent that completes tasks using a think-execute-observe loop.

In each turn, do exactly one thing: call the execute_command tool to execute one command, or call the final_answer tool to finish.

Rules:
1. You must call exactly one tool per turn. Do not put protocol JSON in normal assistant text.
2. If a command must be executed, call execute_command.
3. If the task is complete or no command is needed, call final_answer.
4. Use the target language requested by the user for both thought and answer whenever possible.
5. Prefer read-only commands to gather information before making changes.
6. Do not execute irreversible high-risk commands (for example rm -rf /, mkfs, or stopping SSH). Explain the risk in thought and call final_answer instead.
7. Do not invent facts. If uncertain, verify first.
8. Do not ask the user for passwords, private keys, or tokens.
9. Commands must fit the user's current system and shell environment.
10. riskLevel guidance: read-only commands -> low, normal write actions -> medium, delete/restart/permission changes -> high, irreversible destructive actions -> critical.
11. execute_command calls must include both riskLevel and riskReason. Keep riskReason brief and explain why the risk applies.";

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum PromptLanguage {
    ZhCn,
    En,
}

fn normalize_prompt_locale(language: &str) -> String {
    let normalized = language.trim().replace('_', "-").to_ascii_lowercase();
    match normalized.as_str() {
        "zh" | "zh-cn" | "zh-hans" | "zh-hans-cn" => "zh-cn".to_string(),
        "en" | "en-us" | "en-gb" => "en".to_string(),
        _ => normalized,
    }
}

fn prompt_language_map() -> &'static HashMap<&'static str, PromptLanguage> {
    static PROMPT_LANGUAGE_MAP: OnceLock<HashMap<&'static str, PromptLanguage>> = OnceLock::new();
    PROMPT_LANGUAGE_MAP.get_or_init(|| {
        HashMap::from([("zh-cn", PromptLanguage::ZhCn), ("en", PromptLanguage::En)])
    })
}

fn resolve_prompt_language(language: &str) -> PromptLanguage {
    let normalized = normalize_prompt_locale(language);
    prompt_language_map()
        .get(normalized.as_str())
        .copied()
        .unwrap_or(PromptLanguage::En)
}

pub(super) fn system_prompt(language: &str) -> &'static str {
    match resolve_prompt_language(language) {
        PromptLanguage::ZhCn => SYSTEM_PROMPT_ZH,
        PromptLanguage::En => SYSTEM_PROMPT_EN,
    }
}

pub(super) fn agent_system_prompt(language: &str) -> &'static str {
    match resolve_prompt_language(language) {
        PromptLanguage::ZhCn => AGENT_SYSTEM_PROMPT_ZH,
        PromptLanguage::En => AGENT_SYSTEM_PROMPT_EN,
    }
}

pub(super) fn build_agent_prompt(request: &AiChatRequest, settings: &AiSettings) -> String {
    let ctx = &request.context;
    if resolve_prompt_language(&request.options.language) == PromptLanguage::ZhCn {
        format!(
            r"用户任务：
{user_input}

当前连接上下文：
- 连接名：{connection_name}
- 主机：{host}
- 用户：{username}
- 当前目录：{cwd}
- 操作系统：{os}
- 架构：{arch}

最近终端输出（最多 {line_limit} 行）：
{recent_output}

要求：
- 面向用户的说明、总结以及推理过程使用：{language}
- 命令、路径、文件名、配置键名保持原样，不要翻译

请开始执行任务。每轮调用且只调用一个工具。",
            user_input = request.user_input,
            connection_name = ctx.connection_name.as_deref().unwrap_or("-"),
            host = ctx.host.as_deref().unwrap_or("-"),
            username = ctx.username.as_deref().unwrap_or("-"),
            cwd = ctx.cwd.as_deref().unwrap_or("-"),
            os = ctx.os.as_deref().unwrap_or("-"),
            arch = ctx.arch.as_deref().unwrap_or(std::env::consts::ARCH),
            line_limit = settings.context_line_limit,
            recent_output = ctx.recent_output,
            language = request.options.language,
        )
    } else {
        format!(
            r"User task:
{user_input}

Current connection context:
- Connection name: {connection_name}
- Host: {host}
- User: {username}
- Current directory: {cwd}
- Operating system: {os}
- Architecture: {arch}

Recent terminal output (up to {line_limit} lines):
{recent_output}

Requirements:
- Use {language} for user-facing explanations and summaries.
- Prefer {language} for reasoning when possible.
- Keep commands, paths, file names, and configuration keys unchanged.

Start the task now. Call exactly one tool per turn.",
            user_input = request.user_input,
            connection_name = ctx.connection_name.as_deref().unwrap_or("-"),
            host = ctx.host.as_deref().unwrap_or("-"),
            username = ctx.username.as_deref().unwrap_or("-"),
            cwd = ctx.cwd.as_deref().unwrap_or("-"),
            os = ctx.os.as_deref().unwrap_or("-"),
            arch = ctx.arch.as_deref().unwrap_or(std::env::consts::ARCH),
            line_limit = settings.context_line_limit,
            recent_output = ctx.recent_output,
            language = request.options.language,
        )
    }
}

pub(super) fn build_observation_message(
    obs: &CommandObservation,
    command: &str,
    language: &str,
) -> String {
    let status = obs
        .exit_code
        .map(|c| format!("exit code {c}"))
        .unwrap_or_else(|| "unknown exit code".to_string());
    let output = if obs.output.len() > 8000 {
        let truncated = &obs.output[obs.output.len() - 8000..];
        format!("...(truncated)\n{truncated}")
    } else {
        obs.output.clone()
    };
    if resolve_prompt_language(language) == PromptLanguage::ZhCn {
        format!(
            "命令 `{command}` 执行完成（{status}，耗时 {duration}ms）。\n\n输出：\n{output}\n\n请根据观察结果决定下一步。每轮必须且只能调用一个工具：execute_command 或 final_answer。不要在普通正文里输出 JSON。",
            duration = obs.duration_ms,
        )
    } else {
        format!(
            "Command `{command}` finished ({status}, {duration}ms).\n\nOutput:\n{output}\n\nDecide the next step based on this observation. Call exactly one tool: execute_command or final_answer. Do not put protocol JSON in normal assistant text.",
            duration = obs.duration_ms,
        )
    }
}

pub(super) fn build_prompt(request: &AiChatRequest, settings: &AiSettings) -> String {
    let ctx = &request.context;
    if resolve_prompt_language(&request.options.language) == PromptLanguage::ZhCn {
        let action = match request.action {
            AiAction::GenerateCommand => "根据自然语言需求生成 1 到 2 条 Shell 命令",
            AiAction::ExplainOutput => "解释最近终端输出并给出下一步建议",
            AiAction::ExplainSelected => "解释用户选中的终端文本并给出下一步建议",
            AiAction::AnalyzeError => "分析终端错误输出并给出排查步骤",
            AiAction::RepairFromSelection => "根据选中内容生成修复或排查命令",
            AiAction::CustomTerminalAction => "根据用户配置的终端 AI 功能处理选中内容",
            AiAction::CustomFileAction => "根据用户配置的文件 AI 功能处理文件内容",
        };
        format!(
            r"任务：{action}
用户需求：
{user_input}

当前连接上下文：
- 连接名：{connection_name}
- 主机：{host}
- 端口：{port}
- 用户：{username}
- 当前目录：{cwd}
- 操作系统：{os}
- 架构：{arch}
- 当前输入：{input_buffer}

选中文本：
{selected_text}

最近终端输出（最多 {line_limit} 行）：
{recent_output}

要求：
- 语言：{language}
- 面向用户的说明和推理过程使用该语言；命令、路径、文件名、配置键名保持原样
- 安全模式：{safety_mode}
- 最多生成 {max_commands} 条命令
- 优先生成只读诊断命令
- 如果信息不足，请给出验证命令
- 必须返回 JSON 对象，不要返回 Markdown",
            user_input = request.user_input,
            connection_name = ctx.connection_name.as_deref().unwrap_or("-"),
            host = ctx.host.as_deref().unwrap_or("-"),
            port = ctx
                .port
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_string()),
            username = ctx.username.as_deref().unwrap_or("-"),
            cwd = ctx.cwd.as_deref().unwrap_or("-"),
            os = ctx.os.as_deref().unwrap_or("-"),
            arch = ctx.arch.as_deref().unwrap_or(std::env::consts::ARCH),
            input_buffer = ctx.input_buffer,
            selected_text = ctx.selected_text,
            line_limit = settings.context_line_limit,
            recent_output = ctx.recent_output,
            language = request.options.language,
            safety_mode = request.options.safety_mode,
            max_commands = request.options.max_output_commands,
        )
    } else {
        let action = match request.action {
            AiAction::GenerateCommand => {
                "Generate 1 to 2 Shell commands from the natural language request"
            }
            AiAction::ExplainOutput => {
                "Explain the recent terminal output and suggest the next step"
            }
            AiAction::ExplainSelected => {
                "Explain the selected terminal text and suggest the next step"
            }
            AiAction::AnalyzeError => {
                "Analyze the terminal error output and provide troubleshooting steps"
            }
            AiAction::RepairFromSelection => {
                "Generate repair or troubleshooting commands from the selected content"
            }
            AiAction::CustomTerminalAction => {
                "Handle the selected content using the configured terminal AI action"
            }
            AiAction::CustomFileAction => {
                "Handle the file content using the configured file AI action"
            }
        };
        format!(
            r"Task: {action}
User request:
{user_input}

Current connection context:
- Connection name: {connection_name}
- Host: {host}
- Port: {port}
- User: {username}
- Current directory: {cwd}
- Operating system: {os}
- Architecture: {arch}
- Current input: {input_buffer}

Selected text:
{selected_text}

Recent terminal output (up to {line_limit} lines):
{recent_output}

Requirements:
- Target language: {language}
- Use that language for user-facing explanation and reasoning when possible.
- Keep commands, paths, file names, and configuration keys unchanged.
- Safety mode: {safety_mode}
- Generate at most {max_commands} commands.
- Prefer read-only diagnostic commands first.
- If information is insufficient, provide verification commands.
- Return a JSON object only. Do not return Markdown.",
            user_input = request.user_input,
            connection_name = ctx.connection_name.as_deref().unwrap_or("-"),
            host = ctx.host.as_deref().unwrap_or("-"),
            port = ctx
                .port
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_string()),
            username = ctx.username.as_deref().unwrap_or("-"),
            cwd = ctx.cwd.as_deref().unwrap_or("-"),
            os = ctx.os.as_deref().unwrap_or("-"),
            arch = ctx.arch.as_deref().unwrap_or(std::env::consts::ARCH),
            input_buffer = ctx.input_buffer,
            selected_text = ctx.selected_text,
            line_limit = settings.context_line_limit,
            recent_output = ctx.recent_output,
            language = request.options.language,
            safety_mode = request.options.safety_mode,
            max_commands = request.options.max_output_commands,
        )
    }
}
