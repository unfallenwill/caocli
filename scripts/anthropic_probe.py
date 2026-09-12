#!/usr/bin/env python3
"""Ask an Anthropic-compatible endpoint which standard optional fields it takes.

The protocol has three optional fields this program knows how to send and an
endpoint is free not to serve: `output_config.effort`, `thinking.display`, and
the top-level `cache_control` that turns the prompt cache on. None of them is
required, and a gateway that refuses a field it does not know would refuse
*every* request carrying one -- which is why `provider.rs` carries the answer
per preset instead of sending everything and hoping.

This asks the endpoint itself, one field at a time, so the answer is evidence
rather than a guess. A field that comes back 2xx is a field to turn on in the
preset; a 4xx names the reason it will not do.

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

    print("\nSet the ones that came back yes in `provider.rs`:")
    print(f"    anthropic: AnthropicOptions {{ effort: {str(accepted['output_config.effort']).lower()}, "
          f"display: {str(accepted['thinking.display']).lower()}, "
          f"cache_control: {str(accepted['cache_control']).lower()} }},")
    return 0


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
