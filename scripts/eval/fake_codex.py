#!/usr/bin/env python3
"""STUB Codex CLI for the device-free evaluation turn. It is not a model.

It accepts the exact `codex --search exec ...` command line that
bridge/codex_bridge.py:run_codex builds, checks the parts the bridge relies on
(schema file, PNG inputs, --output-last-message), records what the bridge sent,
and answers with the canned JSON in $FAKE_CODEX_ANSWER. Its stdout is the JSONL
event stream the bridge counts web lookups from; it contains only read-only
event types, so a failure here would still be marked safe to fall back.
"""
import json
import os
import sys
from pathlib import Path


def main():
    argv = sys.argv[1:]
    record = Path(os.environ["FAKE_CODEX_RECORD"])
    record.mkdir(parents=True, exist_ok=True)

    def value(flag):
        return argv[argv.index(flag) + 1]

    images = [argv[i + 1] for i, a in enumerate(argv) if a == "--image"]
    for image in images:
        assert Path(image).read_bytes().startswith(b"\x89PNG\r\n\x1a\n"), image
    schema = json.loads(Path(value("--output-schema")).read_text())
    assert schema["required"] == ["lines", "illustrations"], schema
    instructions = sys.stdin.read()
    # Record the command shape with temporary paths reduced to file names.
    shown = [Path(a).name if os.path.isabs(a) else a for a in argv]
    (record / "codex-argv.json").write_text(json.dumps(shown, indent=1) + "\n")
    (record / "bridge-instructions.txt").write_text(instructions, encoding="utf-8")
    (record / "schema.json").write_text(json.dumps(schema, indent=1) + "\n")
    calls = record / "stub-invocations.txt"
    calls.write_text(str(int(calls.read_text()) + 1 if calls.exists() else 1))

    answer = json.loads(Path(os.environ["FAKE_CODEX_ANSWER"]).read_text())
    answer.pop("_label", None)
    Path(value("--output-last-message")).write_text(json.dumps(answer))
    for event in ({"type": "thread.started", "thread_id": "stub"}, {"type": "turn.started"},
                  {"type": "item.completed", "item": {"type": "agent_message", "text": "(stub)"}},
                  {"type": "turn.completed", "usage": {}}):
        print(json.dumps(event))


if __name__ == "__main__":
    main()
