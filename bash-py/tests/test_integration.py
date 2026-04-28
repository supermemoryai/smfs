"""End-to-end test against a live Supermemory container. Skipped unless
SUPERMEMORY_API_KEY is set, so default test runs (without secrets) stay green.

Run locally with:
    SUPERMEMORY_API_KEY=sk-... uv run pytest tests/test_integration.py -v
"""
from __future__ import annotations

import asyncio
import os
import secrets
import time

import pytest

pytestmark = pytest.mark.skipif(
    not os.environ.get("SUPERMEMORY_API_KEY"),
    reason="needs SUPERMEMORY_API_KEY",
)

from supermemory_bash import create_bash
from supermemory_bash._volume import SupermemoryVolume

CONTAINER_TAG = f"bash_integ_py_{int(time.time())}_{secrets.token_hex(3)}"

SEED_FILES = [
    ("/todo.md", "- [ ] write the report\n- [ ] review pull requests\n- [x] respond to mom\n"),
    (
        "/journal/2026-04-25.md",
        "Friday — long debug session on the rename bug. Found that PATCH with content silently ignores filepath. PATCH without content honors it.\n",
    ),
    (
        "/journal/2026-04-26.md",
        "Saturday — built the createBash factory. The synthetic-dir + customCommand resolution interaction was painful.\n",
    ),
    (
        "/work/projects/auth.md",
        "OAuth implementation handles refresh tokens with a 30-day TTL. Access tokens expire in 15 minutes. Bearer tokens are passed in the Authorization header.\n",
    ),
    (
        "/work/projects/billing.md",
        "Stripe webhooks for invoice.paid, subscription.updated, customer.subscription.deleted. Webhook signing secret rotates every 90 days.\n",
    ),
    ("/work/notes.md", "Standup at 10am Mondays. Sprint planning every other Wednesday.\n"),
    (
        "/reading/highlights.txt",
        "Photosynthesis is the process by which plants convert sunlight into chemical energy. The reaction takes place in chloroplasts.\n",
    ),
]


async def wait_terminal(volume: SupermemoryVolume, doc_id: str, max_s: float = 30.0) -> str | None:
    start = time.monotonic()
    while time.monotonic() - start < max_s:
        try:
            got = await volume.client._request("GET", f"/v3/documents/{doc_id}")
            status = got.get("status")
            if status in ("done", "failed"):
                return status
        except Exception:
            pass
        await asyncio.sleep(1.5)
    return None


@pytest.fixture(scope="module")
async def session():
    api_key = os.environ["SUPERMEMORY_API_KEY"]
    print(f"\n[integration] container: {CONTAINER_TAG}")
    res = await create_bash(api_key=api_key, container_tag=CONTAINER_TAG, eager_load=True, eager_content=True)
    bash, vol = res.bash, res.volume
    await vol.remove_by_prefix("/")
    for path, content in SEED_FILES:
        await vol.add_doc(path, content)
    for path, _ in SEED_FILES:
        doc_id = vol.path_index.resolve(path)
        if doc_id:
            await wait_terminal(vol, doc_id)
    yield bash, vol
    try:
        await vol.remove_by_prefix("/")
    except Exception as e:
        print(f"[integration] cleanup failed: {e}")
    await vol.client.close()


# ── core parity (matches bash/tests/integration.test.ts) ──────────────────


async def test_pwd_returns_root(session):
    bash, _ = session
    r = await bash.exec("pwd")
    assert r.exit_code == 0
    assert r.stdout.strip() == "/"


async def test_ls_root_lists_only_seeded_top_level_entries(session):
    bash, _ = session
    r = await bash.exec("ls /")
    assert r.exit_code == 0
    seen = set(r.stdout.split())
    for expected in ("journal", "reading", "todo.md", "work"):
        assert expected in seen, f"missing {expected!r} in 'ls /' output: {seen!r}"
    for unexpected in ("dev", "home", "tmp"):
        assert unexpected not in seen, f"'{unexpected}' should not appear in 'ls /'"


async def test_ls_work_lists_nested_entries(session):
    bash, _ = session
    r = await bash.exec("ls /work/")
    assert r.exit_code == 0
    seen = r.stdout.split()
    assert "notes.md" in seen
    assert "projects" in seen


async def test_ls_work_projects_lists_deeper_files(session):
    bash, _ = session
    r = await bash.exec("ls /work/projects/")
    assert r.exit_code == 0
    seen = r.stdout.split()
    assert "auth.md" in seen
    assert "billing.md" in seen


async def test_cat_returns_seeded_content(session):
    bash, _ = session
    r = await bash.exec("cat /todo.md")
    assert r.exit_code == 0
    assert "write the report" in r.stdout


async def test_grep_F_finds_literal_substring(session):
    bash, _ = session
    r = await bash.exec("grep -F 'OAuth' /work/projects/auth.md")
    assert r.exit_code == 0
    assert "OAuth" in r.stdout


async def test_pipe_chain_returns_byte_count(session):
    bash, _ = session
    r = await bash.exec("cat /work/projects/auth.md | head -1 | wc -c")
    assert r.exit_code == 0
    n = int(r.stdout.strip())
    assert n > 10


async def test_stat_exits_zero(session):
    bash, _ = session
    r = await bash.exec("stat /reading/highlights.txt")
    assert r.exit_code == 0
    assert len(r.stdout) > 0


async def test_file_and_dir_tests(session):
    bash, _ = session
    f = await bash.exec("[ -f /todo.md ] && echo file")
    assert f.stdout.strip() == "file"
    d = await bash.exec("[ -d /work ] && echo dir")
    assert d.stdout.strip() == "dir"


async def test_find_md_files(session):
    bash, _ = session
    r = await bash.exec("find /work -name '*.md'")
    assert r.exit_code == 0


async def test_append_preserves_doc_id(session):
    bash, vol = session
    before_id = vol.path_index.resolve("/todo.md")
    assert before_id
    await wait_terminal(vol, before_id)
    r1 = await bash.exec("echo '- [x] write the report' >> /todo.md")
    assert r1.exit_code == 0
    after_id = vol.path_index.resolve("/todo.md")
    assert after_id == before_id, f"docId changed from {before_id} to {after_id}"
    r2 = await bash.exec("cat /todo.md")
    assert "- [x] write the report" in r2.stdout


async def test_overwrite_replaces_content(session):
    bash, vol = session
    before_id = vol.path_index.resolve("/work/notes.md")
    if before_id:
        await wait_terminal(vol, before_id)
    r1 = await bash.exec("echo 'rewritten content' > /work/notes.md")
    assert r1.exit_code == 0
    r2 = await bash.exec("cat /work/notes.md")
    assert r2.stdout.strip() == "rewritten content"


async def test_mv_keeps_doc_id_stable(session):
    bash, vol = session
    before_id = vol.path_index.resolve("/journal/2026-04-25.md")
    assert before_id
    await wait_terminal(vol, before_id)
    r = await bash.exec("mv /journal/2026-04-25.md /journal/friday.md")
    assert r.exit_code == 0
    new_id = vol.path_index.resolve("/journal/friday.md")
    assert new_id == before_id
    assert vol.path_index.resolve("/journal/2026-04-25.md") is None


async def test_cp_copies_file(session):
    bash, _ = session
    r1 = await bash.exec("cp /todo.md /todo-backup.md")
    assert r1.exit_code == 0
    r2 = await bash.exec("cat /todo-backup.md")
    assert "write the report" in r2.stdout


async def test_mkdir_p_creates_nested_dirs(session):
    bash, vol = session
    r = await bash.exec("mkdir -p /scratch/temp/today")
    assert r.exit_code == 0
    assert vol.path_index.is_directory("/scratch/temp/today")


async def test_rm_deletes_file(session):
    bash, vol = session
    backup_id = vol.path_index.resolve("/todo-backup.md")
    if backup_id:
        await wait_terminal(vol, backup_id)
    r1 = await bash.exec("rm /todo-backup.md")
    assert r1.exit_code == 0, f"rm failed: {r1.stderr!r}"
    r2 = await bash.exec("[ ! -f /todo-backup.md ] && echo gone")
    assert r2.stdout.strip() == "gone"


async def test_sgrep_finds_seeded_file_by_topic(session):
    bash, vol = session
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        r = await vol.search("OAuth refresh tokens")
        if any((x.filepath or "").startswith("/work/") for x in r.results):
            break
        await asyncio.sleep(3)
    else:
        pytest.skip("memory pipeline did not surface the seeded content within 60s")
    r = await bash.exec("sgrep 'OAuth refresh tokens'")
    assert r.exit_code == 0
    assert "/work/projects/auth.md" in r.stdout


# ── new cases: profile virtual file + validation pipeline ─────────────────


async def test_profile_md_appears_in_ls_root(session):
    bash, _ = session
    r = await bash.exec("ls /")
    assert "profile.md" in r.stdout.split(), r.stdout


async def test_cat_profile_returns_real_content(session):
    bash, _ = session
    r = await bash.exec("cat /profile.md")
    assert r.exit_code == 0
    assert "# Memory Profile" in r.stdout


async def test_write_to_profile_rejected_with_eperm(session):
    bash, _ = session
    r = await bash.exec("echo overwrite > /profile.md")
    assert r.exit_code == 1
    assert "EPERM" in r.stderr


async def test_filepath_collision_returns_enotdir(session):
    bash, _ = session
    probe = "/probe-collision.md"
    w1 = await bash.exec(f"echo first > {probe}")
    assert w1.exit_code == 0
    w2 = await bash.exec(f"echo second > {probe}/inner.md")
    assert w2.exit_code == 1, w2
    assert "ENOTDIR" in w2.stderr or "Not a directory" in w2.stderr, w2.stderr


async def test_invalid_filepath_shape_returns_einval(session):
    bash, _ = session
    r = await bash.exec("echo hi > /noext")
    assert r.exit_code == 1
    assert "EINVAL" in r.stderr
