#!/usr/bin/env python3
"""Validate the golden Responses fixtures against the official openai-python models.

Every `*.jsonl` stream fixture must parse, event by event, as
`openai.types.responses.ResponseStreamEvent`, and `non_streaming.jsonl` as
`openai.types.responses.Response`. Run it after re-blessing the fixtures
(`FERROX_BLESS=1 cargo test -p ferrox-providers responses_emitter`):

    python -m venv /tmp/oai && /tmp/oai/bin/pip install openai
    /tmp/oai/bin/python ferrox-providers/tests/fixtures/responses/validate_sdk.py
"""

import json
import pathlib
import sys

from pydantic import TypeAdapter

from openai.types.responses import Response, ResponseStreamEvent

HERE = pathlib.Path(__file__).parent
stream_event = TypeAdapter(ResponseStreamEvent)
failures = 0

for path in sorted(HERE.glob("*.jsonl")):
    for n, line in enumerate(path.read_text().splitlines(), 1):
        frame = json.loads(line)
        try:
            if path.stem == "non_streaming":
                Response.model_validate(frame, strict=False)
            else:
                event = stream_event.validate_python(frame["data"])
                assert event.type == frame["event"], f"event: {frame['event']} != type {event.type}"
        except Exception as e:  # noqa: BLE001 - report every failure, then exit non-zero
            failures += 1
            print(f"FAIL {path.name}:{n}: {str(e)[:500]}")
    print(f"ok   {path.name}")

sys.exit(1 if failures else 0)
