//! The request the interpreter sends: the system prompt, the history as stored,
//! the tools, and the provider's profile — pure in `(provider, meta, history)`.
//!
//! This is the file that the prefix cache is about: the same three inputs must
//! produce the same bytes, which is why one test here freezes the whole request
//! as a literal.

use crate::provider;
use crate::session::SessionMeta;
use crate::types::{Message, Role, ToolCall};

use super::{SYSTEM_PROMPT, build_request};

/// A meta as a session file holds it, with a provider and a stored effort.
fn test_meta() -> SessionMeta {
    SessionMeta {
        provider: Some("deepseek".into()),
        model: "deepseek-v4-flash".into(),
        reasoning_effort: Some("high".into()),
    }
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
    let request = build_request(&provider::DEEPSEEK, &test_meta(), &history);
    assert_eq!(request.messages.len(), 2);
    assert_eq!(request.messages[0].role, Role::System);
    assert_eq!(request.messages[0].text().as_deref(), Some(SYSTEM_PROMPT));
    assert_eq!(request.messages[1], Message::user("q1"));
    assert_eq!(request.tools.as_ref().unwrap()[0].function.name, "Bash");
    assert_eq!(request.tool_choice.as_deref(), Some("auto"));
}

#[test]
fn build_request_defaults_missing_effort_to_max() {
    // Even when a session's meta has no effort, it must be pinned to the
    // provider's default rather than left to each backend's own.
    let mut meta = test_meta();
    meta.reasoning_effort = None;
    let request = build_request(&provider::DEEPSEEK, &meta, &[]);
    assert_eq!(request.reasoning_effort.as_deref(), Some("max"));
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
                function: crate::types::ToolCallFunction {
                    name: "Bash".into(),
                    arguments: r#"{"command":"true"}"#.into(),
                },
            }]),
            tool_call_id: None,
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
    };
    assert_eq!(
        serde_json::to_string(&build_request(&provider::DEEPSEEK, &deepseek, &history)).unwrap(),
        r#"{"model":"deepseek-flash","max_tokens":384000,"messages":[{"role":"system","content":"You are caocli, a coding agent. You and the user share one workspace, and your job is to collaborate with them until their goal is genuinely handled. Keep answers concise. Tool routing: use Read to read a file, Edit to modify an existing file, Write to create or fully rewrite a file, and Bash for everything else (running programs, builds, tests, git, directories, bulk text processing). Prefer absolute paths: each Bash call starts a fresh shell, so cd does not persist."},{"role":"user","content":"freeze"},{"role":"assistant","content":"","reasoning_content":"think","tool_calls":[{"id":"call_f1","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"true\"}"}}]},{"role":"tool","content":"exit_code: 0","tool_call_id":"call_f1"},{"role":"user","content":"again"}],"tools":[{"type":"function","function":{"name":"Bash","description":"Run one bash command on the local machine; returns exit_code, stdout and stderr (returned separately). Every call is a fresh shell: the working directory and environment variables do not persist, so to target a directory write cd /abs/path && ... inside the same command, and prefer absolute paths. Use for: running programs, builds, tests, git, directory operations, bulk text processing. Not for: reading a text file (use Read), modifying an existing file (use Edit), creating or fully rewriting a file (use Write); do not substitute cat/sed -i/tee for them. Each stream is kept to 10240 bytes: a longer one is shown from both of its ends, with what fell between them counted in its place. A command that runs past its timeout is killed, with everything it started: 120s unless timeout says otherwise (at most 1800), and a job that wants longer belongs in the background, where its output can be polled from a file. A screen editor (vim, nano, ...) is refused: nothing a call runs has a terminal, so one would only draw its interface into the result. Edit a file with Edit/Write, or with sed -i, python3 -c, or the editor's own script mode (vim -es).","parameters":{"properties":{"command":{"description":"the bash command to run","type":"string"},"timeout":{"description":"seconds to let the command run before it is killed (default 120, at most 1800)","type":"integer"}},"required":["command"],"type":"object"}}},{"type":"function","function":{"name":"Read","description":"Read a UTF-8 text file, one numbered row per line: the line number, a tab, then the line. The number and the tab are not part of the file content. Confirm the original text with this tool before modifying a file. Use offset (the 1-based line number to start at; default 1) and limit (how many lines to read; default to the end of the file) to page through a long file: it is read a page at a time, so a page of a huge file costs no more than a page of a small one. Output is capped at 10240 bytes, cut on a line boundary. The marker after the last row says which lines were shown, how many lines the file has, and the offset to read on from. A note says so when the page's lines end with CRLF or the file has no newline after its last line; a single line longer than the whole cap is shown cut and named, and the rest of it is read with Bash. A directory, a file that is not regular, a page that is not valid UTF-8, and a file over 256MB are reported as errors.","parameters":{"properties":{"file_path":{"description":"path of the file to read","type":"string"},"limit":{"description":"how many lines to read; default all the way to the end","type":"integer"},"offset":{"description":"the 1-based line number to start reading at; default 1, the first line","type":"integer"}},"required":["file_path"],"type":"object"}}},{"type":"function","function":{"name":"Edit","description":"Make an exact string replacement in an existing file (old_string -> new_string). Cannot create a new file; use Write to create one or to rewrite a whole file. old_string must match the file content character for character, including indentation, tab-versus-space differences and line endings — one character off is reported as not found, so use Read to check the original when the indentation is uncertain. new_string does not have to carry them: the text inserted is written in the line endings of the file it goes into, so a replacement typed with plain newlines lands as the file's own. old_string must occur exactly once in the file (zero or multiple occurrences is an error); one call replaces one occurrence, so make several calls for several edits. old_string must not be empty; an empty new_string deletes the matched text.","parameters":{"properties":{"file_path":{"description":"path of the file to modify","type":"string"},"new_string":{"description":"the replacement text; an empty string deletes the matched text","type":"string"},"old_string":{"description":"the original text to replace; must occur exactly once in the file","type":"string"}},"required":["file_path","old_string","new_string"],"type":"object"}}},{"type":"function","function":{"name":"Write","description":"Create a file, or overwrite a whole file; missing parent directories are created automatically. Content is written in the line endings the file already uses -- a CRLF file stays CRLF and an LF one stays LF, and the call's own newlines are brought into them -- so a whole-file rewrite does not turn every line of a long file into a change. A file that does not exist yet, one whose endings are mixed, and one over 10MB are written exactly as they were sent. An existing file is overwritten completely and unrecoverably, so use Read to check it first; use this only to create a file or rewrite one wholesale, and use Edit for partial changes to an existing file. A content larger than 10MB is rejected.","parameters":{"properties":{"content":{"description":"the full contents to write","type":"string"},"file_path":{"description":"path of the file to write","type":"string"}},"required":["file_path","content"],"type":"object"}}},{"type":"function","function":{"name":"AskUserQuestion","description":"Ask the user a question and wait for the answer. Use it when a choice is the user's to make -- which of several approaches to take, which file is meant, whether to go ahead -- and when guessing would waste the work. Asking stops the turn until they answer, so ask everything in one call rather than in several, and do not ask what you can find out yourself. Each question carries a stable id, the words to ask, and up to four options; an option is a label (what they pick) plus one sentence of description (what they read). Put the option you recommend first and end its label with \"(Recommended)\". A question with no options is answered in the user's own words. The result reports the chosen labels per question id.","parameters":{"properties":{"questions":{"description":"Questions to ask the user before continuing.","items":{"additionalProperties":true,"properties":{"header":{"description":"Optional short heading for the question, such as \"Confirm\" or \"Choose Mode\".","type":"string"},"id":{"description":"Stable id for this question; echoed in the answer.","type":"string"},"multi_select":{"description":"Whether the user may select more than one option. Defaults to false.","type":"boolean"},"options":{"description":"Optional choices to show the user. If you recommend one, put it first and append \"(Recommended)\" to that label.","items":{"additionalProperties":true,"properties":{"description":{"description":"One sentence explaining the tradeoff or impact.","type":"string"},"label":{"description":"Short user-facing option label.","type":"string"}},"required":["label"],"type":"object"},"type":"array"},"question":{"description":"The specific question to ask the user.","type":"string"}},"required":["id","question"],"type":"object"},"type":"array"}},"required":["questions"],"type":"object"}}},{"type":"function","function":{"name":"TodoWrite","description":"Record the plan for the work in hand, and keep it up to date as you go. Use it for work that takes several steps; a question answered in one line does not need a list. The whole list is sent every time and replaces the one before it, so each call is the state of the work rather than a change to it: send every task, not only the one that moved. The list is shown to the user and stays in view while the turn runs, so keep the items short, keep at most one task in_progress, and mark a task completed as soon as it is done rather than in a batch at the end. Send an empty list when nothing is left to track. Writing the list is not doing the work: it is how the user sees what is being done.","parameters":{"properties":{"todos":{"description":"The whole list, in the order the work is done. Send an empty array to clear it.","items":{"additionalProperties":true,"properties":{"content":{"description":"What the task is, in the imperative: \"Add the parse function\".","type":"string"},"status":{"description":"Where the task stands. Defaults to pending.","enum":["pending","in_progress","completed"],"type":"string"}},"required":["content"],"type":"object"},"maxItems":20,"type":"array"}},"required":["todos"],"type":"object"}}}],"tool_choice":"auto","stream":true,"thinking":{"type":"enabled"},"reasoning_effort":"high"}"#
    );
    // The other preset: its own answer ceiling, and the default effort when
    // the meta carries none (a stored value rides along in `deepseek` above).
    let zai = SessionMeta {
        provider: Some("zai-coding-cn".into()),
        model: "glm-5.3".into(),
        reasoning_effort: None,
    };
    assert_eq!(
        serde_json::to_string(&build_request(&provider::ZAI_CODING_CN, &zai, &history)).unwrap(),
        r#"{"model":"glm-5.3","max_tokens":128000,"messages":[{"role":"system","content":"You are caocli, a coding agent. You and the user share one workspace, and your job is to collaborate with them until their goal is genuinely handled. Keep answers concise. Tool routing: use Read to read a file, Edit to modify an existing file, Write to create or fully rewrite a file, and Bash for everything else (running programs, builds, tests, git, directories, bulk text processing). Prefer absolute paths: each Bash call starts a fresh shell, so cd does not persist."},{"role":"user","content":"freeze"},{"role":"assistant","content":"","reasoning_content":"think","tool_calls":[{"id":"call_f1","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"true\"}"}}]},{"role":"tool","content":"exit_code: 0","tool_call_id":"call_f1"},{"role":"user","content":"again"}],"tools":[{"type":"function","function":{"name":"Bash","description":"Run one bash command on the local machine; returns exit_code, stdout and stderr (returned separately). Every call is a fresh shell: the working directory and environment variables do not persist, so to target a directory write cd /abs/path && ... inside the same command, and prefer absolute paths. Use for: running programs, builds, tests, git, directory operations, bulk text processing. Not for: reading a text file (use Read), modifying an existing file (use Edit), creating or fully rewriting a file (use Write); do not substitute cat/sed -i/tee for them. Each stream is kept to 10240 bytes: a longer one is shown from both of its ends, with what fell between them counted in its place. A command that runs past its timeout is killed, with everything it started: 120s unless timeout says otherwise (at most 1800), and a job that wants longer belongs in the background, where its output can be polled from a file. A screen editor (vim, nano, ...) is refused: nothing a call runs has a terminal, so one would only draw its interface into the result. Edit a file with Edit/Write, or with sed -i, python3 -c, or the editor's own script mode (vim -es).","parameters":{"properties":{"command":{"description":"the bash command to run","type":"string"},"timeout":{"description":"seconds to let the command run before it is killed (default 120, at most 1800)","type":"integer"}},"required":["command"],"type":"object"}}},{"type":"function","function":{"name":"Read","description":"Read a UTF-8 text file, one numbered row per line: the line number, a tab, then the line. The number and the tab are not part of the file content. Confirm the original text with this tool before modifying a file. Use offset (the 1-based line number to start at; default 1) and limit (how many lines to read; default to the end of the file) to page through a long file: it is read a page at a time, so a page of a huge file costs no more than a page of a small one. Output is capped at 10240 bytes, cut on a line boundary. The marker after the last row says which lines were shown, how many lines the file has, and the offset to read on from. A note says so when the page's lines end with CRLF or the file has no newline after its last line; a single line longer than the whole cap is shown cut and named, and the rest of it is read with Bash. A directory, a file that is not regular, a page that is not valid UTF-8, and a file over 256MB are reported as errors.","parameters":{"properties":{"file_path":{"description":"path of the file to read","type":"string"},"limit":{"description":"how many lines to read; default all the way to the end","type":"integer"},"offset":{"description":"the 1-based line number to start reading at; default 1, the first line","type":"integer"}},"required":["file_path"],"type":"object"}}},{"type":"function","function":{"name":"Edit","description":"Make an exact string replacement in an existing file (old_string -> new_string). Cannot create a new file; use Write to create one or to rewrite a whole file. old_string must match the file content character for character, including indentation, tab-versus-space differences and line endings — one character off is reported as not found, so use Read to check the original when the indentation is uncertain. new_string does not have to carry them: the text inserted is written in the line endings of the file it goes into, so a replacement typed with plain newlines lands as the file's own. old_string must occur exactly once in the file (zero or multiple occurrences is an error); one call replaces one occurrence, so make several calls for several edits. old_string must not be empty; an empty new_string deletes the matched text.","parameters":{"properties":{"file_path":{"description":"path of the file to modify","type":"string"},"new_string":{"description":"the replacement text; an empty string deletes the matched text","type":"string"},"old_string":{"description":"the original text to replace; must occur exactly once in the file","type":"string"}},"required":["file_path","old_string","new_string"],"type":"object"}}},{"type":"function","function":{"name":"Write","description":"Create a file, or overwrite a whole file; missing parent directories are created automatically. Content is written in the line endings the file already uses -- a CRLF file stays CRLF and an LF one stays LF, and the call's own newlines are brought into them -- so a whole-file rewrite does not turn every line of a long file into a change. A file that does not exist yet, one whose endings are mixed, and one over 10MB are written exactly as they were sent. An existing file is overwritten completely and unrecoverably, so use Read to check it first; use this only to create a file or rewrite one wholesale, and use Edit for partial changes to an existing file. A content larger than 10MB is rejected.","parameters":{"properties":{"content":{"description":"the full contents to write","type":"string"},"file_path":{"description":"path of the file to write","type":"string"}},"required":["file_path","content"],"type":"object"}}},{"type":"function","function":{"name":"AskUserQuestion","description":"Ask the user a question and wait for the answer. Use it when a choice is the user's to make -- which of several approaches to take, which file is meant, whether to go ahead -- and when guessing would waste the work. Asking stops the turn until they answer, so ask everything in one call rather than in several, and do not ask what you can find out yourself. Each question carries a stable id, the words to ask, and up to four options; an option is a label (what they pick) plus one sentence of description (what they read). Put the option you recommend first and end its label with \"(Recommended)\". A question with no options is answered in the user's own words. The result reports the chosen labels per question id.","parameters":{"properties":{"questions":{"description":"Questions to ask the user before continuing.","items":{"additionalProperties":true,"properties":{"header":{"description":"Optional short heading for the question, such as \"Confirm\" or \"Choose Mode\".","type":"string"},"id":{"description":"Stable id for this question; echoed in the answer.","type":"string"},"multi_select":{"description":"Whether the user may select more than one option. Defaults to false.","type":"boolean"},"options":{"description":"Optional choices to show the user. If you recommend one, put it first and append \"(Recommended)\" to that label.","items":{"additionalProperties":true,"properties":{"description":{"description":"One sentence explaining the tradeoff or impact.","type":"string"},"label":{"description":"Short user-facing option label.","type":"string"}},"required":["label"],"type":"object"},"type":"array"},"question":{"description":"The specific question to ask the user.","type":"string"}},"required":["id","question"],"type":"object"},"type":"array"}},"required":["questions"],"type":"object"}}},{"type":"function","function":{"name":"TodoWrite","description":"Record the plan for the work in hand, and keep it up to date as you go. Use it for work that takes several steps; a question answered in one line does not need a list. The whole list is sent every time and replaces the one before it, so each call is the state of the work rather than a change to it: send every task, not only the one that moved. The list is shown to the user and stays in view while the turn runs, so keep the items short, keep at most one task in_progress, and mark a task completed as soon as it is done rather than in a batch at the end. Send an empty list when nothing is left to track. Writing the list is not doing the work: it is how the user sees what is being done.","parameters":{"properties":{"todos":{"description":"The whole list, in the order the work is done. Send an empty array to clear it.","items":{"additionalProperties":true,"properties":{"content":{"description":"What the task is, in the imperative: \"Add the parse function\".","type":"string"},"status":{"description":"Where the task stands. Defaults to pending.","enum":["pending","in_progress","completed"],"type":"string"}},"required":["content"],"type":"object"},"maxItems":20,"type":"array"}},"required":["todos"],"type":"object"}}}],"tool_choice":"auto","stream":true,"thinking":{"type":"enabled"},"reasoning_effort":"max"}"#
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
    };
    let json = serde_json::to_string(&build_request(&p, &meta, &[])).unwrap();
    assert!(!json.contains("\"thinking\""), "{json}");
    assert!(json.contains("\"reasoning_effort\":\"max\""), "{json}");
}
