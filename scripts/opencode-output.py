#!/usr/bin/env python3
"""Read the latest OpenCode answer through Herdr's session record (no screen scraping)."""

import argparse
import json
import math
import re
import subprocess
import sys


def last_answer(messages):
    """Only accept the latest execution, closed by an explicit successful idle record."""
    if not messages or messages[-1].get("type") != "idle":
        return {"status": "not_completed", "text": None}
    if messages[-1].get("outcome") != "succeeded":
        return {"status": messages[-1].get("outcome", "unknown"), "text": None}
    end = len(messages) - 1
    start = next(
        (i + 1 for i in range(end - 1, -1, -1) if messages[i].get("type") == "idle"),
        0,
    )
    turn_start = start
    # User/synthetic input starts a new request; never borrow a previous request's answer.
    start = next(
        (i + 1 for i in range(end - 1, start - 1, -1)
         if messages[i].get("type") in ("user", "synthetic")),
        start,
    )
    answer = next(
        (m for m in reversed(messages[start:end]) if m.get("type") == "assistant"),
        None,
    )
    if not answer or not answer.get("time", {}).get("completed") or answer.get("error"):
        return {"status": "no_text", "text": None}
    text = "\n".join(
        part["text"] for part in answer.get("content", []) if part.get("type") == "text"
    ).strip()
    result = {"status": "ok" if text else "no_text", "text": text or None}
    if text:
        result.update(turn_statistics(messages[turn_start:end + 1]))
        model = answer.get("model", {}).get("id")
        if isinstance(model, str) and model.strip():
            result["model"] = model.strip()
        time = answer.get("time", {})
        first = time.get("firstToken")
        ended = time.get("streamed", time.get("completed"))
        tokens = answer.get("tokens", {}).get("output")
        if all(isinstance(n, (int, float)) and not isinstance(n, bool)
               and math.isfinite(n) and n >= 0 for n in (first, ended, tokens)) and ended > first:
            result["tokens_per_second"] = tokens * 1000 / (ended - first)
    return result


def turn_statistics(messages):
    """Whole execution, including steered prompts, tools and user waits (milliseconds)."""
    def number(value):
        return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value) and value >= 0

    result = {}
    prompt = next((m for m in messages if m.get("type") in ("user", "synthetic")), None)
    started = (prompt or {}).get("time", {}).get("created")
    ended = messages[-1].get("time", {}).get("created")
    if number(started) and number(ended) and ended >= started:
        result["duration_ms"] = int(ended - started)
    calls = [m for m in messages if m.get("type") == "assistant"]
    counts = [m.get("tokens", {}).get("output") for m in calls]
    if calls and all(number(n) for n in counts):
        result["output_tokens"] = int(sum(counts))
    return result


def read_json(command):
    result = subprocess.run(command, check=True, capture_output=True, text=True, timeout=3)
    return json.loads(result.stdout)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("terminal", help="Herdr terminal address, e.g. t2r")
    parser.add_argument("--session", help="Exact OpenCode session ID when the terminal has multiple records")
    parser.add_argument("--completed-session", help="Previously captured busy-session binding (idle clears records)")
    args = parser.parse_args()
    terminal = read_json(["hc", "get", args.terminal, "--json"])
    sessions = [key for key, value in terminal.get("records", {}).items()
                if value.get("app") == "opencode" and re.fullmatch(r"ses[\w.+-]*", key)]
    if args.completed_session:
        if not re.fullmatch(r"ses[\w.+-]*", args.completed_session):
            raise ValueError("Invalid captured session ID")
        if sessions and sessions != [args.completed_session]:
            raise ValueError("Terminal session changed since completion")
        sessions = [args.completed_session]
    elif args.session:
        if args.session not in sessions:
            raise ValueError("Requested session is not an OpenCode record on this terminal")
        sessions = [args.session]
    if len(sessions) != 1:
        raise ValueError("Need exactly one OpenCode session record; use --session to select one")
    if "/" in terminal.get("address", args.terminal):
        raise ValueError("Remote terminals require an explicit remote OpenCode connection; not supported yet")
    # OpenCode handles discovery/authentication. A hashed OSC record will not resolve:
    # fail rather than guessing another session from its directory or title.
    response = read_json(["opencode", "api", "get", f"/api/session/{sessions[0]}/context"])
    print(json.dumps({"terminal": terminal["address"], "session": sessions[0],
                      **last_answer(response["data"])}, ensure_ascii=False))


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, OSError, subprocess.SubprocessError) as error:
        print(json.dumps({"status": "error", "error": str(error)}, ensure_ascii=False))
        sys.exit(1)
