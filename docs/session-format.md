# caocli Session File Format (v1)

A session file is the **execution trace of one caocli session**: the events of
the run, in the order they happened, in a form that can be read back both by
caocli itself and by a person with `jq`.

## [Unreleased]

### Added — Xiaomi MiMo V2.5

A third wire and a fourth provider. Xiaomi's `https://api.xiaomimimo.com`
serves its `mimo-v2.5-pro` (the flagship reasoning model) and `mimo-v2.5`
(the omni-modal one) over the OpenAI Responses API — a list of `input`
items rather than the chat wire's list of messages, with the system prompt
riding beside the conversation as `instructions`. The wire speaks the same
standards: the standard events, the standard `usage.input_tokens_details.
cached_tokens` for prompt caching, and `response.incomplete_details.reason`
for the ceiling notice. The reasoning items the model writes come back as
typed items with an id of their own, and the chain-of-thought replay that
keeps multi-turn reasoning continuous works on this wire the same way the
signed blocks do on the Anthropic one: the next request sends them whole,
in the order the model produced them.

- `crates/openai/src/responses_stream.rs`: `ResponseUsageSummary` keeps the
  input/output breakdowns and `ResponseSummary` keeps `incomplete_details` —
  the standard fields the lifecycle events carry and the totals-only shape
  was dropping. `ResponseInputItem::FunctionCall` is added so a call the
  model made on an earlier turn replays alongside its `function_call_output`,
  which is what `is_request_valid`'s tool-result window requires.
- `src/types.rs::ReasoningItem`: the wire's own form of the chain of
  thought (id + text), carried on the assistant message alongside the
  Anthropic `thinking` blocks and the OpenAI `reasoning_content` flat
  text. The accumulator groups `reasoning_text.delta` fragments under the
  item id the wire streamed them with, so each item arrives whole.
- `src/provider.rs::MIMO`: the preset, with `efforts = ["none","low","medium","high"]`,
  the thinking tier as the wire's `reasoning.effort`, and a `Wire::Responses`
  variant that names `https://api.xiaomimimo.com/v1/responses` verbatim.
- `src/session.rs::WireKind::OpenAiResponses` is added to the trace
  vocabulary, so a reader rebuilding a `request_prefix` event knows which
  recipe the body was made from.

Two readers, two requirements, one file:

- **the machine** folds the trace back into state (`machine.rs`: *state = a fold
  of the log*). The messages it projects are sent to the backend as they stand,
  byte for byte, because the prefix cache depends on it.
- **a person or a script** asks what happened: which model, which tool, who
  approved it, why the turn ended, what it cost. Fields may grow for this
  reader; what the other reader sees may not.

Every rule below follows from those two sentences.

## Status legend

| Status | Meaning |
|---|---|
| `current` | Written and read by caocli today. Frozen: a reader must keep folding it the same way forever. |
| `planned` | Designed here, not implemented yet. No writer may emit it before the reader tolerates it (see *Writer rules*). |

## The file

- UTF-8, LF line endings, one JSON object per line (JSONL), no BOM, no
  trailing whitespace significance.
- **Append-only, never rewritten.** A trace that is rewritten is not a trace.
  A crash can damage the tail line and nothing else.
- Line 1 is the `header`. Every other line is a `msg`, an `ev` or a `meta`
  line, discriminated by `type`.
- One session, one file, one writer at a time (an exclusive `flock` in
  production; the constraint is what keeps the trace ordered).
- **Location: `~/.caocli/sessions/<escaped working directory>/<id>.jsonl`**,
  mode `0600` (new files).
- Durability: an append is a `write(2)` into the page cache. There is no
  `fsync`. A power cut can lose the tail; the reader's `heal` then makes
  the loss look like an ordinary interruption.

## Envelope

Every v1 line carries an envelope. **No event payload may use these names**:

```
type  sequence  timestamp_ms  turn  kind  source_sequences  ignorable
message  format  format_version  id
```

| Key | Type | Meaning |
|---|---|---|
| `type` | string | Line type: `header`, `msg`, `ev`, `meta` (the reader's dispatch key) |
| `sequence` | int ≥ 1 | Position in this file. Strictly monotonic. The only ordering authority. Every line but the header carries one, `meta` included, and the reader must track all of them: skipping a line type here leaves the next line looking like a gap |
| `timestamp_ms` | int? | Wall clock UTC epoch milliseconds (optional; absent means not recorded) |
| `turn` | int? | One user message and every sub-request that follows until the next |
| `kind` | string? | On `ev` lines: `request_prefix`, `request`, `reply`, `gate`, `call_end`, `stop` |
| `source_sequences` | int[]? | The `sequence` values of lines this event is about |
| `ignorable` | bool? | Whether a reader that does not know this `kind` may skip the line |

## `header` — line 1

```json
{"type":"header","format":"caocli-session","format_version":1,
 "id":"20260914-164241","timestamp_ms":1789375361123,
 "application":{"version":"0.4.1","commit":"9c45254"},
 "provider":"deepseek","model":"deepseek-flash","reasoning_effort":"max",
 "instructions":"…"}
```

| Key | Meaning |
|---|---|
| `format` | `"caocli-session"`. Makes the file self-describing |
| `format_version` | `1`. The dialect marker; absence means `v0` |
| `id` | The trace's identity |
| `application` | `{"version", "commit"}`. Which caocli wrote this trace |
| `working_directory` | Absolute path (reserved) |
| `provider`, `model`, `reasoning_effort`, `instructions` | The meta object |

## `msg` — one message of the conversation

```json
{"type":"msg","sequence":5,"timestamp_ms":1789375380001,"turn":1,
 "message":{"role":"assistant","content":"…"}}
```

The `message` object is **the wire object itself**. Field names match the API.
A log-only key inside it would either be sent to the backend or silently
dropped — both break byte-for-byte replay. Everything the trace wants to say
about a message is said by an `ev` line that points at it with `source_sequences`.

## `meta` — settings at a point in the trace

```json
{"type":"meta","sequence":9,"timestamp_ms":1789375380600,
 "provider":"zai-coding-cn","model":"glm-5.3","reasoning_effort":"high"}
```

The payload is the whole meta object. Last line wins. Absent field means
"not in force".

## `ev` — what the harness did

```json
{"type":"ev","sequence":6,"timestamp_ms":1789375380002,"turn":1,"kind":"gate",
 "source_sequences":[5],"tool_call_id":"call_2","decided_by":"user","verdict":"denied","ignorable":true}
```

| `kind` | Payload | Answers |
|---|---|---|
| `request_prefix` | `wire`, `endpoint`, `system_prompt`, `tools`, `parameters` | What the request carried before the messages |
| `request` | `wire`, `endpoint`, `model`, `body_sha256`, `attempt`, `history_through_sequence`, `duration_ms` | One HTTP attempt. `body_sha256` is canonical-sha256 of the body |
| `reply` | `usage`, `finish_reason`, `truncated`, `duration_ms`, `model` | What the backend answered (normalised usage) |
| `gate` | `tool_call_id`, `decided_by`, `verdict` | Was a call put to the user, what did they say? |
| `call_end` | `tool_call_id`, `tool_name`, `outcome`, `duration_ms?` | How the harness closed a declared call |
| `stop` | `reason`, `detail?` | Why a turn ended. **Exactly one `stop` per `turn`** |

## `reply.usage`

```json
{"prompt_tokens":41233,"completion_tokens":812,"cache_hit_tokens":38912,"cache_miss_tokens":2321}
```

Both backends' shapes (DeepSeek's flat fields, GLM's nested
`prompt_tokens_details.cached_tokens`) fold into the same `cache_hit_tokens` /
`cache_miss_tokens` pair. A reader rebuilding the trace does not have to know
which wire produced the line.

## Rebuilding a request body

The reader's job: rebuild the body the agent sent, hash it, and compare
to `body_sha256`. They are either equal or the reader reports a mismatch —
that is the only test there is for whether the recipe stayed correct across
caocli versions.

The hash is over **canonical JSON** (RFC 8785):

- Object keys are sorted in lexicographic order by UTF-16 code units.
  We approximate this by sorting `String`s by their byte order; ASCII keys
  match byte-for-byte, and every request body in this binary has
  ASCII-only keys.
- Numbers are serialised in the JSON-number grammar.
- Strings are escaped with the JSON string grammar.
- No insignificant whitespace.

## `v0` to `v1`, field by field

| `v0` | `v1` |
|---|---|
| `t` | `type` |
| `created_at` (seconds, header only) | the header's `timestamp_ms` (milliseconds) |
| `seq` | `sequence` |
| `at_ms` | `timestamp_ms` |
| `of` | `source_sequences` |
| `v` | `format_version` |
| `app` | `application` |
| `by` | `decided_by` |
| `call` | `tool_call_id` |
| `name` | `tool_name` |
| `from`, `to` | `previous_state`, `state` |
| `count`, `hash`, `names` | `tool_count`, `definition_hash`, `tool_names` |
| `stop.message` | `stop.detail` |

Nothing in a `v0` file is renamed on disk: those files are read as they are.

## Version rules

- `format_version` is a property of the **file**, not of a line. There is no
  per-line version and no per-section version.
- A reader **must** accept `format_version ≤` its own and **must** refuse a
  greater one with an explicit error. Silently degrading is not allowed: the
  message objects of a newer file may carry fields this reader would drop,
  and a dropped field in a message is a request the backend no longer matches.
- The projection of a given version is frozen. Adding event names may change
  what a *new* file can express; it may never change what an old file folds to.

## Reader rules

| Situation | Behaviour |
|---|---|
| Unknown `type` | Skip the line silently. A newer writer's new line type is not corruption. |
| Unknown `kind` with `"ignorable": true` | Skip the line silently. |
| Unknown `kind` without the marker | **Refuse to reconstruct**, and say which event it was. |
| Line that is not valid JSON (or not valid UTF-8) | Warn and skip. |
| Such a line that is **not** the last non-empty line | Warn differently and loudly: append-only says only the tail can be damaged. |
| `sequence` gap or duplicate | **Refuse to load**: the counter slipped, so every line past it has a `sequence` that no longer says where it belongs and a resume cannot be trusted. |
| No valid header | `v0`: fatal (the meta lives only there). `v1`: warn, take the `id` from the file stem, fold anyway. |

## Writer rules

- Append only. Never rewrite, renumber or compress a line.
- `sequence` starts at 1 on the first line after the header and increments by
  one.
- `message` objects are written exactly as sent and as received. No trimming,
  clipping or normalizing on the way in or out.
- **Only a settled message becomes a line.** An answer that was still streaming
  when the process died is not in the file at all.
- **Migrating a `v0` file means writing a new one.** Read it, write a `v1` file
  with new `sequence` values and no inventing of times that were never
  recorded, and leave the original exactly as it was.

## Locked decisions

- **Naming**: full names unless the short one is the name everyone knows.
- **Two dialects, frozen**: `v0` is what every file written today looks
  like (`t` on every line, no `format_version`). `v1` is what this document
  specifies (`format_version: 1`, the full envelope and event vocabulary).
  A file is one dialect for its whole life. `v0` files are never upgraded
  in place.
- **Numbering starts at `v0`**: there are no real users yet, only the
  bootstrapper, and a file whose only difference is that someone wrote it
  after some other file does not deserve a higher number. `v1` is what
  becomes the current dialect once the envelope and event vocabulary
  described here are written.
- **Layout**: `~/.caocli/sessions/<escaped working directory>/<id>.jsonl`.
- **Turn**: one user message and every sub-request that follows until the
  next user message.
- **Durability**: no `fsync`. The choice is part of the contract.
- **Safety invariants**: only settled messages become lines; `interrupted`
  is reader-derived, never writer-written; effects never replay; an unknown
  event without `ignorable: true` refuses reconstruction; `body_sha256`
  mismatch is a bug, not a warning.
