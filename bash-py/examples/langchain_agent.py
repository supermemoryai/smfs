"""
Example: using supermemory-bash as a tool in a LangChain agent.

Usage:
    export SUPERMEMORY_API_KEY=sm-...
    export OPENAI_API_KEY=sk-...
    uv run --with langchain --with langchain-openai --with supermemory-bash \
        python examples/langchain_agent.py
"""

from __future__ import annotations

import asyncio
import os

from langchain.agents import create_agent
from langchain_core.tools import tool
from langchain_openai import ChatOpenAI

from supermemory_bash import create_bash

CONTAINER_TAG = os.environ.get("CONTAINER_TAG", "user_42")
MODEL = os.environ.get("OPENAI_MODEL", "gpt-4o")
PROMPT = os.environ.get(
    "PROMPT",
    "Create /notes/langchain.txt containing 'Hello from LangChain!', then cat it back to confirm.",
)


async def run_agent(user_message: str) -> str:
    result = await create_bash(
        api_key=os.environ["SUPERMEMORY_API_KEY"],
        container_tag=CONTAINER_TAG,
    )
    bash = result.bash

    @tool("bash", description=result.tool_description)
    async def bash_tool(cmd: str) -> str:
        """Execute a bash command in the Supermemory-backed shell."""
        print(f"[bash] $ {cmd}")
        exec_result = await bash.exec(cmd)
        return "\n".join(
            part
            for part in [
                exec_result.stdout or None,
                f"[stderr]\n{exec_result.stderr}" if exec_result.stderr else None,
                f"[exit {exec_result.exit_code}]",
            ]
            if part is not None
        )

    llm = ChatOpenAI(api_key=os.environ["OPENAI_API_KEY"], model=MODEL)
    agent = create_agent(model=llm, tools=[bash_tool])

    response = await agent.ainvoke(
        {"messages": [{"role": "user", "content": user_message}]},
        {"recursion_limit": 16},
    )
    final_message = response["messages"][-1]
    content = getattr(final_message, "content", "")
    if isinstance(content, str):
        return content
    return str(content)


async def main() -> None:
    answer = await run_agent(PROMPT)
    print(f"\n[assistant] {answer}\n")


if __name__ == "__main__":
    asyncio.run(main())
