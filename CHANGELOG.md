# Changelog

All notable changes to caocli are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/) and the project tags
semantic-version bumps per release.

## [Unreleased]

## [0.1.5] — 2026-09-13

### Added — MCP (Model Context Protocol)

caocli now speaks MCP, both transports the specification defines
(revision 2025-06-18):

- **stdio** servers: started as child processes, JSON-RPC framed one
  message per line, the standard input closed on shutdown.
- **streamable HTTP** servers: `POST` with `application/json` or
  `text/event-stream` responses, the `MCP-Protocol-Version` and
  `Mcp-Session-Id` headers, `DELETE` to end the session.

A server's tools are offered to the model beside the built-in ones,
named `mcp__<server>__<tool>`. Two files configure servers:

- `~/.caocli/settings.json` under `"mcpServers"` for the ones you want
  in every workspace;
- the workspace's `.mcp.json` for the ones a project ships (the file
  other clients read; the project wins for a name both files define).

`${VAR}` and `${VAR:-default}` are expanded when the file is read. A
reference with neither resolves to a warning and the text stays as
written, so a value turned into nothing is never what starts a server.

The new `/mcp` REPL command lists the servers, what they call
themselves, the protocol version both ends settled on, and the names
the model sees.

### Changed

- `Agent` now holds an `Arc<Hub>` over the MCP servers this session
  connected to. Tool dispatches go through it. Tool lists are appended
  after the built-in ones so a session's earlier requests stay a prefix
  of its later ones.
- `tools::execute_live` carries the hub, so a call to a server's tool
  is dispatched like any other.

### Implementation notes

- The MCP client declares no capabilities of its own (no roots, sampling
  or elicitation). A server that asks is refused by name rather than
  left waiting.
- `--ask` applies to MCP tools as well: what a server does with a call
  is the server's own, and the spec's own note about annotations is why
  the server's word does not settle it.
- A server that does not come up costs nothing else: the session runs
  with the servers that did, and `/mcp` says which is missing and why.
- OAuth 2.1 (dynamic client registration, PKCE, RFC 8707 resource
  parameters) is not implemented. HTTP servers that need it accept a
  static Bearer token in `headers`; servers that require the dance
  will refuse a session without it.
- JSON-RPC batching, removed from the spec in 2025-06-18, is not sent
  or parsed.
- Server-emitted notifications (`logging`, `progress`, `list_changed`,
  `cancelled`, `message`) are read past silently. Spec marks these
  MAY/SHOULD rather than MUST on the client side; nothing this client
  does depends on any of them.

## [0.1.4] and earlier

Pre-0.1.5 history is not recorded here. See `git log` for the
changes between earlier releases.