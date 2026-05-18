"""
Example: using supermemory-bash as a tool with the OpenAI SDK.

Usage:
    export SUPERMEMORY_API_KEY=sm-...
    export OPENAI_API_KEY=sk-...
    uv run --with openai --with supermemory-bash python examples/openai_agent.py
"""

from __future__ import annotations

import asyncio
import json
import os
from typing import Any

from openai import AsyncOpenAI

from supermemory_bash import create_bash

CONTAINER_TAG = os.environ.get("CONTAINER_TAG", "user_42")
MODEL = os.environ.get("OPENAI_MODEL", "gpt-4o")
PROMPT = os.environ.get(
    "PROMPT",
    "Create /notes/openai.txt containing 'Hello from OpenAI SDK!', then cat it back to confirm.",
)


async def run_agent(user_message: str) -> str:
    result = await create_bash(
        api_key=os.environ["SUPERMEMORY_API_KEY"],
        container_tag=CONTAINER_TAG,
    )
    bash = result.bash

    client = AsyncOpenAI(api_key=os.environ["OPENAI_API_KEY"])
    tools: list[dict[str, Any]] = [
        {
            "type": "function",
            "function": {
                "name": "bash",
                "description": result.tool_description,
                "parameters": {
                    "type": "object",
                    "properties": {
                        "cmd": {"type": "string", "description": "The bash command to execute."}
                    },
                    "required": ["cmd"],
                    "additionalProperties": False,
                },
            },
        }
    ]

    messages: list[dict[str, Any]] = [{"role": "user", "content": user_message}]

    for _ in range(8):
        response = await client.chat.completions.create(
            model=MODEL,
            messages=messages,
            tools=tools,
        )

        message = response.choices[0].message
        messages.append(message.model_dump(exclude_none=True))

        if not message.tool_calls:
            return message.content or ""

        for tool_call in message.tool_calls:
            if tool_call.type != "function" or tool_call.function.name != "bash":
                continue

            args = json.loads(tool_call.function.arguments or "{}")
            cmd = args.get("cmd", "")
            print(f"[bash] $ {cmd}")

            exec_result = await bash.exec(cmd)
            output = "\n".join(
                part
                for part in [
                    exec_result.stdout or None,
                    f"[stderr]\n{exec_result.stderr}" if exec_result.stderr else None,
                    f"[exit {exec_result.exit_code}]",
                ]
                if part is not None
            )
            messages.append(
                {
                    "role": "tool",
                    "tool_call_id": tool_call.id,
                    "content": output,
                }
            )

    return "(max steps reached)"


async def main() -> None:
    answer = await run_agent(PROMPT)
    print(f"\n[assistant] {answer}\n")


if __name__ == "__main__":
    asyncio.run(main())
