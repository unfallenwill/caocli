#!/usr/bin/env python3
"""Ask an Anthropic-compatible endpoint which standard optional fields it takes.

The protocol has three optional fields this program knows how to send and an
endpoint is free not to serve: `output_config.effort`, `thinking.display`, and
the top-level `cache_control` that turns the prompt cache on. None of them is
required, and a gateway that refuses a field it does not know would refuse
*every* request carrying one -- which is why `provider.rs` carries the answer
per preset instead of sending everything and hoping.

This asks the endpoint itself, one field at a time, so the answer is evidence
rather than a guess. A field that comes back 2xx is a field it will *take*; the
second phase asks whether the field does anything — a repeated prefix reports
cache reads, and a thinking request reports whether reasoning still streams —
because "accepted" and "in effect" are different answers and only the second one
is worth turning on.

Usage:
    scripts/anthropic_probe.py                 # the MiniMax preset
    scripts/anthropic_probe.py --provider deepseek   # a field is a no-op there

The key is the one `/login` stored (`~/.caocli/settings.json`), or
`CAOCLI_PROBE_KEY`. Nothing else is sent: every probe is one tiny request
(max_tokens 1) that costs a fraction of a cent, and the last one deliberately
asks for an answer too long to finish, to see the truncation report arrive.
"""

import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.request

# The preset table's endpoints, so the script asks the same URL the program
# would. Kept in step by hand: this is an operator's tool, not a code path.
ENDPOINTS = {
    "minimax": "https://api.minimax.cn/anthropic/v1/messages",
    "deepseek": "https://api.deepseek.com/chat/completions",
}
MODELS = {"minimax": "MiniMax-M3", "deepseek": "deepseek-flash"}

API_VERSION = "2023-06-01"


def stored_key(provider: str) -> str | None:
    key = os.environ.get("CAOCLI_PROBE_KEY")
    if key:
        return key
    path = os.path.expanduser("~/.caocli/settings.json")
    try:
        with open(path) as f:
            return json.load(f).get("providers", {}).get(provider, {}).get("api_key")
    except (OSError, ValueError):
        return None


def post(url: str, key: str, body: dict) -> tuple[int, str]:
    """One request. Returns the status and the body, whatever the status."""
    request = urllib.request.Request(
        url,
        data=json.dumps(body).encode(),
        headers={
            "content-type": "application/json",
            "anthropic-version": API_VERSION,
            "authorization": f"Bearer {key}",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=120) as response:
            return response.status, response.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()
    except OSError as e:
        return 0, str(e)


def probe(provider: str, key: str) -> int:
    url = ENDPOINTS[provider]
    model = MODELS[provider]
    base = {
        "model": model,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "ping"}],
        "stream": True,
    }
    if provider != "minimax":
        print(f"{provider} speaks the OpenAI wire; none of these fields exist there.")
        return 0

    fields = {
        "thinking.display": {"thinking": {"type": "adaptive", "display": "summarized"}},
        "output_config.effort": {"output_config": {"effort": "low"}},
        "cache_control": {"cache_control": {"type": "ephemeral"}},
    }
    print(f"Probing {provider} at {url}\n")
    accepted = {}
    for name, extra in fields.items():
        status, body = post(url, key, {**base, **extra})
        ok = 200 <= status < 300
        accepted[name] = ok
        detail = "" if ok else " " + body.replace("\n", " ")[:300]
        print(f"  {'yes' if ok else 'NO '}  {name}{detail}")

    # The required shape, for a baseline: if this fails, nothing above means
    # anything.
    status, body = post(url, key, base)
    print(f"\n  {'yes' if 200 <= status < 300 else 'NO '}  the required shape (baseline)")
    if not 200 <= status < 300:
        print(f"      {body[:300]}")
        return 1

    # Truncation: an answer that cannot fit in one token, so `stop_reason`
    # has to say so. This is what the turn reports to the reader.
    long = dict(base, max_tokens=1, messages=[{"role": "user", "content": "Count to a hundred."}])
    status, body = post(url, key, long)
    if 200 <= status < 300:
        print(f"  stop_reason reported as: {stop_reason(body)!r}")

    in_effect = {}
    if accepted["cache_control"]:
        in_effect["cache"] = cache_effect(url, key, model)
    if accepted["thinking.display"]:
        in_effect["thinking"] = thinking_effect(url, key, model)

    print("\nWhat to put in `provider.rs` — a field goes on when it is *served*, and")
    print("`effort` only when this preset's tiers are the standard levels")
    print("(low/medium/high/xhigh/max) rather than a switch of its own:")
    for name in ("effort", "display", "cache_control"):
        note = "  # only if this preset's tiers are the standard levels"
        if name == "display" and in_effect.get("thinking") is not None:
            note = "  # reasoning text streams" if in_effect["thinking"] else "  # NO reasoning text"
        if name == "cache_control" and in_effect.get("cache") is not None:
            note = "  # cache reads measured" if in_effect["cache"] else "  # NO cache reads"
        print(f"    {name}: {str(accepted[{'effort': 'output_config.effort', 'display': 'thinking.display', 'cache_control': 'cache_control'}[name]]).lower()}{note}")
    return 0


def prefix() -> str:
    """A prompt long enough to be cacheable: the shortest cacheable prompt is
    around a thousand tokens, so this is a few thousand.

    Salted per run, or a run would read the cache the last one wrote and the
    difference between a first and a second request — the whole measurement —
    would be invisible."""
    salt = str(int(time.time() * 1000) % 1_000_000)
    return f"You are a careful assistant ({salt}). " + (
        "The rules are simple and must be followed exactly. " * 220
    )


def cache_effect(url: str, key: str, model: str) -> bool:
    """Whether a repeated prefix is actually read from the cache.

    The same request twice: the second one's `cache_read_input_tokens` is the
    answer, and a first request that reports almost none is what makes the
    difference readable.
    """
    body = {
        "model": model,
        "max_tokens": 8,
        "system": prefix(),
        "messages": [{"role": "user", "content": "Reply with the single word: ok"}],
        "stream": True,
        "cache_control": {"type": "ephemeral"},
    }
    first = usage_of(post(url, key, body)[1])
    second = usage_of(post(url, key, body)[1])
    read = second.get("cache_read_input_tokens", 0)
    print(f"\n  cache: first report {first.get('cache_read_input_tokens', 0)} read,"
          f" second {read} read")
    return read > 0


def thinking_effect(url: str, key: str, model: str) -> bool:
    """Whether the reasoning text still arrives with `display: summarized`."""
    body = {
        "model": model,
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "What is 17 times 23? Think it through."}],
        "stream": True,
        "thinking": {"type": "adaptive", "display": "summarized"},
    }
    kinds = delta_kinds(post(url, key, body)[1])
    print(f"  thinking: deltas seen {kinds}")
    return kinds.get("thinking_delta", 0) > 0


def usage_of(body: str) -> dict:
    """The usage a stream reported, as the merging of its events leaves it."""
    total: dict = {}
    for event in events_of(body):
        usage = event.get("usage")
        if isinstance(usage, dict):
            total.update({k: v for k, v in usage.items() if v is not None})
        if event.get("type") == "message_start":
            usage = (event.get("message") or {}).get("usage")
            if isinstance(usage, dict):
                total.update({k: v for k, v in usage.items() if v is not None})
    return total


def delta_kinds(body: str) -> dict:
    """How many deltas of each kind a stream carried."""
    kinds: dict = {}
    for event in events_of(body):
        if event.get("type") == "content_block_delta":
            kind = (event.get("delta") or {}).get("type", "?")
            kinds[kind] = kinds.get(kind, 0) + 1
    return kinds


def events_of(body: str) -> list:
    """The JSON payloads of a stream's data lines."""
    out = []
    for line in body.splitlines():
        if not line.startswith("data:"):
            continue
        try:
            out.append(json.loads(line[len("data:") :].strip()))
        except ValueError:
            continue
    return out


def stop_reason(body: str) -> str | None:
    """The last `stop_reason` of a stream, if the endpoint reported one."""
    reason = None
    for line in body.splitlines():
        if not line.startswith("data:"):
            continue
        try:
            event = json.loads(line[len("data:") :].strip())
        except ValueError:
            continue
        if event.get("type") == "message_start":
            reason = (event.get("message") or {}).get("stop_reason") or reason
        if event.get("type") == "message_delta":
            reason = (event.get("delta") or {}).get("stop_reason") or reason
    return reason


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--provider", default="minimax", choices=sorted(ENDPOINTS))
    args = parser.parse_args()
    key = stored_key(args.provider)
    if not key:
        print(f"no key for {args.provider}: run /login {args.provider}, or set CAOCLI_PROBE_KEY")
        return 1
    return probe(args.provider, key)


if __name__ == "__main__":
    sys.exit(main())
