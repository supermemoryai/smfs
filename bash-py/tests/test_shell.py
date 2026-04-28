"""Unit tests for the shell interpreter, using a mock volume."""
from __future__ import annotations

import pytest
from unittest.mock import MagicMock
from supermemory_bash._shell import Shell, ExecResult, _normalize_path
from supermemory_bash._volume import DocResult, DocSummary, DocStat, SearchResp, SearchResult
from supermemory_bash._path_index import PathIndex
from supermemory_bash._session_cache import SessionCache


class FakeVolume:
    """In-memory volume for testing the shell without HTTP."""

    def __init__(self) -> None:
        self.path_index = PathIndex()
        self.cache = SessionCache(ttl_ms=None)
        self._files: dict[str, str] = {}
        for d in ["/home", "/home/user", "/tmp", "/dev"]:
            self.path_index.mark_synthetic_dir(d)

    def mark_synthetic_dir(self, path: str) -> None:
        self.path_index.mark_synthetic_dir(path)

    async def add_doc(self, path: str, content: str | bytes) -> tuple[str, str]:
        if isinstance(content, bytes):
            content = content.decode()
        self._files[path] = content
        doc_id = f"doc-{hash(path) % 10000}"
        self.path_index.insert(path, doc_id)
        self.cache.set(path, content, "done")
        return doc_id, "done"

    async def get_doc(self, path: str) -> DocResult | None:
        if path not in self._files:
            return None
        return DocResult(
            id=f"doc-{hash(path) % 10000}",
            content=self._files[path],
            status="done",
        )

    async def remove_doc(self, path: str) -> None:
        self._files.pop(path, None)
        self.path_index.remove(path)
        self.cache.delete(path)

    async def remove_by_prefix(self, prefix: str) -> MagicMock:
        to_remove = [p for p in self._files if p.startswith(prefix)]
        for p in to_remove:
            del self._files[p]
            self.path_index.remove(p)
            self.cache.delete(p)
        dir_self = prefix[:-1] if prefix.endswith("/") else prefix
        for d in list(self.path_index.synthetic_dir_paths()):
            if d == dir_self or d.startswith(prefix):
                self.path_index.remove_synthetic_dir(d)
        result = MagicMock()
        result.deleted = len(to_remove)
        result.errors = []
        return result

    async def is_dir_empty(self, path: str) -> bool:
        prefix = path if path == "/" else f"{path}/"
        for p in self._files:
            if p.startswith(prefix):
                return False
        for d in self.path_index.synthetic_dir_paths():
            if d != path and d.startswith(prefix):
                return False
        return True

    async def move_tree(self, src: str, dest: str) -> MagicMock:
        src_prefix = src if src.endswith("/") else f"{src}/"
        dest_prefix = dest if dest.endswith("/") else f"{dest}/"
        for p in [p for p in list(self._files) if p.startswith(src_prefix)]:
            await self.move_doc(p, dest_prefix + p[len(src_prefix):])
        for d in list(self.path_index.synthetic_dir_paths()):
            if d == src:
                self.path_index.remove_synthetic_dir(d)
            elif d.startswith(src_prefix):
                self.path_index.remove_synthetic_dir(d)
                self.path_index.mark_synthetic_dir(dest_prefix + d[len(src_prefix):])
        self.path_index.mark_synthetic_dir(dest)
        result = MagicMock()
        result.errors = []
        return result

    async def copy_tree(self, src: str, dest: str) -> MagicMock:
        src_prefix = src if src.endswith("/") else f"{src}/"
        dest_prefix = dest if dest.endswith("/") else f"{dest}/"
        for p in [p for p in list(self._files) if p.startswith(src_prefix)]:
            await self.add_doc(dest_prefix + p[len(src_prefix):], self._files[p])
        for d in self.path_index.synthetic_dir_paths():
            if d.startswith(src_prefix):
                self.path_index.mark_synthetic_dir(dest_prefix + d[len(src_prefix):])
        self.path_index.mark_synthetic_dir(dest)
        result = MagicMock()
        result.errors = []
        return result

    async def move_doc(self, from_path: str, to_path: str) -> None:
        content = self._files.pop(from_path, "")
        self._files[to_path] = content
        doc_id = self.path_index.resolve(from_path)
        self.path_index.remove(from_path)
        self.path_index.insert(to_path, doc_id or "moved")
        self.cache.delete(from_path)
        self.cache.set(to_path, content, "done")

    async def stat_doc(self, path: str) -> DocStat | None:
        if self.path_index.is_directory(path) and not self.path_index.is_file(path):
            return DocStat(is_file=False, is_directory=True, size=0, mtime=0.0)
        if path in self._files:
            return DocStat(
                is_file=True, is_directory=False,
                size=len(self._files[path]), mtime=0.0,
                id=f"doc-{hash(path) % 10000}", status="done",
            )
        return None

    async def list_by_prefix(
        self, prefix: str, *, with_content: bool = False, exact: bool = False, limit: int | None = None,
    ) -> list[DocSummary]:
        out = []
        for p, content in sorted(self._files.items()):
            if exact:
                if p != prefix:
                    continue
            else:
                if not p.startswith(prefix):
                    continue
            out.append(DocSummary(
                id=f"doc-{hash(p) % 10000}",
                filepath=p,
                status="done",
                size=len(content),
                mtime=0.0,
                content=content if with_content else None,
            ))
            if limit and len(out) >= limit:
                break
        return out

    async def search(self, q: str, filepath: str | None = None) -> SearchResp:
        results = []
        for p, content in self._files.items():
            if filepath and not p.startswith(filepath):
                continue
            if q.lower() in content.lower():
                results.append(SearchResult(id="r1", filepath=p, memory=content, similarity=0.9))
        return SearchResp(results=results)

    def cached_all_paths(self) -> list[str]:
        return sorted(self._files.keys())

    def synthetic_dir_paths(self) -> list[str]:
        return self.path_index.synthetic_dir_paths()


def make_shell() -> tuple[Shell, FakeVolume]:
    vol = FakeVolume()
    shell = Shell(vol, cwd="/home/user")  # type: ignore[arg-type]
    return shell, vol


# --- Path normalization ---

def test_normalize_path():
    assert _normalize_path("/a/b/../c") == "/a/c"
    assert _normalize_path("/a/./b") == "/a/b"
    assert _normalize_path("///a//b") == "/a/b"
    assert _normalize_path("/") == "/"


# --- Shell command tests ---

@pytest.fixture
def shell_and_vol():
    return make_shell()


@pytest.mark.asyncio
async def test_echo(shell_and_vol):
    shell, vol = shell_and_vol
    r = await shell.exec("echo hello world")
    assert r.stdout == "hello world\n"
    assert r.exit_code == 0


@pytest.mark.asyncio
async def test_echo_n(shell_and_vol):
    shell, vol = shell_and_vol
    r = await shell.exec("echo -n hello")
    assert r.stdout == "hello"


@pytest.mark.asyncio
async def test_write_and_cat(shell_and_vol):
    shell, vol = shell_and_vol
    r = await shell.exec("echo 'hello world' > /a.md")
    assert r.exit_code == 0
    r = await shell.exec("cat /a.md")
    assert r.stdout == "hello world\n"


@pytest.mark.asyncio
async def test_append_redirect(shell_and_vol):
    shell, vol = shell_and_vol
    await shell.exec("echo line1 > /b.txt")
    await shell.exec("echo line2 >> /b.txt")
    r = await shell.exec("cat /b.txt")
    assert "line1" in r.stdout
    assert "line2" in r.stdout


@pytest.mark.asyncio
async def test_pipe(shell_and_vol):
    shell, vol = shell_and_vol
    await vol.add_doc("/test.txt", "line1\nline2\nline3\nline4\nline5\n")
    r = await shell.exec("cat /test.txt | head -n 2")
    lines = r.stdout.strip().split("\n")
    assert len(lines) == 2
    assert lines[0] == "line1"


@pytest.mark.asyncio
async def test_and_chain_success(shell_and_vol):
    shell, vol = shell_and_vol
    r = await shell.exec("echo first && echo second")
    assert "first" in r.stdout
    assert "second" in r.stdout


@pytest.mark.asyncio
async def test_and_chain_failure(shell_and_vol):
    shell, vol = shell_and_vol
    r = await shell.exec("cat /nonexistent && echo should_not_appear")
    assert "should_not_appear" not in r.stdout
    assert r.exit_code != 0


@pytest.mark.asyncio
async def test_or_chain(shell_and_vol):
    shell, vol = shell_and_vol
    r = await shell.exec("cat /nonexistent || echo fallback")
    assert "fallback" in r.stdout


@pytest.mark.asyncio
async def test_pwd(shell_and_vol):
    shell, vol = shell_and_vol
    r = await shell.exec("pwd")
    assert r.stdout.strip() == "/home/user"


@pytest.mark.asyncio
async def test_cd_and_pwd(shell_and_vol):
    shell, vol = shell_and_vol
    vol.mark_synthetic_dir("/notes")
    await shell.exec("cd /notes")
    r = await shell.exec("pwd")
    assert r.stdout.strip() == "/notes"


@pytest.mark.asyncio
async def test_mkdir(shell_and_vol):
    shell, vol = shell_and_vol
    await shell.exec("mkdir /mydir")
    r = await shell.exec("test -d /mydir")
    assert r.exit_code == 0


@pytest.mark.asyncio
async def test_mkdir_p(shell_and_vol):
    shell, vol = shell_and_vol
    await shell.exec("mkdir -p /a/b/c")
    r = await shell.exec("test -d /a/b/c")
    assert r.exit_code == 0


@pytest.mark.asyncio
async def test_rm_file(shell_and_vol):
    shell, vol = shell_and_vol
    await vol.add_doc("/to_delete.txt", "content")
    r = await shell.exec("rm /to_delete.txt")
    assert r.exit_code == 0
    r = await shell.exec("cat /to_delete.txt")
    assert r.exit_code != 0


@pytest.mark.asyncio
async def test_mv(shell_and_vol):
    shell, vol = shell_and_vol
    await vol.add_doc("/old.txt", "content")
    await shell.exec("mv /old.txt /new.txt")
    r = await shell.exec("cat /new.txt")
    assert r.stdout == "content"
    r = await shell.exec("cat /old.txt")
    assert r.exit_code != 0


@pytest.mark.asyncio
async def test_cp(shell_and_vol):
    shell, vol = shell_and_vol
    await vol.add_doc("/src.txt", "original")
    await shell.exec("cp /src.txt /dst.txt")
    r = await shell.exec("cat /dst.txt")
    assert r.stdout == "original"
    r = await shell.exec("cat /src.txt")
    assert r.stdout == "original"


@pytest.mark.asyncio
async def test_head(shell_and_vol):
    shell, vol = shell_and_vol
    content = "\n".join(f"line{i}" for i in range(20))
    await vol.add_doc("/many.txt", content)
    r = await shell.exec("head -n 3 /many.txt")
    lines = r.stdout.strip().split("\n")
    assert len(lines) == 3


@pytest.mark.asyncio
async def test_tail(shell_and_vol):
    shell, vol = shell_and_vol
    content = "\n".join(f"line{i}" for i in range(20))
    await vol.add_doc("/many.txt", content)
    r = await shell.exec("tail -n 3 /many.txt")
    lines = r.stdout.strip().split("\n")
    assert len(lines) == 3
    assert "line19" in lines[-1]


@pytest.mark.asyncio
async def test_grep(shell_and_vol):
    shell, vol = shell_and_vol
    await vol.add_doc("/data.txt", "apple\nbanana\napricot\ncherry\n")
    r = await shell.exec("grep ap /data.txt")
    assert "apple" in r.stdout
    assert "apricot" in r.stdout
    assert "cherry" not in r.stdout


@pytest.mark.asyncio
async def test_grep_case_insensitive(shell_and_vol):
    shell, vol = shell_and_vol
    await vol.add_doc("/data.txt", "Apple\nBANANA\n")
    r = await shell.exec("grep -i apple /data.txt")
    assert "Apple" in r.stdout


@pytest.mark.asyncio
async def test_wc(shell_and_vol):
    shell, vol = shell_and_vol
    await vol.add_doc("/wc.txt", "one two three\nfour five\n")
    r = await shell.exec("wc -l /wc.txt")
    assert "2" in r.stdout


@pytest.mark.asyncio
async def test_sort(shell_and_vol):
    shell, vol = shell_and_vol
    r = await shell.exec("echo -e 'c\na\nb' | sort")
    lines = r.stdout.strip().split("\n")
    assert lines == ["a", "b", "c"]


@pytest.mark.asyncio
async def test_test_file(shell_and_vol):
    shell, vol = shell_and_vol
    await vol.add_doc("/exists.txt", "hi")
    r = await shell.exec("test -f /exists.txt")
    assert r.exit_code == 0
    r = await shell.exec("test -f /nope.txt")
    assert r.exit_code != 0


@pytest.mark.asyncio
async def test_test_dir(shell_and_vol):
    shell, vol = shell_and_vol
    r = await shell.exec("test -d /home")
    assert r.exit_code == 0
    r = await shell.exec("test -d /nonexistent")
    assert r.exit_code != 0


@pytest.mark.asyncio
async def test_basename(shell_and_vol):
    shell, vol = shell_and_vol
    r = await shell.exec("basename /home/user/file.txt")
    assert r.stdout.strip() == "file.txt"


@pytest.mark.asyncio
async def test_dirname(shell_and_vol):
    shell, vol = shell_and_vol
    r = await shell.exec("dirname /home/user/file.txt")
    assert r.stdout.strip() == "/home/user"


@pytest.mark.asyncio
async def test_seq(shell_and_vol):
    shell, vol = shell_and_vol
    r = await shell.exec("seq 5")
    assert r.stdout.strip() == "1\n2\n3\n4\n5"


@pytest.mark.asyncio
async def test_variable_assignment(shell_and_vol):
    shell, vol = shell_and_vol
    await shell.exec("FOO=hello")
    r = await shell.exec("echo $FOO")
    assert r.stdout.strip() == "hello"


@pytest.mark.asyncio
async def test_touch(shell_and_vol):
    shell, vol = shell_and_vol
    await shell.exec("touch /newfile.txt")
    r = await shell.exec("test -f /newfile.txt")
    assert r.exit_code == 0


@pytest.mark.asyncio
async def test_command_not_found(shell_and_vol):
    shell, vol = shell_and_vol
    r = await shell.exec("nonexistent_cmd")
    assert r.exit_code == 127
    assert "command not found" in r.stderr


@pytest.mark.asyncio
async def test_sgrep(shell_and_vol):
    shell, vol = shell_and_vol
    await vol.add_doc("/notes/auth.md", "authentication tokens and OAuth flow")
    r = await shell.exec("sgrep 'authentication tokens'")
    assert r.exit_code == 0
    assert "auth.md" in r.stdout


@pytest.mark.asyncio
async def test_sed_substitute(shell_and_vol):
    shell, vol = shell_and_vol
    r = await shell.exec("echo 'hello world' | sed 's/world/earth/'")
    assert "earth" in r.stdout


@pytest.mark.asyncio
async def test_tee(shell_and_vol):
    shell, vol = shell_and_vol
    r = await shell.exec("echo 'hello' | tee /tee_out.txt")
    assert "hello" in r.stdout
    r2 = await shell.exec("cat /tee_out.txt")
    assert "hello" in r2.stdout


@pytest.mark.asyncio
async def test_ls(shell_and_vol):
    shell, vol = shell_and_vol
    await vol.add_doc("/mydir/a.txt", "a")
    await vol.add_doc("/mydir/b.txt", "b")
    r = await shell.exec("ls /mydir")
    assert "a.txt" in r.stdout
    assert "b.txt" in r.stdout


# --- Regression tests: fd redirects, /dev/null, heredocs ---

@pytest.mark.asyncio
async def test_stderr_redirect_to_dev_null(shell_and_vol):
    shell, vol = shell_and_vol
    r = await shell.exec("ls /nonexistent 2>/dev/null")
    assert r.stderr == ""
    assert r.exit_code != 0


@pytest.mark.asyncio
async def test_stdout_redirect_to_dev_null(shell_and_vol):
    shell, vol = shell_and_vol
    await vol.add_doc("/test.txt", "content")
    r = await shell.exec("cat /test.txt > /dev/null")
    assert r.stdout == ""


@pytest.mark.asyncio
async def test_fd_redirect_not_passed_as_arg(shell_and_vol):
    """2>/dev/null should not include '2' in the command output."""
    shell, vol = shell_and_vol
    r = await shell.exec("echo hello 2>/dev/null")
    assert r.stdout.strip() == "hello"
    assert "2" not in r.stdout


@pytest.mark.asyncio
async def test_combined_redirects(shell_and_vol):
    shell, vol = shell_and_vol
    r = await shell.exec("echo hello > /out.txt 2>/dev/null")
    doc = await vol.get_doc("/out.txt")
    assert doc is not None
    assert "hello" in doc.content


@pytest.mark.asyncio
async def test_heredoc(shell_and_vol):
    shell, vol = shell_and_vol
    r = await shell.exec("cat <<EOF\nhello world\nEOF")
    assert "hello world" in r.stdout


@pytest.mark.asyncio
async def test_ls_with_stderr_suppressed(shell_and_vol):
    """The original bug: ls /path 2>/dev/null | head -50"""
    shell, vol = shell_and_vol
    await vol.add_doc("/mydir/file.txt", "content")
    r = await shell.exec("ls /mydir 2>/dev/null | head -50")
    assert "file.txt" in r.stdout
    assert r.stderr == ""


@pytest.mark.asyncio
async def test_dev_null_no_api_write(shell_and_vol):
    """Redirecting to /dev/null should NOT create a file."""
    shell, vol = shell_and_vol
    await shell.exec("echo test > /dev/null")
    doc = await vol.get_doc("/dev/null")
    assert doc is None


# ── FsError exec wrapper ──────────────────────────────────────────────────


class _ThrowingVolume(FakeVolume):
    """FakeVolume whose add_doc raises an FsError, simulating Volume's
    assert_writable rejecting a write."""

    def __init__(self, error_factory):
        super().__init__()
        self._error_factory = error_factory

    async def add_doc(self, path: str, content):  # type: ignore[override]
        raise self._error_factory(path)


@pytest.mark.asyncio
async def test_fs_error_caught_and_returned_as_exit_1():
    from supermemory_bash._errors import eperm

    vol = _ThrowingVolume(lambda p: eperm(p, "addDoc"))
    shell = Shell(vol, cwd="/")
    r = await shell.exec("echo hi > /profile.md")
    assert r.exit_code == 1, r
    assert "EPERM" in r.stderr, r.stderr
    assert "bash:" in r.stderr, r.stderr


@pytest.mark.asyncio
async def test_fs_error_einval_caught_and_returned_as_exit_1():
    from supermemory_bash._errors import einval

    vol = _ThrowingVolume(lambda p: einval(f"'{p}': missing_extension"))
    shell = Shell(vol, cwd="/")
    r = await shell.exec("echo hi > /noext")
    assert r.exit_code == 1, r
    assert "EINVAL" in r.stderr, r.stderr


@pytest.mark.asyncio
async def test_input_redirect_feeds_stdin_to_command(shell_and_vol):
    shell, vol = shell_and_vol
    await vol.add_doc("/in.txt", "alpha\nbeta\ngamma\n")
    r = await shell.exec("wc -l < /in.txt")
    assert r.exit_code == 0, r
    assert r.stdout.strip() == "3", r.stdout


@pytest.mark.asyncio
async def test_mv_into_existing_directory_uses_basename(shell_and_vol):
    shell, vol = shell_and_vol
    await vol.add_doc("/dst/keep.md", "anchor")
    await vol.add_doc("/a.txt", "hello")
    r = await shell.exec("mv /a.txt /dst/")
    assert r.exit_code == 0, r
    assert vol.path_index.is_file("/dst/a.txt"), vol.path_index.paths()
    assert vol.path_index.is_file("/dst/keep.md"), vol.path_index.paths()
    assert not vol.path_index.is_file("/dst")


@pytest.mark.asyncio
async def test_cp_into_existing_directory_uses_basename(shell_and_vol):
    shell, vol = shell_and_vol
    await vol.add_doc("/dst/keep.md", "anchor")
    await vol.add_doc("/a.txt", "hello")
    r = await shell.exec("cp /a.txt /dst/")
    assert r.exit_code == 0, r
    assert vol.path_index.is_file("/dst/a.txt"), vol.path_index.paths()
    assert vol.path_index.is_file("/a.txt"), vol.path_index.paths()
    assert vol.path_index.is_file("/dst/keep.md"), vol.path_index.paths()
    assert not vol.path_index.is_file("/dst")


@pytest.mark.asyncio
async def test_grep_c_counts_each_match_once(shell_and_vol):
    shell, vol = shell_and_vol
    await vol.add_doc("/file.txt", "apple\napple\nbanana\n")
    r = await shell.exec("grep -c apple /file.txt")
    assert r.exit_code == 0, r
    assert r.stdout.strip() == "2", r.stdout


# --- Nested-synthetic-dir bugs (from v6 PR review) ---

@pytest.mark.asyncio
async def test_rm_r_evicts_nested_synthetic_dirs(shell_and_vol):
    shell, vol = shell_and_vol
    await shell.exec("mkdir -p /a/b/c")
    r = await shell.exec("rm -r /a")
    assert r.exit_code == 0, r
    assert not vol.path_index.is_directory("/a"), vol.path_index.synthetic_dir_paths()
    assert not vol.path_index.is_directory("/a/b"), vol.path_index.synthetic_dir_paths()
    assert not vol.path_index.is_directory("/a/b/c"), vol.path_index.synthetic_dir_paths()


@pytest.mark.asyncio
async def test_rmdir_refuses_dir_with_only_synthetic_children(shell_and_vol):
    shell, vol = shell_and_vol
    await shell.exec("mkdir -p /a/b")
    r = await shell.exec("rmdir /a")
    assert r.exit_code == 1, r
    assert "Directory not empty" in r.stderr, r.stderr
    assert vol.path_index.is_directory("/a"), vol.path_index.synthetic_dir_paths()
    assert vol.path_index.is_directory("/a/b"), vol.path_index.synthetic_dir_paths()


@pytest.mark.asyncio
async def test_mv_dir_migrates_synthetic_subdirs(shell_and_vol):
    shell, vol = shell_and_vol
    await shell.exec("mkdir -p /src/empty")
    await shell.exec("echo hello > /src/file.md")
    r = await shell.exec("mv /src /dst")
    assert r.exit_code == 0, r
    assert vol.path_index.is_directory("/dst/empty"), vol.path_index.synthetic_dir_paths()
    assert not vol.path_index.is_directory("/src/empty"), vol.path_index.synthetic_dir_paths()


@pytest.mark.asyncio
async def test_cp_r_replicates_synthetic_subdirs(shell_and_vol):
    shell, vol = shell_and_vol
    await shell.exec("mkdir -p /src/empty")
    await shell.exec("echo hello > /src/file.md")
    r = await shell.exec("cp -r /src /dst")
    assert r.exit_code == 0, r
    assert vol.path_index.is_directory("/dst/empty"), vol.path_index.synthetic_dir_paths()
    assert vol.path_index.is_directory("/src/empty"), vol.path_index.synthetic_dir_paths()
