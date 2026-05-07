use super::super::parse;
use super::super::{
    ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit, PresetContext,
    TranscriptFormat, TranscriptSource,
};
use crate::authorship::working_log::AgentId;
use crate::commands::checkpoint_agent::bash_tool::ToolClass;
use crate::error::GitAiError;
use crate::transcripts::model_extraction;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Legacy extension path (before_edit / after_edit)
// ---------------------------------------------------------------------------

pub(super) fn parse_legacy_extension_hooks(
    data: &serde_json::Value,
    hook_event_name: &str,
    trace_id: &str,
) -> Result<Vec<ParsedHookEvent>, GitAiError> {
    let cwd = parse::optional_str_multi(data, &["workspace_folder", "workspaceFolder"])
        .ok_or_else(|| {
            GitAiError::PresetError(
                "workspace_folder or workspaceFolder not found in hook_input for GitHub Copilot preset".to_string(),
            )
        })?;

    let dirty_files = super::dirty_files_from_hook_data(data, cwd);

    let session_id = super::extract_session_id(data);

    if hook_event_name == "before_edit" {
        let will_edit_filepaths = data
            .get("will_edit_filepaths")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| parse::resolve_absolute(s, cwd))
                    .collect::<Vec<PathBuf>>()
            })
            .ok_or_else(|| {
                GitAiError::PresetError(
                    "will_edit_filepaths is required for before_edit hook_event_name".to_string(),
                )
            })?;

        if will_edit_filepaths.is_empty() {
            return Err(GitAiError::PresetError(
                "will_edit_filepaths cannot be empty for before_edit hook_event_name".to_string(),
            ));
        }

        let context = PresetContext {
            agent_id: AgentId {
                tool: "github-copilot".to_string(),
                id: session_id.clone(),
                model: "unknown".to_string(),
            },
            session_id,
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(cwd),
            metadata: HashMap::new(),
        };

        return Ok(vec![ParsedHookEvent::PreFileEdit(PreFileEdit {
            context,
            file_paths: will_edit_filepaths,
            dirty_files,
            tool_use_id: None,
        })]);
    }

    // after_edit path
    let chat_session_path =
        parse::optional_str_multi(data, &["chat_session_path", "chatSessionPath"]).ok_or_else(
            || {
                GitAiError::PresetError(
                    "chat_session_path or chatSessionPath not found in hook_input for after_edit"
                        .to_string(),
                )
            },
        )?;

    let edited_filepaths = data
        .get("edited_filepaths")
        .and_then(|val| val.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| parse::resolve_absolute(s, cwd))
                .collect::<Vec<PathBuf>>()
        })
        .unwrap_or_default();

    let mut metadata = HashMap::new();
    metadata.insert(
        "chat_session_path".to_string(),
        chat_session_path.to_string(),
    );

    let context = PresetContext {
        agent_id: AgentId {
            tool: "github-copilot".to_string(),
            id: session_id.clone(),
            model: model_extraction::extract_model(
                Path::new(chat_session_path),
                crate::transcripts::sweep::TranscriptFormat::CopilotSessionJson,
                None,
            )
            .ok()
            .flatten()
            .unwrap_or_else(|| "unknown".to_string()),
        },
        session_id,
        trace_id: trace_id.to_string(),
        cwd: PathBuf::from(cwd),
        metadata,
    };

    let transcript_source = Some(TranscriptSource {
        path: PathBuf::from(chat_session_path),
        format: TranscriptFormat::CopilotSessionJson,
        session_id: context.session_id.clone(),
        external_thread_id: None,
    });

    Ok(vec![ParsedHookEvent::PostFileEdit(PostFileEdit {
        context,
        file_paths: edited_filepaths,
        dirty_files,
        transcript_source,
        tool_use_id: None,
    })])
}

// ---------------------------------------------------------------------------
// VS Code native path (PreToolUse / PostToolUse)
// ---------------------------------------------------------------------------

pub(super) fn parse_vscode_native_hooks(
    data: &serde_json::Value,
    hook_event_name: &str,
    trace_id: &str,
) -> Result<Vec<ParsedHookEvent>, GitAiError> {
    let cwd = parse::optional_str_multi(data, &["cwd", "workspace_folder", "workspaceFolder"])
        .ok_or_else(|| GitAiError::PresetError("cwd not found in hook_input".to_string()))?;

    let dirty_files = super::dirty_files_from_hook_data(data, cwd);

    let session_id = super::extract_session_id(data);

    let tool_name =
        parse::optional_str_multi(data, &["tool_name", "toolName"]).unwrap_or("unknown");

    // Enforce tool filtering to avoid creating checkpoints for read/search tools
    if !is_supported_vscode_edit_tool_name(tool_name) {
        return Err(GitAiError::PresetError(format!(
            "Skipping VS Code hook for unsupported tool_name '{}' (non-edit tool).",
            tool_name
        )));
    }

    let tool_input = data.get("tool_input").or_else(|| data.get("toolInput"));
    let tool_response = data
        .get("tool_response")
        .or_else(|| data.get("toolResponse"));

    // Extract file paths from tool_input and tool_response only (not session-level data)
    let extracted_paths =
        super::extract_filepaths_from_vscode_hook_payload(tool_input, tool_response, cwd);

    let transcript_path = transcript_path_from_hook_data(data).map(|s| s.to_string());

    if let Some(ref path) = transcript_path
        && looks_like_claude_transcript_path(path)
    {
        return Err(GitAiError::PresetError(
            "Skipping VS Code hook because transcript_path looks like a Claude transcript path."
                .to_string(),
        ));
    }

    if !is_likely_copilot_native_hook(transcript_path.as_deref()) {
        return Err(GitAiError::PresetError(format!(
            "Skipping VS Code hook for non-Copilot session (tool_name: {}).",
            tool_name,
        )));
    }

    let tool_class = classify_copilot_tool(tool_name);
    let is_bash = tool_class == ToolClass::Bash;

    let tool_use_id = parse::optional_str_multi(data, &["tool_use_id", "toolUseId"])
        .unwrap_or("unknown")
        .to_string();

    let mut metadata = HashMap::new();
    if let Some(ref path) = transcript_path {
        metadata.insert("transcript_path".to_string(), path.clone());
        metadata.insert("chat_session_path".to_string(), path.clone());
    }

    // Determine transcript format: newer native uses EventStreamJsonl
    let transcript_format = if transcript_path
        .as_deref()
        .map(|p| p.contains("/workspaceStorage/") || p.contains("\\workspaceStorage\\"))
        .unwrap_or(false)
    {
        TranscriptFormat::CopilotEventStreamJsonl
    } else {
        TranscriptFormat::CopilotSessionJson
    };

    let context = PresetContext {
        agent_id: AgentId {
            tool: "github-copilot".to_string(),
            id: session_id.clone(),
            model: transcript_path
                .as_ref()
                .and_then(|tp| {
                    let path = Path::new(tp.as_str());
                    let sweep_format = match transcript_format {
                        TranscriptFormat::CopilotEventStreamJsonl => {
                            crate::transcripts::sweep::TranscriptFormat::CopilotEventStreamJsonl
                        }
                        _ => crate::transcripts::sweep::TranscriptFormat::CopilotSessionJson,
                    };
                    model_extraction::extract_model(path, sweep_format, None)
                        .ok()
                        .flatten()
                        .or_else(|| {
                            model_extraction::extract_model_from_copilot_editing_state(path)
                        })
                })
                .unwrap_or_else(|| "unknown".to_string()),
        },
        session_id,
        trace_id: trace_id.to_string(),
        cwd: PathBuf::from(cwd),
        metadata,
    };

    let transcript_source = transcript_path.map(|tp| TranscriptSource {
        path: PathBuf::from(tp),
        format: transcript_format,
        session_id: context.session_id.clone(),
        external_thread_id: None,
    });

    if hook_event_name == "PreToolUse" {
        if is_bash {
            return Ok(vec![ParsedHookEvent::PreBashCall(PreBashCall {
                context,
                tool_use_id,
            })]);
        }

        if tool_name.eq_ignore_ascii_case("create_file") {
            if extracted_paths.is_empty() {
                return Err(GitAiError::PresetError(
                    "No file path found in create_file PreToolUse tool_input".to_string(),
                ));
            }

            let mut empty_dirty_files: HashMap<PathBuf, String> = HashMap::new();
            for path in &extracted_paths {
                empty_dirty_files.insert(path.clone(), String::new());
            }
            return Ok(vec![ParsedHookEvent::PreFileEdit(PreFileEdit {
                context,
                file_paths: extracted_paths,
                dirty_files: Some(empty_dirty_files),
                tool_use_id: Some(tool_use_id),
            })]);
        }

        if extracted_paths.is_empty() {
            return Err(GitAiError::PresetError(format!(
                "No editable file paths found in VS Code hook input (tool_name: {}). Skipping checkpoint.",
                tool_name
            )));
        }

        return Ok(vec![ParsedHookEvent::PreFileEdit(PreFileEdit {
            context,
            file_paths: extracted_paths,
            dirty_files,
            tool_use_id: Some(tool_use_id),
        })]);
    }

    // PostToolUse
    if is_bash {
        return Ok(vec![ParsedHookEvent::PostBashCall(PostBashCall {
            context,
            tool_use_id,
            transcript_source,
        })]);
    }

    if extracted_paths.is_empty() {
        return Err(GitAiError::PresetError(format!(
            "No editable file paths found in VS Code PostToolUse hook input (tool_name: {}). Skipping checkpoint.",
            tool_name
        )));
    }

    // For PostToolUse edit tools: the file may not yet be saved to disk by VS Code.
    // Compute the expected "after" content from tool_input so the checkpoint records
    // AI attribution even when the filesystem write is delayed.
    let dirty_files = compute_post_tool_dirty_files(tool_name, tool_input, cwd, dirty_files);

    Ok(vec![ParsedHookEvent::PostFileEdit(PostFileEdit {
        context,
        file_paths: extracted_paths,
        dirty_files,
        transcript_source,
        tool_use_id: Some(tool_use_id),
    })])
}

// ---------------------------------------------------------------------------
// IDE-specific helpers
// ---------------------------------------------------------------------------

fn transcript_path_from_hook_data(data: &serde_json::Value) -> Option<&str> {
    parse::optional_str_multi(
        data,
        &[
            "transcript_path",
            "transcriptPath",
            "chat_session_path",
            "chatSessionPath",
        ],
    )
}

fn looks_like_claude_transcript_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    normalized.contains("/.claude/") || normalized.contains("/claude/projects/")
}

fn looks_like_copilot_transcript_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    normalized.contains("/github.copilot-chat/transcripts/")
        || normalized.contains("vscode-chat-session")
        || normalized.contains("copilot_session")
        || (normalized.contains("/workspacestorage/") && normalized.contains("/chatsessions/"))
}

fn is_likely_copilot_native_hook(transcript_path: Option<&str>) -> bool {
    let Some(path) = transcript_path else {
        return false;
    };
    if looks_like_claude_transcript_path(path) {
        return false;
    }
    looks_like_copilot_transcript_path(path)
}

fn is_supported_vscode_edit_tool_name(tool_name: &str) -> bool {
    let lower = tool_name.to_ascii_lowercase();

    // Explicit bash/terminal tools
    let bash_tools = ["run_in_terminal"];
    if bash_tools.iter().any(|name| lower == *name) {
        return true;
    }

    let non_edit_keywords = [
        "find", "search", "read", "grep", "glob", "list", "ls", "fetch", "web", "open", "todo",
    ];
    if non_edit_keywords.iter().any(|kw| lower.contains(kw)) {
        return false;
    }

    let exact_edit_tools = [
        "write",
        "edit",
        "multiedit",
        "applypatch",
        "apply_patch",
        "copilot_insertedit",
        "copilot_replacestring",
        "vscode_editfile_internal",
        "create_file",
        "delete_file",
        "rename_file",
        "move_file",
        "replace_string_in_file",
        "insert_edit_into_file",
    ];
    if exact_edit_tools.iter().any(|name| lower == *name) {
        return true;
    }

    lower.contains("edit") || lower.contains("write") || lower.contains("replace")
}

/// Classify GitHub Copilot tool for bash vs file edit handling.
/// GithubCopilot is not in the `Agent` enum, so we implement classification locally.
fn classify_copilot_tool(tool_name: &str) -> ToolClass {
    let lower = tool_name.to_ascii_lowercase();
    match lower.as_str() {
        "run_in_terminal" => ToolClass::Bash,
        "create_file"
        | "replace_string_in_file"
        | "apply_patch"
        | "delete_file"
        | "rename_file"
        | "move_file" => ToolClass::FileEdit,
        _ if lower.contains("edit") || lower.contains("write") || lower.contains("replace") => {
            ToolClass::FileEdit
        }
        _ => ToolClass::Skip,
    }
}

/// Extract file paths from apply_patch text format. Called from the shared
/// `collect_tool_paths` because apply_patch payloads embed paths in the patch
/// text rather than in JSON keys.
pub(super) fn collect_apply_patch_paths_from_text(raw: &str, out: &mut Vec<String>) {
    for line in raw.lines() {
        let trimmed = line.trim();
        let maybe_path = trimmed
            .strip_prefix("*** Update File: ")
            .or_else(|| trimmed.strip_prefix("*** Add File: "))
            .or_else(|| trimmed.strip_prefix("*** Delete File: "))
            .or_else(|| trimmed.strip_prefix("*** Move to: "));

        if let Some(path) = maybe_path {
            let path = path.trim();
            if !path.is_empty() && !out.iter().any(|existing| existing == path) {
                out.push(path.to_string());
            }
        }
    }
}

/// Compute dirty_files for PostToolUse events by applying the tool's transformation.
/// VS Code may not have saved the file to disk yet when PostToolUse fires, so we compute
/// the expected content from tool_input to ensure the checkpoint has the correct "after" state.
fn compute_post_tool_dirty_files(
    tool_name: &str,
    tool_input: Option<&serde_json::Value>,
    cwd: &str,
    existing_dirty_files: Option<HashMap<PathBuf, String>>,
) -> Option<HashMap<PathBuf, String>> {
    let lower = tool_name.to_ascii_lowercase();

    // tool_input may be a JSON string that needs parsing, or already a JSON object.
    let parsed_input: Option<serde_json::Value>;
    let input_obj = match tool_input {
        Some(serde_json::Value::Object(_)) => tool_input,
        Some(serde_json::Value::String(s)) => {
            // For apply_patch, the string is the patch text itself (not JSON)
            if lower == "apply_patch" {
                tool_input
            } else {
                // Try parsing as JSON object
                parsed_input = serde_json::from_str(s).ok();
                parsed_input.as_ref()
            }
        }
        _ => tool_input,
    };

    match lower.as_str() {
        "apply_patch" => {
            let patch_text = tool_input
                .and_then(|v| v.as_str())
                .or_else(|| {
                    tool_input
                        .and_then(|v| v.get("input"))
                        .and_then(|v| v.as_str())
                })
                .unwrap_or("");
            if !patch_text.is_empty() {
                apply_patch_to_dirty_files(patch_text, cwd).or(existing_dirty_files)
            } else {
                existing_dirty_files
            }
        }
        "replace_string_in_file" => {
            let input = input_obj?;
            let file_path = input
                .get("filePath")
                .or_else(|| input.get("file_path"))
                .or_else(|| input.get("path"))
                .and_then(|v| v.as_str())?;
            let old_string = input
                .get("oldString")
                .or_else(|| input.get("old_string"))
                .and_then(|v| v.as_str())?;
            let new_string = input
                .get("newString")
                .or_else(|| input.get("new_string"))
                .and_then(|v| v.as_str())?;

            let path = super::normalize_hook_path(file_path, cwd)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(file_path));
            let original = std::fs::read_to_string(&path).ok()?;
            let replaced = original.replacen(old_string, new_string, 1);
            if replaced == original {
                return existing_dirty_files;
            }
            let mut result = HashMap::new();
            result.insert(path, replaced);
            Some(result)
        }
        "create_file" => {
            let input = input_obj?;
            let file_path = input
                .get("filePath")
                .or_else(|| input.get("file_path"))
                .or_else(|| input.get("path"))
                .and_then(|v| v.as_str())?;
            let content = input
                .get("content")
                .or_else(|| input.get("file_text"))
                .or_else(|| input.get("fileText"))
                .and_then(|v| v.as_str())
                .unwrap_or("");

            let path = super::normalize_hook_path(file_path, cwd)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(file_path));
            let mut result = HashMap::new();
            result.insert(path, content.to_string());
            Some(result)
        }
        _ => existing_dirty_files,
    }
}

/// Apply a VS Code apply_patch tool_input to produce dirty_files with expected content.
/// Returns None if the patch cannot be applied (malformed patch or file read error).
fn apply_patch_to_dirty_files(patch_text: &str, cwd: &str) -> Option<HashMap<PathBuf, String>> {
    let mut result = HashMap::new();
    let lines: Vec<&str> = patch_text.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let trimmed = lines[i].trim();

        if let Some(path_str) = trimmed.strip_prefix("*** Add File: ") {
            let path_str = path_str.trim();
            let path = super::normalize_hook_path(path_str, cwd)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(path_str));
            i += 1;
            // Collect all added content after the @@ marker
            let mut content = String::new();
            if i < lines.len() && lines[i].trim() == "@@" {
                i += 1;
            }
            while i < lines.len() {
                let line = lines[i];
                let line_trimmed = line.trim();
                if line_trimmed.starts_with("*** ") {
                    break;
                }
                if let Some(added) = line.strip_prefix('+') {
                    content.push_str(added);
                    content.push('\n');
                } else if !line.starts_with('-') {
                    // Context line or bare line in an Add File section
                    let ctx = line.strip_prefix(' ').unwrap_or(line);
                    content.push_str(ctx);
                    content.push('\n');
                }
                i += 1;
            }
            result.insert(path, content);
            continue;
        }

        if let Some(path_str) = trimmed.strip_prefix("*** Update File: ") {
            let path_str = path_str.trim();
            let path = super::normalize_hook_path(path_str, cwd)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(path_str));
            i += 1;

            // Read current file content from disk
            let original = std::fs::read_to_string(&path).unwrap_or_default();
            let original_lines: Vec<&str> = original.lines().collect();

            // Parse and apply all hunks for this file
            let mut output_lines: Vec<String> = Vec::new();
            let mut src_idx: usize = 0;

            while i < lines.len() {
                let line_trimmed = lines[i].trim();
                if line_trimmed.starts_with("*** ") {
                    break;
                }
                if line_trimmed == "@@" {
                    i += 1;
                    // Find the context start to locate position in source
                    let _hunk_start = i;
                    // Collect hunk lines until next @@ or *** or end
                    let mut hunk_lines: Vec<&str> = Vec::new();
                    while i < lines.len() {
                        let hl = lines[i].trim();
                        if hl == "@@" || hl.starts_with("*** ") {
                            break;
                        }
                        hunk_lines.push(lines[i]);
                        i += 1;
                    }

                    // Find the first context or removal line to locate position
                    let first_match_line = hunk_lines.iter().find_map(|l| {
                        if let Some(stripped) = l.strip_prefix(' ').or_else(|| l.strip_prefix('-'))
                        {
                            Some(stripped)
                        } else if !l.starts_with('+') && !l.is_empty() {
                            Some(*l)
                        } else {
                            None
                        }
                    });

                    if let Some(match_content) = first_match_line {
                        // Find this line in source starting from src_idx
                        let match_pos = original_lines[src_idx..]
                            .iter()
                            .position(|l| *l == match_content)
                            .map(|p| p + src_idx);

                        if let Some(pos) = match_pos {
                            // Copy lines before the hunk
                            for line in &original_lines[src_idx..pos] {
                                output_lines.push(line.to_string());
                            }
                            src_idx = pos;
                        }
                    }

                    // Apply the hunk
                    for hl in &hunk_lines {
                        if let Some(rest) = hl.strip_prefix('+') {
                            output_lines.push(rest.to_string());
                        } else if hl.starts_with('-') {
                            // Skip this source line
                            src_idx += 1;
                        } else if let Some(rest) = hl.strip_prefix(' ') {
                            output_lines.push(rest.to_string());
                            src_idx += 1;
                        } else if !hl.is_empty() {
                            // Bare context line (no prefix)
                            output_lines.push(hl.to_string());
                            src_idx += 1;
                        }
                    }
                    continue;
                }
                i += 1;
            }

            // Copy remaining lines from source
            while src_idx < original_lines.len() {
                output_lines.push(original_lines[src_idx].to_string());
                src_idx += 1;
            }

            // Join with newlines, preserving trailing newline if original had one
            let mut content = output_lines.join("\n");
            if original.ends_with('\n') || content.is_empty() {
                content.push('\n');
            }
            result.insert(path, content);
            continue;
        }

        if let Some(path_str) = trimmed.strip_prefix("*** Delete File: ") {
            let path_str = path_str.trim();
            let path = super::normalize_hook_path(path_str, cwd)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(path_str));
            result.insert(path, String::new());
            i += 1;
            continue;
        }

        i += 1;
    }

    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::AgentPreset;
    use super::super::GithubCopilotPreset;
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // Legacy extension path tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_copilot_legacy_before_edit() {
        let input = json!({
            "hook_event_name": "before_edit",
            "workspace_folder": "/home/user/project",
            "will_edit_filepaths": ["/home/user/project/src/main.rs"],
            "chat_session_id": "sess-123",
            "dirty_files": {"/home/user/project/src/main.rs": "old content"}
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "github-copilot");
                assert_eq!(e.context.session_id, "sess-123");
                assert_eq!(e.context.cwd, PathBuf::from("/home/user/project"));
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/main.rs")]
                );
                assert!(e.dirty_files.is_some());
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_copilot_dirty_files_camel_case() {
        let input = json!({
            "hook_event_name": "before_edit",
            "workspace_folder": "/home/user/project",
            "will_edit_filepaths": ["/home/user/project/src/main.rs"],
            "chat_session_id": "sess-123",
            "dirtyFiles": {"/home/user/project/src/main.rs": "content"}
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert!(e.dirty_files.is_some());
                let df = e.dirty_files.as_ref().unwrap();
                assert!(df.contains_key(&PathBuf::from("/home/user/project/src/main.rs")));
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_copilot_legacy_after_edit() {
        let input = json!({
            "hook_event_name": "after_edit",
            "workspace_folder": "/home/user/project",
            "chat_session_path": "/home/user/.vscode/sessions/sess-123.json",
            "session_id": "sess-123",
            "edited_filepaths": ["src/main.rs"]
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "github-copilot");
                assert_eq!(e.context.session_id, "sess-123");
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/main.rs")]
                );
                assert!(matches!(
                    e.transcript_source,
                    Some(TranscriptSource {
                        format: TranscriptFormat::CopilotSessionJson,
                        ..
                    })
                ));
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_copilot_legacy_before_edit_empty_filepaths() {
        let input = json!({
            "hook_event_name": "before_edit",
            "workspace_folder": "/home/user/project",
            "will_edit_filepaths": [],
            "chat_session_id": "sess-123"
        })
        .to_string();
        let result = GithubCopilotPreset.parse(&input, "t_test123456789a");
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // VS Code native path tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_copilot_native_pre_file_edit() {
        let input = json!({
            "hook_event_name": "PreToolUse",
            "cwd": "/home/user/project",
            "tool_name": "replace_string_in_file",
            "session_id": "sess-456",
            "tool_use_id": "tu-1",
            "tool_input": {"file_path": "/home/user/project/src/main.rs"},
            "transcript_path": "/home/user/.vscode/data/github.copilot-chat/transcripts/sess-456.json"
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "github-copilot");
                assert_eq!(e.context.session_id, "sess-456");
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/main.rs")]
                );
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_copilot_native_post_file_edit() {
        let input = json!({
            "hook_event_name": "PostToolUse",
            "cwd": "/home/user/project",
            "tool_name": "create_file",
            "session_id": "sess-456",
            "tool_use_id": "tu-2",
            "tool_input": {"file_path": "/home/user/project/src/new.rs"},
            "transcript_path": "/home/user/.vscode/data/github.copilot-chat/transcripts/sess-456.json"
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "github-copilot");
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/new.rs")]
                );
                assert!(matches!(
                    e.transcript_source,
                    Some(TranscriptSource {
                        format: TranscriptFormat::CopilotSessionJson,
                        ..
                    })
                ));
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_copilot_native_pre_bash_call() {
        let input = json!({
            "hook_event_name": "PreToolUse",
            "cwd": "/home/user/project",
            "tool_name": "run_in_terminal",
            "session_id": "sess-456",
            "tool_use_id": "tu-3",
            "transcript_path": "/home/user/.vscode/data/github.copilot-chat/transcripts/sess-456.json"
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "github-copilot");
                assert_eq!(e.tool_use_id, "tu-3");
            }
            _ => panic!("Expected PreBashCall"),
        }
    }

    #[test]
    fn test_copilot_native_post_bash_call() {
        let input = json!({
            "hook_event_name": "PostToolUse",
            "cwd": "/home/user/project",
            "tool_name": "run_in_terminal",
            "session_id": "sess-456",
            "tool_use_id": "tu-3",
            "transcript_path": "/home/user/.vscode/data/github.copilot-chat/transcripts/sess-456.json"
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "github-copilot");
                assert_eq!(e.tool_use_id, "tu-3");
            }
            _ => panic!("Expected PostBashCall"),
        }
    }

    #[test]
    fn test_copilot_native_create_file_pre_empty_dirty() {
        let input = json!({
            "hook_event_name": "PreToolUse",
            "cwd": "/home/user/project",
            "tool_name": "create_file",
            "session_id": "sess-456",
            "tool_input": {"file_path": "/home/user/project/src/new_file.rs"},
            "transcript_path": "/home/user/.vscode/data/github.copilot-chat/transcripts/sess-456.json"
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/new_file.rs")]
                );
                assert_eq!(
                    e.dirty_files
                        .as_ref()
                        .unwrap()
                        .get(&PathBuf::from("/home/user/project/src/new_file.rs")),
                    Some(&String::new())
                );
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_copilot_skips_non_edit_tools() {
        let input = json!({
            "hook_event_name": "PreToolUse",
            "cwd": "/home/user/project",
            "tool_name": "search_files",
            "session_id": "sess-456",
            "transcript_path": "/home/user/.vscode/data/github.copilot-chat/transcripts/sess-456.json"
        })
        .to_string();
        let result = GithubCopilotPreset.parse(&input, "t_test123456789a");
        assert!(result.is_err());
    }

    #[test]
    fn test_copilot_skips_claude_transcript() {
        let input = json!({
            "hook_event_name": "PreToolUse",
            "cwd": "/home/user/project",
            "tool_name": "create_file",
            "session_id": "sess-456",
            "tool_input": {"file_path": "src/main.rs"},
            "transcript_path": "/home/user/.claude/projects/test.json"
        })
        .to_string();
        let result = GithubCopilotPreset.parse(&input, "t_test123456789a");
        assert!(result.is_err());
    }

    #[test]
    fn test_copilot_session_id_fallback() {
        let input = json!({
            "hook_event_name": "before_edit",
            "workspace_folder": "/home/user/project",
            "will_edit_filepaths": ["/home/user/project/src/main.rs"],
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.session_id, "unknown");
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    // -----------------------------------------------------------------------
    // Helper function tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_classify_copilot_tool_bash() {
        assert_eq!(classify_copilot_tool("run_in_terminal"), ToolClass::Bash);
    }

    #[test]
    fn test_classify_copilot_tool_file_edit() {
        assert_eq!(classify_copilot_tool("create_file"), ToolClass::FileEdit);
        assert_eq!(
            classify_copilot_tool("replace_string_in_file"),
            ToolClass::FileEdit
        );
        assert_eq!(classify_copilot_tool("apply_patch"), ToolClass::FileEdit);
        assert_eq!(classify_copilot_tool("delete_file"), ToolClass::FileEdit);
    }

    #[test]
    fn test_classify_copilot_tool_heuristic() {
        assert_eq!(
            classify_copilot_tool("custom_edit_tool"),
            ToolClass::FileEdit
        );
        assert_eq!(classify_copilot_tool("write_changes"), ToolClass::FileEdit);
    }

    #[test]
    fn test_classify_copilot_tool_skip() {
        assert_eq!(classify_copilot_tool("search_files"), ToolClass::Skip);
        assert_eq!(classify_copilot_tool("unknown_tool"), ToolClass::Skip);
    }

    #[test]
    fn test_collect_apply_patch_paths() {
        let text = "*** Update File: /home/user/src/main.rs\n--- some diff ---\n*** Add File: /home/user/src/new.rs\n";
        let mut paths = Vec::new();
        collect_apply_patch_paths_from_text(text, &mut paths);
        assert_eq!(
            paths,
            vec!["/home/user/src/main.rs", "/home/user/src/new.rs"]
        );
    }

    #[test]
    fn test_looks_like_copilot_transcript_path() {
        assert!(looks_like_copilot_transcript_path(
            "/home/user/.vscode/data/github.copilot-chat/transcripts/test.json"
        ));
        assert!(looks_like_copilot_transcript_path(
            "/path/to/vscode-chat-session-123.json"
        ));
        assert!(!looks_like_copilot_transcript_path(
            "/home/user/.claude/projects/test.json"
        ));
    }

    #[test]
    fn test_is_supported_vscode_edit_tool_name() {
        assert!(is_supported_vscode_edit_tool_name("create_file"));
        assert!(is_supported_vscode_edit_tool_name("run_in_terminal"));
        assert!(is_supported_vscode_edit_tool_name("replace_string_in_file"));
        assert!(is_supported_vscode_edit_tool_name("custom_edit_tool"));
        assert!(!is_supported_vscode_edit_tool_name("search_files"));
        assert!(!is_supported_vscode_edit_tool_name("read_file"));
    }

    #[test]
    fn test_copilot_camel_case_keys() {
        let input = json!({
            "hookEventName": "before_edit",
            "workspaceFolder": "/home/user/project",
            "will_edit_filepaths": ["/home/user/project/src/main.rs"],
            "chatSessionId": "sess-789"
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.session_id, "sess-789");
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_copilot_default_after_edit_when_no_hook_event_name() {
        // When hook_event_name is missing, defaults to "after_edit"
        let input = json!({
            "workspace_folder": "/home/user/project",
            "chat_session_path": "/home/user/.vscode/sessions/sess-123.json",
            "session_id": "sess-123",
            "edited_filepaths": ["src/main.rs"]
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ParsedHookEvent::PostFileEdit(_)));
    }

    #[test]
    fn test_copilot_native_workspace_storage_format() {
        let input = json!({
            "hook_event_name": "PostToolUse",
            "cwd": "/home/user/project",
            "tool_name": "create_file",
            "session_id": "sess-456",
            "tool_input": {"file_path": "/home/user/project/src/new.rs"},
            "transcript_path": "/home/user/.vscode/data/workspaceStorage/abc/chatSessions/sess-456.json"
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert!(matches!(
                    e.transcript_source,
                    Some(TranscriptSource {
                        format: TranscriptFormat::CopilotEventStreamJsonl,
                        ..
                    })
                ));
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }
}
