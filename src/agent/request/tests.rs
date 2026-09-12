//! The request the interpreter sends: the system prompt, the history as stored,
//! the tools, and the provider's profile — pure in
//! `(provider, meta, history, tools)`.
//!
//! This is the file that the prefix cache is about: the same four inputs must
//! produce the same bytes, which is why one test here freezes the whole request
//! as a literal — for each wire.

use crate::provider;
use crate::session::SessionMeta;
use crate::types::{
    Content, FunctionDef, Message, Role, ThinkingBlock, ToolCall, ToolCallFunction, ToolDef,
    WireRequest,
};
use anthropic::{
    Block, BlockKind, ImageSource, MessageContent, SystemPrompt, ThinkingConfig, ToolUnion,
};

use super::{SYSTEM_PROMPT, build_request};

/// A meta as a session file holds it, with a provider and a stored effort.
fn test_meta() -> SessionMeta {
    SessionMeta {
        provider: Some("deepseek".into()),
        model: "deepseek-v4-flash".into(),
        reasoning_effort: Some("high".into()),
        instructions: None,
    }
}

/// Unwrap the OpenAI shape, owned: a borrowed unwrap would fight the
/// temporary the builder returns.
fn openai_of(request: &WireRequest) -> openai::ChatCompletionRequest {
    match request {
        WireRequest::OpenAi(r) => (**r).clone(),
        other => panic!("expected an OpenAI-shaped request, got {other:?}"),
    }
}

fn anthropic_of(request: &WireRequest) -> anthropic::MessagesRequest {
    match request {
        WireRequest::Anthropic(r) => (**r).clone(),
        other => panic!("expected an Anthropic-shaped request, got {other:?}"),
    }
}

/// The text a message carries, lifted out of the SDK's `MessageContent`
/// (string or parts) — the same thing [`Message::text`] does for our local
/// type, repeated here so an assertion reads as a string the way it used to.
fn sdk_text(content: &openai::MessageContent) -> String {
    match content {
        openai::MessageContent::Text(s) => s.clone(),
        openai::MessageContent::Parts(parts) => parts
            .iter()
            .filter_map(|p| match p {
                openai::ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// The role on a SDK message, as the wire writes it. The enum is already
/// `#[serde(rename_all = "lowercase")]`, so this is a small match on the
/// variant — the role lives on the enum wrapper, not on the inner struct.
fn sdk_role_wire(m: &openai::ChatCompletionMessageParam) -> &'static str {
    match m {
        openai::ChatCompletionMessageParam::System(_) => "system",
        openai::ChatCompletionMessageParam::Developer(_) => "developer",
        openai::ChatCompletionMessageParam::User(_) => "user",
        openai::ChatCompletionMessageParam::Assistant(_) => "assistant",
        openai::ChatCompletionMessageParam::Tool(_) => "tool",
        openai::ChatCompletionMessageParam::Function(_) => "function",
    }
}

/// The text of the SDK message, regardless of which variant it is in. The
/// system prompt goes through the same helper as the history, so the
/// assertion reads the way it did when the local type carried it.
fn sdk_message_text(m: &openai::ChatCompletionMessageParam) -> Option<String> {
    match m {
        openai::ChatCompletionMessageParam::System(m) => Some(sdk_text(&m.content)),
        openai::ChatCompletionMessageParam::Developer(m) => Some(sdk_text(&m.content)),
        openai::ChatCompletionMessageParam::User(m) => Some(sdk_text(&m.content)),
        openai::ChatCompletionMessageParam::Assistant(m) => m.content.as_ref().map(sdk_text),
        openai::ChatCompletionMessageParam::Tool(m) => Some(sdk_text(&m.content)),
        openai::ChatCompletionMessageParam::Function(m) => Some(sdk_text(&m.content)),
    }
}

#[test]
fn the_wire_follows_the_provider() {
    assert_eq!(
        build_request(&provider::DEEPSEEK, &test_meta(), &[], &[]),
        WireRequest::OpenAi(Box::new(openai_of(&build_request(
            &provider::DEEPSEEK,
            &test_meta(),
            &[],
            &[]
        ))))
    );
    assert!(matches!(
        build_request(&provider::MINIMAX, &test_meta(), &[], &[]),
        WireRequest::Anthropic(_)
    ));
}

/// The tools of an MCP server go after the built-in ones, on both wires: the
/// tool list is part of the request prefix, and a prefix that has already been
/// sent has to stay a prefix of what is sent next.
#[test]
fn the_servers_tools_are_offered_after_the_built_ins() {
    let server = vec![ToolDef {
        r#type: "function".into(),
        function: FunctionDef {
            name: "mcp__alpha__echo".into(),
            description: Some("the echo tool".into()),
            parameters: Some(serde_json::json!({"type": "object"})),
        },
    }];
    let openai = openai_of(&build_request(
        &provider::DEEPSEEK,
        &test_meta(),
        &[],
        &server,
    ));
    let names: Vec<&str> = openai
        .tools
        .as_ref()
        .unwrap()
        .iter()
        .map(|tool| match tool {
            openai::Tool::Function(f) => f.function.name.as_str(),
        })
        .collect();
    let mut expected: Vec<String> = crate::tools::definitions()
        .into_iter()
        .map(|def| def.function.name)
        .collect();
    expected.push("mcp__alpha__echo".into());
    assert_eq!(
        names,
        expected.iter().map(String::as_str).collect::<Vec<_>>()
    );

    let anthropic = anthropic_of(&build_request(
        &provider::MINIMAX,
        &test_meta(),
        &[],
        &server,
    ));
    let tools = anthropic.tools.as_ref().unwrap();
    assert_eq!(tools.len(), expected.len());
    let ToolUnion::Client(last) = tools.last().unwrap() else {
        panic!("the last tool is one we run: {tools:?}");
    };
    assert_eq!(last.name, "mcp__alpha__echo");
    assert_eq!(last.description.as_deref(), Some("the echo tool"));
    assert_eq!(last.input_schema, serde_json::json!({"type": "object"}));
}

#[test]
fn system_prompt_is_stable_constant() {
    // Guard against someone injecting dynamic content into the system prompt
    // and breaking KVCache in the future
    assert!(!SYSTEM_PROMPT.contains("now"));
    assert!(!SYSTEM_PROMPT.contains("cwd"));
}

#[test]
fn build_request_prepends_system_and_keeps_history_order() {
    let history = vec![Message::user("q1")];
    let request = openai_of(&build_request(
        &provider::DEEPSEEK,
        &test_meta(),
        &history,
        &[],
    ));
    assert_eq!(request.messages.len(), 2);
    assert_eq!(sdk_role_wire(&request.messages[0]), "system");
    assert_eq!(
        sdk_message_text(&request.messages[0]).as_deref(),
        Some(SYSTEM_PROMPT)
    );
    assert_eq!(sdk_role_wire(&request.messages[1]), "user");
    assert_eq!(
        sdk_message_text(&request.messages[1]).as_deref(),
        Some("q1")
    );
    // The first tool is Bash — the order is fixed for KVCache.
    let openai::Tool::Function(f) = &request.tools.as_ref().unwrap()[0];
    assert_eq!(f.function.name, "Bash");
    assert!(matches!(
        request.tool_choice,
        Some(openai::ToolChoice::Mode(openai::ToolChoiceMode::Auto))
    ));
}

#[test]
fn build_request_defaults_missing_effort_to_max() {
    // Even when a session's meta has no effort, it must be pinned to the
    // provider's default rather than left to each backend's own.
    let mut meta = test_meta();
    meta.reasoning_effort = None;
    let request = openai_of(&build_request(&provider::DEEPSEEK, &meta, &[], &[]));
    // The reasoning effort rides through extra_body — a vendor field the SDK
    // does not name on its typed slot.
    assert_eq!(
        request.extra_body.get("reasoning_effort"),
        Some(&serde_json::json!("max"))
    );
}

/// A history with a closed tool-call window: system, a call, its result, and
/// a next user line — every message shape the request carries.
fn freeze_history() -> Vec<Message> {
    vec![
        Message::user("freeze"),
        Message {
            role: Role::Assistant,
            content: Some("".into()),
            reasoning_content: Some("think".into()),
            tool_calls: Some(vec![ToolCall {
                id: "call_f1".into(),
                r#type: "function".into(),
                function: ToolCallFunction {
                    name: "Bash".into(),
                    arguments: r#"{"command":"true"}"#.into(),
                },
            }]),
            tool_call_id: None,
            thinking: None,
        },
        Message::tool("call_f1", "exit_code: 0"),
        Message::user("again"),
    ]
}

/// The request prefix, frozen. The backend's KVCache matches on bytes: any
/// change to the system prompt, the tool definitions, the field names or a
/// provider profile changes every request this program sends and voids the
/// cache of every session that ran before it. Some of those changes are
/// right and some are accidents — this test turns each one into a decision,
/// by failing until the literal below is updated with it.
#[test]
fn the_request_prefix_is_frozen() {
    let history = freeze_history();
    let deepseek = SessionMeta {
        provider: Some("deepseek".into()),
        model: "deepseek-flash".into(),
        reasoning_effort: Some("high".into()),
        instructions: None,
    };
    assert_eq!(
        serde_json::to_string(&openai_of(&build_request(
            &provider::DEEPSEEK,
            &deepseek,
            &history,
            &[]
        )))
        .unwrap(),
        r#"{"model":"deepseek-flash","messages":[{"role":"system","content":"You are caocli, a coding agent. You and the user share one workspace, and your job is to collaborate with them until their goal is genuinely handled. Keep answers concise. Tool routing: use Read to read a file, Edit to modify an existing file, Write to create or fully rewrite a file, and Bash for everything else (running programs, builds, tests, git, directories, bulk text processing). Prefer absolute paths: each Bash call starts a fresh shell, so cd does not persist."},{"role":"user","content":"freeze"},{"role":"assistant","content":"","tool_calls":[{"type":"function","id":"call_f1","function":{"name":"Bash","arguments":"{\"command\":\"true\"}"}}],"reasoning_content":"think"},{"role":"tool","content":"exit_code: 0","tool_call_id":"call_f1"},{"role":"user","content":"again"}],"max_tokens":384000,"tools":[{"type":"function","function":{"name":"Bash","description":"Run one bash command on the local machine; returns exit_code, stdout and stderr (returned separately). Every call is a fresh shell: the working directory and environment variables do not persist, so to target a directory write cd /abs/path && ... inside the same command, and prefer absolute paths. Use for: running programs, builds, tests, git, directory operations, bulk text processing. Not for: reading a text file (use Read), modifying an existing file (use Edit), creating or fully rewriting a file (use Write); do not substitute cat/sed -i/tee for them. Each stream is kept to 10240 bytes: a longer one is shown from both of its ends, with what fell between them counted in its place. What the command prints is streamed to the user while they wait; what you read is the result. A command that runs past its timeout is killed, with everything it started: 120s unless timeout says otherwise (at most 1800). A job that should outlive the call -- a build, a server, a long test run -- is started with background:true instead: the call returns at once and the result names the pid and the file the output goes to. A screen editor (vim, nano, ...) is refused: nothing a call runs has a terminal, so one would only draw its interface into the result. Edit a file with Edit/Write, or with sed -i, python3 -c, or the editor's own script mode (vim -es).","parameters":{"properties":{"background":{"description":"start the command and return at once, its output going to the file the result names (default false)","type":"boolean"},"command":{"description":"the bash command to run","type":"string"},"timeout":{"description":"seconds to let the command run before it is killed (default 120, at most 1800); a background run returns at once and takes none","type":"integer"}},"required":["command"],"type":"object"}}},{"type":"function","function":{"name":"Read","description":"Read a UTF-8 text file, one numbered row per line: the line number, a tab, then the line. The number and the tab are not part of the file content. Confirm the original text with this tool before modifying a file. Use offset (the 1-based line number to start at; default 1) and limit (how many lines to read; default to the end of the file) to page through a long file: it is read a page at a time, so a page of a huge file costs no more than a page of a small one. Output is capped at 10240 bytes, cut on a line boundary. The marker after the last row says which lines were shown, how many lines the file has, and the offset to read on from. A note says so when the page's lines end with CRLF or the file has no newline after its last line; a single line longer than the whole cap is shown cut and named, and the rest of it is read with Bash. A directory, a file that is not regular, a page that is not valid UTF-8, and a file over 256MB are reported as errors.","parameters":{"properties":{"file_path":{"description":"path of the file to read","type":"string"},"limit":{"description":"how many lines to read; default all the way to the end","type":"integer"},"offset":{"description":"the 1-based line number to start reading at; default 1, the first line","type":"integer"}},"required":["file_path"],"type":"object"}}},{"type":"function","function":{"name":"Edit","description":"Make an exact string replacement in an existing file (old_string -> new_string). Cannot create a new file; use Write to create one or to rewrite a whole file. old_string must match the file content character for character, including indentation, tab-versus-space differences and line endings — one character off is reported as not found, so use Read to check the original when the indentation is uncertain. new_string does not have to carry them: the text inserted is written in the line endings of the file it goes into, so a replacement typed with plain newlines lands as the file's own. old_string must occur exactly once in the file (zero or multiple occurrences is an error); one call replaces one occurrence, so make several calls for several edits. old_string must not be empty; an empty new_string deletes the matched text.","parameters":{"properties":{"file_path":{"description":"path of the file to modify","type":"string"},"new_string":{"description":"the replacement text; an empty string deletes the matched text","type":"string"},"old_string":{"description":"the original text to replace; must occur exactly once in the file","type":"string"}},"required":["file_path","old_string","new_string"],"type":"object"}}},{"type":"function","function":{"name":"Write","description":"Create a file, or overwrite a whole file; missing parent directories are created automatically. Content is written in the line endings the file already uses -- a CRLF file stays CRLF and an LF one stays LF, and the call's own newlines are brought into them -- so a whole-file rewrite does not turn every line of a long file into a change. A file that does not exist yet, one whose endings are mixed, and one over 10MB are written exactly as they were sent. An existing file is overwritten completely and unrecoverably, so use Read to check it first; use this only to create a file or rewrite one wholesale, and use Edit for partial changes to an existing file. A content larger than 10MB is rejected.","parameters":{"properties":{"content":{"description":"the full contents to write","type":"string"},"file_path":{"description":"path of the file to write","type":"string"}},"required":["file_path","content"],"type":"object"}}},{"type":"function","function":{"name":"AskUserQuestion","description":"Ask the user a question and wait for the answer. Use it when a choice is the user's to make -- which of several approaches to take, which file is meant, whether to go ahead -- and when guessing would waste the work. Asking stops the turn until they answer, so ask everything in one call rather than in several, and do not ask what you can find out yourself. Each question carries a stable id, the words to ask, and up to four options; an option is a label (what they pick) plus one sentence of description (what they read). Put the option you recommend first and end its label with \"(Recommended)\". A question with no options is answered in the user's own words. The result reports the chosen labels per question id.","parameters":{"properties":{"questions":{"description":"Questions to ask the user before continuing.","items":{"additionalProperties":true,"properties":{"header":{"description":"Optional short heading for the question, such as \"Confirm\" or \"Choose Mode\".","type":"string"},"id":{"description":"Stable id for this question; echoed in the answer.","type":"string"},"multi_select":{"description":"Whether the user may select more than one option. Defaults to false.","type":"boolean"},"options":{"description":"Optional choices to show the user. If you recommend one, put it first and append \"(Recommended)\" to that label.","items":{"additionalProperties":true,"properties":{"description":{"description":"One sentence explaining the tradeoff or impact.","type":"string"},"label":{"description":"Short user-facing option label.","type":"string"}},"required":["label"],"type":"object"},"type":"array"},"question":{"description":"The specific question to ask the user.","type":"string"}},"required":["id","question"],"type":"object"},"type":"array"}},"required":["questions"],"type":"object"}}},{"type":"function","function":{"name":"TodoWrite","description":"Record the plan for the work in hand, and keep it up to date as you go. Use it for work that takes several steps; a question answered in one line does not need a list. The whole list is sent every time and replaces the one before it, so each call is the state of the work rather than a change to it: send every task, not only the one that moved. The list is shown to the user and stays in view while the turn runs, so keep the items short, keep at most one task in_progress, and mark a task completed as soon as it is done rather than in a batch at the end. Send an empty list when nothing is left to track. Writing the list is not doing the work: it is how the user sees what is being done.","parameters":{"properties":{"todos":{"description":"The whole list, in the order the work is done. Send an empty array to clear it.","items":{"additionalProperties":true,"properties":{"content":{"description":"What the task is, in the imperative: \"Add the parse function\".","type":"string"},"status":{"description":"Where the task stands. Defaults to pending.","enum":["pending","in_progress","completed"],"type":"string"}},"required":["content"],"type":"object"},"maxItems":20,"type":"array"}},"required":["todos"],"type":"object"}}},{"type":"function","function":{"name":"Glob","description":"Find the files whose name matches a pattern, under a directory and below it: where a file is, without walking the tree by hand. The pattern is a glob, matched against each path below the directory searched. * matches any run of characters within one name and never a /, ? matches exactly one character, ** as a whole name matches any number of directories including none, [abc] matches one character out of a set ([!abc], or [^abc], the ones outside it; a-c a range), and a backslash makes the next character literal. A pattern with no / in it is asked at any depth, as if it began with **/: *.rs is where the .rs files are. A directory whose name starts with a dot, and node_modules and target, are entered only when the pattern names that directory exactly -- dot and all, no wildcards: they are whole cache, vendor and build trees, and a pattern like **/*.rs is not asking about them. The answer is the absolute path of every file that matched, most recently modified first, ties broken by path. Files only: a directory is not listed, and nothing about a file's contents is read -- searching inside files is a Bash call (grep, rg). At most 100 paths and 10240 bytes are listed, and the walk stops after examining 100000 entries; when a cap cut the answer short, the last line says so. Finding nothing is not a failure: the answer is a sentence saying so, and the pattern or the path is what to change. A fault in the call -- a pattern that is absolute, ends at a directory or goes up one, or a path that is not there or is not a directory -- is answered with what to write instead.","parameters":{"properties":{"path":{"description":"the directory searched, and the one the pattern's paths are relative to; default the working directory","type":"string"},"pattern":{"description":"the glob matched against each path below path; * and ? stop at a /, ** stands for any number of directories, and a pattern with no / is asked at any depth","type":"string"}},"required":["pattern"],"type":"object"}}}],"tool_choice":"auto","stream":true,"stream_options":{"include_usage":true},"reasoning_effort":"high","thinking":{"type":"enabled"}}"#
    );
    // The other preset: its own answer ceiling, and the default effort when
    // the meta carries none (a stored value rides along in `deepseek` above).
    let zai = SessionMeta {
        provider: Some("zai-coding-cn".into()),
        model: "glm-5.3".into(),
        reasoning_effort: None,
        instructions: None,
    };
    assert_eq!(
        serde_json::to_string(&openai_of(&build_request(
            &provider::ZAI_CODING_CN,
            &zai,
            &history,
            &[]
        )))
        .unwrap(),
        r#"{"model":"glm-5.3","messages":[{"role":"system","content":"You are caocli, a coding agent. You and the user share one workspace, and your job is to collaborate with them until their goal is genuinely handled. Keep answers concise. Tool routing: use Read to read a file, Edit to modify an existing file, Write to create or fully rewrite a file, and Bash for everything else (running programs, builds, tests, git, directories, bulk text processing). Prefer absolute paths: each Bash call starts a fresh shell, so cd does not persist."},{"role":"user","content":"freeze"},{"role":"assistant","content":"","tool_calls":[{"type":"function","id":"call_f1","function":{"name":"Bash","arguments":"{\"command\":\"true\"}"}}],"reasoning_content":"think"},{"role":"tool","content":"exit_code: 0","tool_call_id":"call_f1"},{"role":"user","content":"again"}],"max_tokens":128000,"tools":[{"type":"function","function":{"name":"Bash","description":"Run one bash command on the local machine; returns exit_code, stdout and stderr (returned separately). Every call is a fresh shell: the working directory and environment variables do not persist, so to target a directory write cd /abs/path && ... inside the same command, and prefer absolute paths. Use for: running programs, builds, tests, git, directory operations, bulk text processing. Not for: reading a text file (use Read), modifying an existing file (use Edit), creating or fully rewriting a file (use Write); do not substitute cat/sed -i/tee for them. Each stream is kept to 10240 bytes: a longer one is shown from both of its ends, with what fell between them counted in its place. What the command prints is streamed to the user while they wait; what you read is the result. A command that runs past its timeout is killed, with everything it started: 120s unless timeout says otherwise (at most 1800). A job that should outlive the call -- a build, a server, a long test run -- is started with background:true instead: the call returns at once and the result names the pid and the file the output goes to. A screen editor (vim, nano, ...) is refused: nothing a call runs has a terminal, so one would only draw its interface into the result. Edit a file with Edit/Write, or with sed -i, python3 -c, or the editor's own script mode (vim -es).","parameters":{"properties":{"background":{"description":"start the command and return at once, its output going to the file the result names (default false)","type":"boolean"},"command":{"description":"the bash command to run","type":"string"},"timeout":{"description":"seconds to let the command run before it is killed (default 120, at most 1800); a background run returns at once and takes none","type":"integer"}},"required":["command"],"type":"object"}}},{"type":"function","function":{"name":"Read","description":"Read a UTF-8 text file, one numbered row per line: the line number, a tab, then the line. The number and the tab are not part of the file content. Confirm the original text with this tool before modifying a file. Use offset (the 1-based line number to start at; default 1) and limit (how many lines to read; default to the end of the file) to page through a long file: it is read a page at a time, so a page of a huge file costs no more than a page of a small one. Output is capped at 10240 bytes, cut on a line boundary. The marker after the last row says which lines were shown, how many lines the file has, and the offset to read on from. A note says so when the page's lines end with CRLF or the file has no newline after its last line; a single line longer than the whole cap is shown cut and named, and the rest of it is read with Bash. A directory, a file that is not regular, a page that is not valid UTF-8, and a file over 256MB are reported as errors.","parameters":{"properties":{"file_path":{"description":"path of the file to read","type":"string"},"limit":{"description":"how many lines to read; default all the way to the end","type":"integer"},"offset":{"description":"the 1-based line number to start reading at; default 1, the first line","type":"integer"}},"required":["file_path"],"type":"object"}}},{"type":"function","function":{"name":"Edit","description":"Make an exact string replacement in an existing file (old_string -> new_string). Cannot create a new file; use Write to create one or to rewrite a whole file. old_string must match the file content character for character, including indentation, tab-versus-space differences and line endings — one character off is reported as not found, so use Read to check the original when the indentation is uncertain. new_string does not have to carry them: the text inserted is written in the line endings of the file it goes into, so a replacement typed with plain newlines lands as the file's own. old_string must occur exactly once in the file (zero or multiple occurrences is an error); one call replaces one occurrence, so make several calls for several edits. old_string must not be empty; an empty new_string deletes the matched text.","parameters":{"properties":{"file_path":{"description":"path of the file to modify","type":"string"},"new_string":{"description":"the replacement text; an empty string deletes the matched text","type":"string"},"old_string":{"description":"the original text to replace; must occur exactly once in the file","type":"string"}},"required":["file_path","old_string","new_string"],"type":"object"}}},{"type":"function","function":{"name":"Write","description":"Create a file, or overwrite a whole file; missing parent directories are created automatically. Content is written in the line endings the file already uses -- a CRLF file stays CRLF and an LF one stays LF, and the call's own newlines are brought into them -- so a whole-file rewrite does not turn every line of a long file into a change. A file that does not exist yet, one whose endings are mixed, and one over 10MB are written exactly as they were sent. An existing file is overwritten completely and unrecoverably, so use Read to check it first; use this only to create a file or rewrite one wholesale, and use Edit for partial changes to an existing file. A content larger than 10MB is rejected.","parameters":{"properties":{"content":{"description":"the full contents to write","type":"string"},"file_path":{"description":"path of the file to write","type":"string"}},"required":["file_path","content"],"type":"object"}}},{"type":"function","function":{"name":"AskUserQuestion","description":"Ask the user a question and wait for the answer. Use it when a choice is the user's to make -- which of several approaches to take, which file is meant, whether to go ahead -- and when guessing would waste the work. Asking stops the turn until they answer, so ask everything in one call rather than in several, and do not ask what you can find out yourself. Each question carries a stable id, the words to ask, and up to four options; an option is a label (what they pick) plus one sentence of description (what they read). Put the option you recommend first and end its label with \"(Recommended)\". A question with no options is answered in the user's own words. The result reports the chosen labels per question id.","parameters":{"properties":{"questions":{"description":"Questions to ask the user before continuing.","items":{"additionalProperties":true,"properties":{"header":{"description":"Optional short heading for the question, such as \"Confirm\" or \"Choose Mode\".","type":"string"},"id":{"description":"Stable id for this question; echoed in the answer.","type":"string"},"multi_select":{"description":"Whether the user may select more than one option. Defaults to false.","type":"boolean"},"options":{"description":"Optional choices to show the user. If you recommend one, put it first and append \"(Recommended)\" to that label.","items":{"additionalProperties":true,"properties":{"description":{"description":"One sentence explaining the tradeoff or impact.","type":"string"},"label":{"description":"Short user-facing option label.","type":"string"}},"required":["label"],"type":"object"},"type":"array"},"question":{"description":"The specific question to ask the user.","type":"string"}},"required":["id","question"],"type":"object"},"type":"array"}},"required":["questions"],"type":"object"}}},{"type":"function","function":{"name":"TodoWrite","description":"Record the plan for the work in hand, and keep it up to date as you go. Use it for work that takes several steps; a question answered in one line does not need a list. The whole list is sent every time and replaces the one before it, so each call is the state of the work rather than a change to it: send every task, not only the one that moved. The list is shown to the user and stays in view while the turn runs, so keep the items short, keep at most one task in_progress, and mark a task completed as soon as it is done rather than in a batch at the end. Send an empty list when nothing is left to track. Writing the list is not doing the work: it is how the user sees what is being done.","parameters":{"properties":{"todos":{"description":"The whole list, in the order the work is done. Send an empty array to clear it.","items":{"additionalProperties":true,"properties":{"content":{"description":"What the task is, in the imperative: \"Add the parse function\".","type":"string"},"status":{"description":"Where the task stands. Defaults to pending.","enum":["pending","in_progress","completed"],"type":"string"}},"required":["content"],"type":"object"},"maxItems":20,"type":"array"}},"required":["todos"],"type":"object"}}},{"type":"function","function":{"name":"Glob","description":"Find the files whose name matches a pattern, under a directory and below it: where a file is, without walking the tree by hand. The pattern is a glob, matched against each path below the directory searched. * matches any run of characters within one name and never a /, ? matches exactly one character, ** as a whole name matches any number of directories including none, [abc] matches one character out of a set ([!abc], or [^abc], the ones outside it; a-c a range), and a backslash makes the next character literal. A pattern with no / in it is asked at any depth, as if it began with **/: *.rs is where the .rs files are. A directory whose name starts with a dot, and node_modules and target, are entered only when the pattern names that directory exactly -- dot and all, no wildcards: they are whole cache, vendor and build trees, and a pattern like **/*.rs is not asking about them. The answer is the absolute path of every file that matched, most recently modified first, ties broken by path. Files only: a directory is not listed, and nothing about a file's contents is read -- searching inside files is a Bash call (grep, rg). At most 100 paths and 10240 bytes are listed, and the walk stops after examining 100000 entries; when a cap cut the answer short, the last line says so. Finding nothing is not a failure: the answer is a sentence saying so, and the pattern or the path is what to change. A fault in the call -- a pattern that is absolute, ends at a directory or goes up one, or a path that is not there or is not a directory -- is answered with what to write instead.","parameters":{"properties":{"path":{"description":"the directory searched, and the one the pattern's paths are relative to; default the working directory","type":"string"},"pattern":{"description":"the glob matched against each path below path; * and ? stop at a /, ** stands for any number of directories, and a pattern with no / is asked at any depth","type":"string"}},"required":["pattern"],"type":"object"}}}],"tool_choice":"auto","stream":true,"stream_options":{"include_usage":true},"reasoning_effort":"max","thinking":{"type":"enabled"}}"#
    );
}

/// The profile as wire: a provider that does not take the thinking switch
/// sends no `thinking` field at all, and its own default effort applies when
/// the meta stores none.
#[test]
fn a_provider_that_omits_thinking_sends_no_field() {
    let mut p = provider::ZAI_CODING_CN;
    p.send_thinking = false;
    let meta = SessionMeta {
        provider: Some("zai-coding-cn".into()),
        model: "glm-5.3".into(),
        reasoning_effort: None,
        instructions: None,
    };
    let json = serde_json::to_string(&openai_of(&build_request(&p, &meta, &[], &[]))).unwrap();
    assert!(!json.contains("\"thinking\""), "{json}");
    assert!(json.contains("\"reasoning_effort\":\"max\""), "{json}");
}

/// The session's project instructions go between the system prompt and the
/// history: ahead of everything the session went on to say, and byte-for-byte
/// what the meta stored.
#[test]
fn the_instructions_sit_between_the_system_prompt_and_the_history() {
    let meta = SessionMeta {
        instructions: Some("the framed instructions".into()),
        ..test_meta()
    };
    let history = vec![Message::user("q1")];
    let request = openai_of(&build_request(&provider::DEEPSEEK, &meta, &history, &[]));
    assert_eq!(request.messages.len(), 3);
    assert_eq!(sdk_role_wire(&request.messages[0]), "system");
    assert_eq!(
        sdk_message_text(&request.messages[0]).as_deref(),
        Some(SYSTEM_PROMPT)
    );
    assert_eq!(sdk_role_wire(&request.messages[1]), "user");
    assert_eq!(
        sdk_message_text(&request.messages[1]).as_deref(),
        Some("the framed instructions")
    );
    assert_eq!(sdk_role_wire(&request.messages[2]), "user");
    assert_eq!(
        sdk_message_text(&request.messages[2]).as_deref(),
        Some("q1")
    );
}

/// A session without instructions — one written before they were recorded, or
/// started in a workspace without AGENTS.md — sends exactly what it always
/// sent. This is the shape the frozen request above pins.
#[test]
fn a_session_without_instructions_sends_nothing_for_them() {
    let history = vec![Message::user("q1")];
    let request = openai_of(&build_request(
        &provider::DEEPSEEK,
        &test_meta(),
        &history,
        &[],
    ));
    assert_eq!(request.messages.len(), 2);
    assert_eq!(sdk_role_wire(&request.messages[1]), "user");
    assert_eq!(
        sdk_message_text(&request.messages[1]).as_deref(),
        Some("q1")
    );
}

/// An empty stored value sends nothing: a message of air is a prefix of air.
#[test]
fn empty_instructions_are_not_sent() {
    let meta = SessionMeta {
        instructions: Some(String::new()),
        ..test_meta()
    };
    let request = openai_of(&build_request(&provider::DEEPSEEK, &meta, &[], &[]));
    assert_eq!(request.messages.len(), 1);
}

// ---------------------------------------------------------------------------
// The Anthropic wire (MiniMax).
// ---------------------------------------------------------------------------

/// A preset of an endpoint that serves the spec as published: the same backend
/// as MiniMax, with every standard optional field offered.
fn standard_preset() -> provider::Provider {
    provider::Provider {
        anthropic: provider::AnthropicOptions {
            effort: true,
            display: true,
            cache_control: true,
        },
        ..provider::MINIMAX
    }
}

/// The same backend with none of them offered: what an endpoint nobody has
/// asked looks like, and what MiniMax itself looked like before it was asked.
fn unasked_preset() -> provider::Provider {
    provider::Provider {
        anthropic: provider::AnthropicOptions::NONE,
        ..provider::MINIMAX
    }
}

/// The standard optional fields are the preset's to offer. An endpoint nobody
/// has asked sends the required shape and nothing else — a field a gateway does
/// not know is a 400 on every request — and each one that is offered arrives in
/// the form the spec spells.
#[test]
fn the_standard_optional_fields_are_sent_only_where_the_preset_offers_them() {
    let history = vec![Message::user("hi")];
    let standard = serde_json::to_value(anthropic_of(&build_request(
        &standard_preset(),
        &minimax_meta(Some("high")),
        &history,
        &[],
    )))
    .unwrap();
    assert_eq!(
        standard["output_config"],
        serde_json::json!({"effort": "high"})
    );
    assert_eq!(
        standard["cache_control"],
        serde_json::json!({"type": "ephemeral"})
    );
    assert_eq!(
        standard["thinking"],
        serde_json::json!({"type": "adaptive", "display": "summarized"})
    );

    // The same request against an endpoint nobody has asked: the required shape
    // and nothing else.
    let quiet = serde_json::to_value(anthropic_of(&build_request(
        &unasked_preset(),
        &minimax_meta(Some("high")),
        &history,
        &[],
    )))
    .unwrap();
    for absent in ["output_config", "cache_control"] {
        assert!(quiet.get(absent).is_none(), "{absent} was sent: {quiet}");
    }
    assert_eq!(quiet["thinking"], serde_json::json!({"type": "adaptive"}));

    // What MiniMax was asked and answered: it takes the cache breakpoint and the
    // `display` field, and both were measured in effect — a repeated prefix read
    // back from the cache, the reasoning text streamed. Its tiers are a thinking
    // switch, so no effort is sent, and `display` rides on the switch.
    let minimax = serde_json::to_value(anthropic_of(&build_request(
        &provider::MINIMAX,
        &minimax_meta(Some("high")),
        &history,
        &[],
    )))
    .unwrap();
    assert!(minimax.get("output_config").is_none(), "{minimax}");
    assert_eq!(
        minimax["cache_control"],
        serde_json::json!({"type": "ephemeral"})
    );
    assert_eq!(
        minimax["thinking"],
        serde_json::json!({"type": "adaptive", "display": "summarized"})
    );
}

/// An effort tier the spec has no word for is left unsent rather than guessed
/// at, and thinking stays on: the endpoint's own tiers are its own, and a
/// value it does not serve is a 400 the session cannot see the reason for.
#[test]
fn an_effort_with_no_standard_word_is_left_unsent() {
    let request = serde_json::to_value(anthropic_of(&build_request(
        &standard_preset(),
        &minimax_meta(Some("off")),
        &[],
        &[],
    )))
    .unwrap();
    assert!(request.get("output_config").is_none(), "{request}");
    assert_eq!(
        request["thinking"],
        serde_json::json!({"type": "adaptive", "display": "summarized"})
    );
}

/// A tool result that reports a failure says so where the wire has a field for
/// it. A failure sent as an ordinary result reads to the model as an answer.
#[test]
fn a_failed_tool_result_is_marked_as_one() {
    let history = vec![
        Message::user("go"),
        freeze_history()[1].clone(),
        Message::tool("call_f1", "error: /nope: no such file"),
        Message::user("try again"),
        freeze_history()[1].clone(),
        Message::tool("call_f1", "exit_code: 0"),
    ];
    let request = serde_json::to_value(anthropic_of(&build_request(
        &provider::MINIMAX,
        &minimax_meta(Some("on")),
        &history,
        &[],
    )))
    .unwrap();
    assert_eq!(request["messages"][2]["content"][0]["is_error"], true);
    // A result that is not a failure carries no field at all: the wire's
    // default is what it means, and a field to read for nothing is a field the
    // endpoint has to take.
    assert!(
        request["messages"][5]["content"][0]
            .get("is_error")
            .is_none(),
        "{request}"
    );
}

/// The project instructions go out on both wires. They were missing from this
/// one: `meta.instructions` is frozen at session creation and sent from the
/// meta, and only the OpenAI shape was reading it — so a MiniMax session was
/// started with no AGENTS.md in front of it at all, while the instructions of a
/// directory discovered mid-session (which ride in the history) arrived as
/// usual. The symptom read as a model that ignored what it had been told.
#[test]
fn anthropic_system_prompt_carries_the_project_instructions() {
    let meta = SessionMeta {
        instructions: Some("the house rules".into()),
        ..minimax_meta(Some("on"))
    };
    let req = anthropic_of(&build_request(&provider::MINIMAX, &meta, &[], &[]));
    assert_eq!(
        req.system,
        Some(SystemPrompt::Text(format!(
            "{SYSTEM_PROMPT}\n\nthe house rules"
        )))
    );
    // And nothing is sent when there are no instructions to send.
    let empty = SessionMeta {
        instructions: Some(String::new()),
        ..minimax_meta(Some("on"))
    };
    let req = anthropic_of(&build_request(&provider::MINIMAX, &empty, &[], &[]));
    assert_eq!(req.system, Some(SystemPrompt::Text(SYSTEM_PROMPT.into())));
}

fn minimax_meta(effort: Option<&str>) -> SessionMeta {
    SessionMeta {
        provider: Some("minimax".into()),
        model: "MiniMax-M3".into(),
        reasoning_effort: effort.map(str::to_owned),
        instructions: None,
    }
}

#[test]
fn anthropic_request_maps_the_history_onto_blocks() {
    let history = freeze_history();
    let req = anthropic_of(&build_request(
        &provider::MINIMAX,
        &minimax_meta(Some("on")),
        &history,
        &[],
    ));
    // The system prompt is a top-level field, not a message.
    assert_eq!(req.system, Some(SystemPrompt::Text(SYSTEM_PROMPT.into())));
    // user, assistant-with-call, user-with-result, user again.
    assert_eq!(req.messages.len(), 4);
    assert_eq!(
        req.messages[0].content,
        MessageContent::Text("freeze".into())
    );
    // The call is a tool_use block; the result a tool_result block in the
    // user turn that follows it.
    assert_eq!(
        req.messages[1].content,
        MessageContent::Blocks(vec![Block::tool_use(
            "call_f1",
            "Bash",
            serde_json::json!({"command": "true"}),
        )])
    );
    assert_eq!(
        req.messages[2].content,
        MessageContent::Blocks(vec![Block::tool_result("call_f1", "exit_code: 0")])
    );
    assert_eq!(
        req.messages[3].content,
        MessageContent::Text("again".into())
    );
    assert_eq!(req.model, "MiniMax-M3");
    assert_eq!(req.max_tokens, 131_072);
    assert_eq!(req.stream, Some(true));
    assert_eq!(req.tool_choice, Some(anthropic::ToolChoice::auto()));
    // Thinking on, with the reasoning text asked for: MiniMax takes `display`.
    assert_eq!(req.thinking, Some(ThinkingConfig::adaptive().summarized()));
    // The tools arrive in the Anthropic shape, in the same fixed order.
    let tools = req.tools.as_ref().unwrap();
    assert_eq!(tools.len(), 7);
    let ToolUnion::Client(bash) = &tools[0] else {
        panic!("every tool this program sends is its own: {:?}", tools[0]);
    };
    assert_eq!(bash.name, "Bash");
    assert_eq!(bash.input_schema["type"], "object");
}

#[test]
fn anthropic_effort_slot_is_the_thinking_switch() {
    let history = vec![Message::user("hi")];
    for (effort, expected) in [
        (Some("on"), ThinkingConfig::adaptive().summarized()),
        (Some("off"), ThinkingConfig::disabled()),
        // A session that stored nothing sits at the preset's default, and
        // the default keeps thinking on. `display` rides on every tier that
        // thinks: `disabled` has no reasoning text to ask for.
        (None, ThinkingConfig::adaptive().summarized()),
    ] {
        let req = anthropic_of(&build_request(
            &provider::MINIMAX,
            &minimax_meta(effort),
            &history,
            &[],
        ));
        assert_eq!(req.thinking, Some(expected), "effort {effort:?}");
    }
}

#[test]
fn anthropic_request_replays_thinking_blocks_verbatim() {
    let history = vec![
        Message::user("run it"),
        Message {
            role: Role::Assistant,
            content: Some("running".into()),
            reasoning_content: Some("I should run it".into()),
            tool_calls: Some(vec![ToolCall {
                id: "call_x".into(),
                r#type: "function".into(),
                function: ToolCallFunction {
                    name: "Bash".into(),
                    arguments: r#"{"command":"ls"}"#.into(),
                },
            }]),
            tool_call_id: None,
            thinking: Some(vec![ThinkingBlock {
                thinking: "I should run it".into(),
                signature: "sig-cafe".into(),
            }]),
        },
        Message::tool("call_x", "out"),
    ];
    let req = anthropic_of(&build_request(
        &provider::MINIMAX,
        &minimax_meta(Some("on")),
        &history,
        &[],
    ));
    let assistant = &req.messages[1];
    assert_eq!(
        assistant.content,
        MessageContent::Blocks(vec![
            Block::thinking("I should run it", "sig-cafe"),
            Block::text("running"),
            Block::tool_use("call_x", "Bash", serde_json::json!({"command": "ls"})),
        ])
    );
}

#[test]
fn anthropic_request_does_not_replay_openai_style_reasoning() {
    // A session that started on the OpenAI wire carries its reasoning as the
    // flat string, which has no signature to replay; it is left off rather
    // than sent back unsigned.
    let history = vec![Message {
        role: Role::Assistant,
        content: Some("done".into()),
        reasoning_content: Some("reasoned about it".into()),
        tool_calls: None,
        tool_call_id: None,
        thinking: None,
    }];
    let req = anthropic_of(&build_request(
        &provider::MINIMAX,
        &minimax_meta(Some("on")),
        &history,
        &[],
    ));
    assert_eq!(
        req.messages[0].content,
        MessageContent::Blocks(vec![Block::text("done")])
    );
}

#[test]
fn anthropic_request_folds_consecutive_results_into_one_user_turn() {
    let call = |id: &str| ToolCall {
        id: id.into(),
        r#type: "function".into(),
        function: ToolCallFunction {
            name: "Read".into(),
            arguments: r#"{"file_path":"a"}"#.into(),
        },
    };
    let history = vec![
        Message::user("look"),
        Message {
            role: Role::Assistant,
            content: Some(Content::Text(String::new())),
            reasoning_content: None,
            tool_calls: Some(vec![call("c1"), call("c2")]),
            tool_call_id: None,
            thinking: None,
        },
        Message::tool("c1", "first"),
        Message::tool("c2", "second"),
    ];
    let req = anthropic_of(&build_request(
        &provider::MINIMAX,
        &minimax_meta(Some("on")),
        &history,
        &[],
    ));
    assert_eq!(req.messages.len(), 3, "{req:#?}");
    // Two calls in the assistant turn, both results in one user turn after.
    match &req.messages[1].content {
        MessageContent::Blocks(blocks) => assert_eq!(blocks.len(), 2),
        other => panic!("expected blocks, got {other:?}"),
    }
    match &req.messages[2].content {
        MessageContent::Blocks(blocks) => {
            assert_eq!(blocks.len(), 2);
            assert!(
                blocks
                    .iter()
                    .all(|b| matches!(b.kind, BlockKind::ToolResult { .. }),)
            );
        }
        other => panic!("expected blocks, got {other:?}"),
    }
}

#[test]
fn anthropic_request_splits_a_data_url_into_a_base64_image_source() {
    let history = vec![Message {
        role: Role::User,
        content: Some(Content::Parts(
            [
                crate::types::ContentPart::text("what is this?"),
                crate::types::ContentPart::image("data:image/png;base64,Zm9vYmFy"),
            ]
            .to_vec(),
        )),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
        thinking: None,
    }];
    let req = anthropic_of(&build_request(
        &provider::MINIMAX,
        &minimax_meta(Some("on")),
        &history,
        &[],
    ));
    assert_eq!(
        req.messages[0].content,
        MessageContent::Blocks(vec![
            Block::text("what is this?"),
            Block::image(ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "Zm9vYmFy".into(),
            }),
        ])
    );
}

#[test]
fn anthropic_request_maps_an_unparseable_call_to_an_empty_input() {
    // The arguments came from the model itself; if they were valid JSON when
    // emitted they parse here. A call that was not is sent with an empty
    // input rather than failing the whole request: the result the model
    // reads back is where it corrects itself from.
    let history = vec![
        Message::user("go"),
        Message {
            role: Role::Assistant,
            content: None,
            reasoning_content: None,
            tool_calls: Some(vec![ToolCall {
                id: "c".into(),
                r#type: "function".into(),
                function: ToolCallFunction {
                    name: "Bash".into(),
                    arguments: "not json".into(),
                },
            }]),
            tool_call_id: None,
            thinking: None,
        },
        Message::tool("c", "result"),
    ];
    let req = anthropic_of(&build_request(
        &provider::MINIMAX,
        &minimax_meta(Some("on")),
        &history,
        &[],
    ));
    match &req.messages[1].content {
        MessageContent::Blocks(blocks) => match &blocks[0].kind {
            BlockKind::ToolUse { input, .. } => {
                assert_eq!(*input, serde_json::json!({}));
            }
            other => panic!("expected a tool_use block, got {other:?}"),
        },
        other => panic!("expected blocks, got {other:?}"),
    }
}

/// The Anthropic prefix, frozen, on the same history as the OpenAI one: the
/// same cache discipline, the other wire. The tool table is asserted
/// structurally (the OpenAI freeze above pins its wording byte for byte, and
/// the mapping is a pure rename); everything the mapping itself decides —
/// field names, block order, the thinking switch — is pinned in the literal.
#[test]
fn the_anthropic_request_prefix_is_frozen() {
    let history = freeze_history();
    let req = anthropic_of(&build_request(
        &provider::MINIMAX,
        &minimax_meta(Some("high")),
        &history,
        &[],
    ));
    let mut tools = req.tools.clone().unwrap();
    for tool in tools.iter_mut() {
        let ToolUnion::Client(tool) = tool else {
            panic!("every tool this program sends is its own");
        };
        tool.description = Some("…".into());
    }
    let stripped = anthropic::MessagesRequest {
        tools: Some(tools),
        ..req.clone()
    };
    assert_eq!(
        serde_json::to_string(&stripped).unwrap(),
        r#"{"model":"MiniMax-M3","max_tokens":131072,"messages":[{"role":"user","content":"freeze"},{"role":"assistant","content":[{"type":"tool_use","id":"call_f1","name":"Bash","input":{"command":"true"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_f1","content":"exit_code: 0"}]},{"role":"user","content":"again"}],"system":"You are caocli, a coding agent. You and the user share one workspace, and your job is to collaborate with them until their goal is genuinely handled. Keep answers concise. Tool routing: use Read to read a file, Edit to modify an existing file, Write to create or fully rewrite a file, and Bash for everything else (running programs, builds, tests, git, directories, bulk text processing). Prefer absolute paths: each Bash call starts a fresh shell, so cd does not persist.","tools":[{"name":"Bash","description":"…","input_schema":{"properties":{"background":{"description":"start the command and return at once, its output going to the file the result names (default false)","type":"boolean"},"command":{"description":"the bash command to run","type":"string"},"timeout":{"description":"seconds to let the command run before it is killed (default 120, at most 1800); a background run returns at once and takes none","type":"integer"}},"required":["command"],"type":"object"}},{"name":"Read","description":"…","input_schema":{"properties":{"file_path":{"description":"path of the file to read","type":"string"},"limit":{"description":"how many lines to read; default all the way to the end","type":"integer"},"offset":{"description":"the 1-based line number to start reading at; default 1, the first line","type":"integer"}},"required":["file_path"],"type":"object"}},{"name":"Edit","description":"…","input_schema":{"properties":{"file_path":{"description":"path of the file to modify","type":"string"},"new_string":{"description":"the replacement text; an empty string deletes the matched text","type":"string"},"old_string":{"description":"the original text to replace; must occur exactly once in the file","type":"string"}},"required":["file_path","old_string","new_string"],"type":"object"}},{"name":"Write","description":"…","input_schema":{"properties":{"content":{"description":"the full contents to write","type":"string"},"file_path":{"description":"path of the file to write","type":"string"}},"required":["file_path","content"],"type":"object"}},{"name":"AskUserQuestion","description":"…","input_schema":{"properties":{"questions":{"description":"Questions to ask the user before continuing.","items":{"additionalProperties":true,"properties":{"header":{"description":"Optional short heading for the question, such as \"Confirm\" or \"Choose Mode\".","type":"string"},"id":{"description":"Stable id for this question; echoed in the answer.","type":"string"},"multi_select":{"description":"Whether the user may select more than one option. Defaults to false.","type":"boolean"},"options":{"description":"Optional choices to show the user. If you recommend one, put it first and append \"(Recommended)\" to that label.","items":{"additionalProperties":true,"properties":{"description":{"description":"One sentence explaining the tradeoff or impact.","type":"string"},"label":{"description":"Short user-facing option label.","type":"string"}},"required":["label"],"type":"object"},"type":"array"},"question":{"description":"The specific question to ask the user.","type":"string"}},"required":["id","question"],"type":"object"},"type":"array"}},"required":["questions"],"type":"object"}},{"name":"TodoWrite","description":"…","input_schema":{"properties":{"todos":{"description":"The whole list, in the order the work is done. Send an empty array to clear it.","items":{"additionalProperties":true,"properties":{"content":{"description":"What the task is, in the imperative: \"Add the parse function\".","type":"string"},"status":{"description":"Where the task stands. Defaults to pending.","enum":["pending","in_progress","completed"],"type":"string"}},"required":["content"],"type":"object"},"maxItems":20,"type":"array"}},"required":["todos"],"type":"object"}},{"name":"Glob","description":"…","input_schema":{"properties":{"path":{"description":"the directory searched, and the one the pattern's paths are relative to; default the working directory","type":"string"},"pattern":{"description":"the glob matched against each path below path; * and ? stop at a /, ** stands for any number of directories, and a pattern with no / is asked at any depth","type":"string"}},"required":["pattern"],"type":"object"}}],"tool_choice":{"type":"auto"},"thinking":{"type":"adaptive","display":"summarized"},"cache_control":{"type":"ephemeral"},"stream":true}"#
    );
    // The stored effort rides along but means the thinking switch here; the
    // frozen literal above pins `adaptive` for a non-off tier.
    let off = anthropic_of(&build_request(
        &provider::MINIMAX,
        &minimax_meta(Some("off")),
        &history,
        &[],
    ));
    assert_eq!(off.thinking, Some(ThinkingConfig::disabled()));
}

#[test]
fn a_session_that_switched_wires_strips_the_other_wires_thinking() {
    // A session that started on MiniMax and continues on DeepSeek must not
    // send the Anthropic thinking blocks to a backend that rejects fields it
    // does not know; the OpenAI request is byte-for-byte what it would have
    // been without them.
    let mut carried = Message {
        role: Role::Assistant,
        content: Some("done".into()),
        reasoning_content: Some("reasoned".into()),
        tool_calls: None,
        tool_call_id: None,
        thinking: Some(vec![ThinkingBlock {
            thinking: "reasoned".into(),
            signature: "sig".into(),
        }]),
    };
    let history = vec![Message::user("hi"), carried.clone()];
    let req = openai_of(&build_request(
        &provider::DEEPSEEK,
        &test_meta(),
        &history,
        &[],
    ));
    // The Anthropic-only `thinking` block has no slot on the SDK request —
    // it is stripped on the send path.
    let assistant_msg = match &req.messages[2] {
        openai::ChatCompletionMessageParam::Assistant(m) => m,
        other => panic!("expected assistant, got {other:?}"),
    };
    assert!(!assistant_msg.extra_body.contains_key("thinking"));
    // `reasoning_content` is the GLM/DeepSeek-side field: it rides through
    // the assistant message's extra_body so a session that switched from
    // OpenAI replays it verbatim.
    assert_eq!(
        assistant_msg.extra_body.get("reasoning_content"),
        Some(&serde_json::json!("reasoned"))
    );
    // The log's own message is untouched: stripping is on the send path.
    assert!(carried.thinking.is_some());
    carried.thinking = None;
}
