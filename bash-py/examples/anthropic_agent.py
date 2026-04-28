"""
Example: using supermemory-bash as a tool in a Claude agent loop.

Usage:
    export SUPERMEMORY_API_KEY=sm-...
    export ANTHROPIC_API_KEY=sk-ant-...
    uv run --with anthropic --with supermemory-bash python examples/anthropic_agent.py
"""

import asyncio
import os

import anthropic

from supermemory_bash import create_bash


async def run_agent(user_message: str) -> str:
    result = await create_bash(
        api_key=os.environ["SUPERMEMORY_API_KEY"],
        container_tag="sm_project_default",
    )
    bash = result.bash

    client = anthropic.Anthropic(api_key=os.environ["ANTHROPIC_API_KEY"])
    tools = [
        {
            "name": "bash",
            "description": result.tool_description,
            "input_schema": {
                "type": "object",
                "properties": {
                    "cmd": {"type": "string", "description": "The bash command to run."}
                },
                "required": ["cmd"],
            },
        }
    ]

    messages: list[dict] = [{"role": "user", "content": user_message}]

    for _ in range(10):
        response = client.messages.create(
            model="claude-sonnet-4-20250514",
            max_tokens=4096,
            tools=tools,
            messages=messages,
        )

        if response.stop_reason == "end_turn":
            for block in response.content:
                if hasattr(block, "text"):
                    return block.text
            return ""

        # Process tool calls
        messages.append({"role": "assistant", "content": response.content})
        tool_results = []
        for block in response.content:
            if block.type == "tool_use":
                cmd = block.input.get("cmd", "")
                print(f"  > {cmd}")
                r = await bash.exec(cmd)
                output = r.stdout
                if r.stderr:
                    output += f"\n[stderr]: {r.stderr}"
                if r.exit_code != 0:
                    output += f"\n[exit_code]: {r.exit_code}"
                tool_results.append(
                    {
                        "type": "tool_result",
                        "tool_use_id": block.id,
                        "content": output or "(no output)",
                    }
                )
        messages.append({"role": "user", "content": tool_results})

    return "(max steps reached)"


async def main() -> None:
    print("Agent: searching for notes about authentication...\n")
    answer = await run_agent("tell me the life story of me")
    print(f"\nAnswer:\n{answer}")


if __name__ == "__main__":
    asyncio.run(main())
