#!/usr/bin/env python3
"""A nyq extension in plain Python (standard library only).

Adds a read-only `word_count` tool, a `/hello` command, and a `tool_call`
guard that blocks `rm -rf` in shell commands. nyq talks JSON-RPC 2.0 over
stdin/stdout, one JSON object per line; anything printed to stderr goes to
~/.nyq/logs/ext-word-tools.log.
"""
import json
import re
import sys

RM_RF = re.compile(r"\brm\s+(-[a-zA-Z]*r[a-zA-Z]*f|-[a-zA-Z]*f[a-zA-Z]*r|-r\s+-f|-f\s+-r)\b")


def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def initialize(params):
    print(f"started for {params['cwd']} ({params['mode']})", file=sys.stderr)
    return {
        "tools": [{
            "name": "word_count",
            "description": "Count the lines, words and characters of a text.",
            "input_schema": {
                "type": "object",
                "properties": {"text": {"type": "string", "description": "The text to count"}},
                "required": ["text"],
            },
            "read_only": True,
        }],
        "commands": [{"name": "hello", "description": "Say hello", "args_hint": "[name]"}],
        "events": ["tool_call"],
    }


def word_count(text):
    return f"{len(text.splitlines())} lines, {len(text.split())} words, {len(text)} characters"


def handle(method, params):
    if method == "initialize":
        return initialize(params)
    if method == "tool/call" and params["name"] == "word_count":
        return {"content": [{"type": "text", "text": word_count(params["input"].get("text", ""))}]}
    if method == "command/run" and params["name"] == "hello":
        name = params.get("args") or "there"
        return {"message": f"Hello, {name}! (from the word-tools extension)"}
    if method == "event/tool_call":
        command = str(params["input"].get("command", "")) if params["tool"] == "bash" else ""
        if RM_RF.search(command):
            return {"block": "rm -rf is blocked by the word-tools extension"}
        return {}
    if method.startswith("event/"):
        return {}
    raise KeyError(method)


for line in sys.stdin:
    try:
        msg = json.loads(line)
    except ValueError:
        continue
    if "method" not in msg:
        continue  # an answer to a request we never send
    try:
        reply = {"result": handle(msg["method"], msg.get("params") or {})}
    except KeyError as e:
        reply = {"error": {"code": -32601, "message": f"unknown method {e}"}}
    if "id" in msg:
        send({"jsonrpc": "2.0", "id": msg["id"], **reply})
