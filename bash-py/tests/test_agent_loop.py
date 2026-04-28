"""Verify the create_bash → tool-use loop pattern works end-to-end (mocked volume)."""
from __future__ import annotations

import pytest
from supermemory_bash._shell import Shell, ExecResult
from supermemory_bash._tool_description import TOOL_DESCRIPTION
from tests.test_shell import FakeVolume


@pytest.fixture
def agent_env():
    vol = FakeVolume()
    shell = Shell(vol, cwd="/home/user")  # type: ignore[arg-type]
    tool_def = {
        "name": "bash",
        "description": TOOL_DESCRIPTION,
        "input_schema": {
            "type": "object",
            "properties": {"cmd": {"type": "string"}},
            "required": ["cmd"],
        },
    }
    return shell, vol, tool_def


@pytest.mark.asyncio
async def test_agent_write_read_search(agent_env):
    """Simulate an agent that writes a file, reads it back, and searches."""
    shell, vol, tool_def = agent_env

    # Agent writes a note
    r = await shell.exec("mkdir -p /notes")
    assert r.exit_code == 0

    r = await shell.exec("echo 'OAuth2 token refresh handles expired sessions' > /notes/auth.md")
    assert r.exit_code == 0

    # Agent reads it back
    r = await shell.exec("cat /notes/auth.md")
    assert r.exit_code == 0
    assert "OAuth2" in r.stdout

    # Agent lists the directory
    r = await shell.exec("ls /notes")
    assert "auth.md" in r.stdout

    # Agent searches semantically
    r = await shell.exec("sgrep 'OAuth2 token' /notes/")
    assert r.exit_code == 0
    assert "auth.md" in r.stdout

    # Agent uses grep for exact match
    r = await shell.exec("grep 'expired' /notes/auth.md")
    assert r.exit_code == 0
    assert "expired" in r.stdout


@pytest.mark.asyncio
async def test_agent_multi_step_pipeline(agent_env):
    """Agent chains commands like a real LLM session."""
    shell, vol, _ = agent_env

    await shell.exec("echo 'apple' > /fruits.txt")
    await shell.exec("echo 'banana' >> /fruits.txt")
    await shell.exec("echo 'cherry' >> /fruits.txt")

    r = await shell.exec("cat /fruits.txt | sort | head -n 2")
    lines = r.stdout.strip().split("\n")
    assert len(lines) == 2
    assert lines[0] == "apple"
    assert lines[1] == "banana"


@pytest.mark.asyncio
async def test_agent_error_handling(agent_env):
    """Agent gets proper error feedback for missing files."""
    shell, vol, _ = agent_env

    r = await shell.exec("cat /does/not/exist.txt")
    assert r.exit_code != 0
    assert "No such file" in r.stderr

    # Agent can recover using ||
    r = await shell.exec("cat /missing || echo 'file not found'")
    assert "file not found" in r.stdout


@pytest.mark.asyncio
async def test_agent_conditional_check(agent_env):
    """Agent uses test/[ ] to check file existence before acting."""
    shell, vol, _ = agent_env

    await shell.exec("echo 'data' > /config.txt")

    # Check + conditional action
    r = await shell.exec("test -f /config.txt && echo 'exists' || echo 'missing'")
    assert "exists" in r.stdout

    r = await shell.exec("test -f /nope.txt && echo 'exists' || echo 'missing'")
    assert "missing" in r.stdout


@pytest.mark.asyncio
async def test_tool_description_present(agent_env):
    """The tool description is non-empty and mentions key features."""
    _, _, tool_def = agent_env
    desc = tool_def["description"]
    assert "sgrep" in desc
    assert "bash" in desc.lower() or "shell" in desc.lower()
    assert len(desc) > 100
