from __future__ import annotations

import fnmatch
import re
from dataclasses import dataclass, field
from typing import Callable, Awaitable

from ._errors import FsError
from ._parse import (
    Redirect,
    UnsupportedSyntaxError,
    parse_command,
    expand_word,
    expand_words,
    extract_redirects,
    extract_assignments,
)
from ._vendor.just_bash.ast.types import (
    SimpleCommandNode,
    PipelineNode,
    StatementNode,
    ScriptNode,
)
from ._volume import SupermemoryVolume


@dataclass
class ExecResult:
    stdout: str = ""
    stderr: str = ""
    exit_code: int = 0


class Shell:
    def __init__(
        self,
        volume: SupermemoryVolume,
        cwd: str = "/home/user",
        env: dict[str, str] | None = None,
    ) -> None:
        self.volume = volume
        self.cwd = cwd
        self.env: dict[str, str] = {"PATH": "", **(env or {})}
        self._last_exit = 0
        self._builtins: dict[str, Callable[..., Awaitable[ExecResult]]] = {
            "echo": self._cmd_echo,
            "printf": self._cmd_printf,
            "cat": self._cmd_cat,
            "head": self._cmd_head,
            "tail": self._cmd_tail,
            "ls": self._cmd_ls,
            "mkdir": self._cmd_mkdir,
            "rm": self._cmd_rm,
            "rmdir": self._cmd_rmdir,
            "mv": self._cmd_mv,
            "cp": self._cmd_cp,
            "touch": self._cmd_touch,
            "stat": self._cmd_stat,
            "pwd": self._cmd_pwd,
            "cd": self._cmd_cd,
            "test": self._cmd_test,
            "[": self._cmd_test,
            "grep": self._cmd_grep,
            "sgrep": self._cmd_sgrep,
            "wc": self._cmd_wc,
            "sort": self._cmd_sort,
            "uniq": self._cmd_uniq,
            "find": self._cmd_find,
            "sed": self._cmd_sed,
            "tee": self._cmd_tee,
            "basename": self._cmd_basename,
            "dirname": self._cmd_dirname,
            "true": self._cmd_true,
            "false": self._cmd_false,
            "cut": self._cmd_cut,
            "tr": self._cmd_tr,
            "seq": self._cmd_seq,
            "date": self._cmd_date,
        }

    def _resolve(self, path: str) -> str:
        if path.startswith("/"):
            return _normalize_path(path)
        return _normalize_path(f"{self.cwd}/{path}")

    def _resolve_dest_into_dir(self, src: str, dest: str) -> str:
        if (self.volume.path_index.is_directory(dest)
                and not self.volume.path_index.is_file(dest)):
            base = src.rstrip("/").rsplit("/", 1)[-1]
            if base:
                return (dest if dest.endswith("/") else dest + "/") + base
        return dest

    async def exec(self, command: str) -> ExecResult:
        self.env["?"] = str(self._last_exit)
        try:
            script = parse_command(command)
        except UnsupportedSyntaxError as e:
            return ExecResult(stderr=f"parse error: {e}\n", exit_code=2)
        try:
            result = await self._exec_script(script)
        except FsError as err:
            result = ExecResult(stdout="", stderr=f"bash: {err}\n", exit_code=1)
        self._last_exit = result.exit_code
        return result

    async def _exec_script(self, script: ScriptNode) -> ExecResult:
        result = ExecResult()
        for stmt in script.statements:
            r = await self._exec_statement(stmt)
            result = ExecResult(
                stdout=result.stdout + r.stdout,
                stderr=result.stderr + r.stderr,
                exit_code=r.exit_code,
            )
        return result

    async def _exec_statement(self, stmt: StatementNode) -> ExecResult:
        result = ExecResult()
        for i, pipeline in enumerate(stmt.pipelines):
            if i > 0:
                op = stmt.operators[i - 1]
                if op == "&&" and result.exit_code != 0:
                    continue
                if op == "||" and result.exit_code == 0:
                    continue
            r = await self._exec_pipeline(pipeline)
            result = ExecResult(
                stdout=result.stdout + r.stdout,
                stderr=result.stderr + r.stderr,
                exit_code=r.exit_code,
            )
        return result

    async def _exec_pipeline(self, pipeline: PipelineNode) -> ExecResult:
        if len(pipeline.commands) == 1:
            return await self._exec_command(pipeline.commands[0], stdin="")

        stdin = ""
        last_result = ExecResult()
        for i, cmd in enumerate(pipeline.commands):
            r = await self._exec_command(cmd, stdin=stdin)
            stdin = r.stdout
            last_result = ExecResult(
                stdout=r.stdout if i == len(pipeline.commands) - 1 else "",
                stderr=last_result.stderr + r.stderr,
                exit_code=r.exit_code,
            )
        last_result.stdout = stdin
        return last_result

    async def _exec_command(self, node: object, stdin: str = "") -> ExecResult:
        if not isinstance(node, SimpleCommandNode):
            return ExecResult(
                stderr=f"unsupported: {type(node).__name__} (only simple commands are supported)\n",
                exit_code=2,
            )

        assigns = extract_assignments(node.assignments, self.env)
        for k, v in assigns.items():
            self.env[k] = v

        if not node.name:
            return ExecResult()

        try:
            name = expand_word(node.name, self.env)
            args = expand_words(node.args, self.env)
            redirects = extract_redirects(node.redirections, self.env)
        except UnsupportedSyntaxError as e:
            return ExecResult(stderr=f"{e}\n", exit_code=2)

        for redir in redirects:
            if redir.content:
                stdin = redir.content
            elif redir.op == "<" and redir.path:
                path = self._resolve(redir.path)
                doc = await self.volume.get_doc(path)
                if not doc:
                    return ExecResult(stderr=f"{name}: {path}: No such file\n", exit_code=1)
                stdin = doc.content if isinstance(doc.content, str) else doc.content.decode()

        handler = self._builtins.get(name)
        if not handler:
            return ExecResult(stderr=f"{name}: command not found\n", exit_code=127)

        try:
            result = await handler(args, stdin)
        except FsError as e:
            return ExecResult(stderr=f"{name}: {e}\n", exit_code=1)
        except Exception as e:
            return ExecResult(stderr=f"{name}: {e}\n", exit_code=1)

        for redir in redirects:
            if redir.content or redir.op == "<":
                continue

            path = self._resolve(redir.path) if redir.path else None

            if path == "/dev/null":
                if redir.fd == 1:
                    result.stdout = ""
                elif redir.fd == 2:
                    result.stderr = ""
                continue

            if redir.op == ">":
                content = result.stdout if redir.fd == 1 else result.stderr
                if path and content:
                    await self.volume.add_doc(path, content)
                if redir.fd == 1:
                    result.stdout = ""
                else:
                    result.stderr = ""
            elif redir.op == ">>":
                content = result.stdout if redir.fd == 1 else result.stderr
                if path:
                    existing = await self.volume.get_doc(path)
                    prev = ""
                    if existing:
                        prev = existing.content if isinstance(existing.content, str) else existing.content.decode()
                    if prev or content:
                        await self.volume.add_doc(path, prev + content)
                if redir.fd == 1:
                    result.stdout = ""
                else:
                    result.stderr = ""

        return result

    # ------------------------------------------------------------------
    # Built-in commands
    # ------------------------------------------------------------------

    async def _cmd_echo(self, args: list[str], stdin: str) -> ExecResult:
        newline = True
        interpret = False
        words = []
        for a in args:
            if a == "-n":
                newline = False
            elif a == "-e":
                interpret = True
            else:
                words.append(a)
        text = " ".join(words)
        if interpret:
            text = text.replace("\\n", "\n").replace("\\t", "\t").replace("\\\\", "\\")
        return ExecResult(stdout=text + ("\n" if newline else ""))

    async def _cmd_printf(self, args: list[str], stdin: str) -> ExecResult:
        if not args:
            return ExecResult()
        fmt = args[0]
        out = fmt.replace("\\n", "\n").replace("\\t", "\t").replace("\\\\", "\\")
        for a in args[1:]:
            out = out.replace("%s", a, 1)
        return ExecResult(stdout=out)

    async def _cmd_cat(self, args: list[str], stdin: str) -> ExecResult:
        numbered = "-n" in args
        paths = [a for a in args if not a.startswith("-")]

        if not paths:
            if numbered:
                lines = stdin.split("\n")
                out = "\n".join(f"     {i+1}\t{l}" for i, l in enumerate(lines) if l or i < len(lines) - 1)
                return ExecResult(stdout=out + "\n" if out else "")
            return ExecResult(stdout=stdin)

        parts: list[str] = []
        for p in paths:
            path = self._resolve(p)
            doc = await self.volume.get_doc(path)
            if not doc:
                return ExecResult(stderr=f"cat: {p}: No such file or directory\n", exit_code=1)
            content = doc.content if isinstance(doc.content, str) else doc.content.decode()
            parts.append(content)

        text = "".join(parts)
        if numbered:
            lines = text.split("\n")
            text = "\n".join(f"     {i+1}\t{l}" for i, l in enumerate(lines))
        return ExecResult(stdout=text)

    async def _cmd_head(self, args: list[str], stdin: str) -> ExecResult:
        n = 10
        paths = []
        i = 0
        while i < len(args):
            if args[i] == "-n" and i + 1 < len(args):
                n = int(args[i + 1])
                i += 2
            elif args[i].startswith("-") and args[i][1:].isdigit():
                n = int(args[i][1:])
                i += 1
            elif not args[i].startswith("-"):
                paths.append(args[i])
                i += 1
            else:
                i += 1

        if not paths:
            lines = stdin.split("\n")
            return ExecResult(stdout="\n".join(lines[:n]) + ("\n" if lines[:n] else ""))

        parts = []
        for p in paths:
            path = self._resolve(p)
            doc = await self.volume.get_doc(path)
            if not doc:
                return ExecResult(stderr=f"head: {p}: No such file or directory\n", exit_code=1)
            content = doc.content if isinstance(doc.content, str) else doc.content.decode()
            lines = content.split("\n")
            parts.append("\n".join(lines[:n]))
        return ExecResult(stdout="\n".join(parts) + "\n")

    async def _cmd_tail(self, args: list[str], stdin: str) -> ExecResult:
        n = 10
        paths = []
        i = 0
        while i < len(args):
            if args[i] == "-n" and i + 1 < len(args):
                n = int(args[i + 1])
                i += 2
            elif args[i].startswith("-") and args[i][1:].isdigit():
                n = int(args[i][1:])
                i += 1
            elif not args[i].startswith("-"):
                paths.append(args[i])
                i += 1
            else:
                i += 1

        if not paths:
            lines = stdin.split("\n")
            taken = lines[-n:] if n <= len(lines) else lines
            return ExecResult(stdout="\n".join(taken) + ("\n" if taken else ""))

        parts = []
        for p in paths:
            path = self._resolve(p)
            doc = await self.volume.get_doc(path)
            if not doc:
                return ExecResult(stderr=f"tail: {p}: No such file or directory\n", exit_code=1)
            content = doc.content if isinstance(doc.content, str) else doc.content.decode()
            lines = content.split("\n")
            taken = lines[-n:] if n <= len(lines) else lines
            parts.append("\n".join(taken))
        return ExecResult(stdout="\n".join(parts) + "\n")

    async def _cmd_ls(self, args: list[str], stdin: str) -> ExecResult:
        long_format = "-l" in args or "-la" in args or "-al" in args
        show_all = "-a" in args or "-la" in args or "-al" in args
        paths = [a for a in args if not a.startswith("-")] or ["."]

        out_parts: list[str] = []
        for p in paths:
            path = self._resolve(p)
            prefix = path if path == "/" else f"{path}/"
            summaries = await self.volume.list_by_prefix(prefix)

            is_dir = self.volume.path_index.is_directory(path)
            if not summaries and not is_dir:
                stat = await self.volume.stat_doc(path)
                if stat and stat.is_file:
                    out_parts.append(p.rsplit("/", 1)[-1])
                    continue
                return ExecResult(stderr=f"ls: {p}: No such file or directory\n", exit_code=1)

            entries: dict[str, tuple[bool, int]] = {}
            for s in summaries:
                rest = s.filepath[len(prefix):]
                if not rest:
                    continue
                slash = rest.find("/")
                name = rest[:slash] if slash != -1 else rest
                is_file = slash == -1
                if name in entries:
                    if not is_file:
                        entries[name] = (False, 0)
                else:
                    entries[name] = (is_file, s.size)

            for d in self.volume.path_index.synthetic_dir_paths():
                if d.startswith(prefix):
                    rest = d[len(prefix):]
                    if rest and "/" not in rest and rest not in entries:
                        entries[rest] = (False, 0)

            names = sorted(entries.keys())
            if not show_all:
                names = [n for n in names if not n.startswith(".")]

            if long_format:
                lines = []
                for name in names:
                    is_file, size = entries[name]
                    mode = "-rw-r--r--" if is_file else "drwxr-xr-x"
                    lines.append(f"{mode}  1 user user {size:>8}  {name}")
                out_parts.append("\n".join(lines))
            else:
                out_parts.append("\n".join(names))

        return ExecResult(stdout="\n".join(out_parts) + "\n" if out_parts else "")

    async def _cmd_mkdir(self, args: list[str], stdin: str) -> ExecResult:
        recursive = "-p" in args
        paths = [a for a in args if not a.startswith("-")]
        for p in paths:
            path = self._resolve(p)
            if self.volume.path_index.is_file(path):
                return ExecResult(stderr=f"mkdir: {p}: Not a directory\n", exit_code=1)
            if self.volume.path_index.is_directory(path) and not recursive:
                return ExecResult(stderr=f"mkdir: {p}: File exists\n", exit_code=1)
            if recursive:
                segments = [s for s in path.split("/") if s]
                cur = ""
                for seg in segments:
                    cur += f"/{seg}"
                    self.volume.mark_synthetic_dir(cur)
            else:
                self.volume.mark_synthetic_dir(path)
        return ExecResult()

    async def _cmd_rm(self, args: list[str], stdin: str) -> ExecResult:
        recursive = any(f in args for f in ("-r", "-rf", "-fr", "-R"))
        force = any(f in args for f in ("-f", "-rf", "-fr"))
        paths = [a for a in args if not a.startswith("-")]
        for p in paths:
            path = self._resolve(p)
            is_dir = self.volume.path_index.is_directory(path) and not self.volume.path_index.is_file(path)
            if is_dir:
                if not recursive:
                    return ExecResult(stderr=f"rm: {p}: is a directory\n", exit_code=1)
                prefix = path if path.endswith("/") else f"{path}/"
                result = await self.volume.remove_by_prefix(prefix)
                if result.errors and not force:
                    return ExecResult(stderr="rm: failed to remove some files\n", exit_code=1)
            else:
                doc_id = self.volume.path_index.resolve(path)
                if not doc_id:
                    if force:
                        continue
                    return ExecResult(stderr=f"rm: {p}: No such file or directory\n", exit_code=1)
                await self.volume.remove_doc(path)
        return ExecResult()

    async def _cmd_rmdir(self, args: list[str], stdin: str) -> ExecResult:
        paths = [a for a in args if not a.startswith("-")]
        for p in paths:
            path = self._resolve(p)
            if self.volume.path_index.is_file(path):
                return ExecResult(stderr=f"rmdir: {p}: Not a directory\n", exit_code=1)
            if not self.volume.path_index.is_directory(path):
                return ExecResult(stderr=f"rmdir: {p}: No such file or directory\n", exit_code=1)
            if not await self.volume.is_dir_empty(path):
                return ExecResult(stderr=f"rmdir: {p}: Directory not empty\n", exit_code=1)
            self.volume.path_index.remove_synthetic_dir(path)
        return ExecResult()

    async def _cmd_mv(self, args: list[str], stdin: str) -> ExecResult:
        paths = [a for a in args if not a.startswith("-")]
        if len(paths) < 2:
            return ExecResult(stderr="mv: missing operand\n", exit_code=1)
        src, dest = self._resolve(paths[0]), self._resolve(paths[1])
        dest = self._resolve_dest_into_dir(src, dest)

        is_dir = self.volume.path_index.is_directory(src) and not self.volume.path_index.is_file(src)
        if is_dir:
            result = await self.volume.move_tree(src, dest)
            if result.errors:
                msg = f"mv: {paths[0]}: failed to move some files\n"
                return ExecResult(stderr=msg, exit_code=1)
        else:
            await self.volume.move_doc(src, dest)
        return ExecResult()

    async def _cmd_cp(self, args: list[str], stdin: str) -> ExecResult:
        recursive = any(f in args for f in ("-r", "-R", "-a"))
        paths = [a for a in args if not a.startswith("-")]
        if len(paths) < 2:
            return ExecResult(stderr="cp: missing operand\n", exit_code=1)
        src, dest = self._resolve(paths[0]), self._resolve(paths[1])
        dest = self._resolve_dest_into_dir(src, dest)

        is_dir = self.volume.path_index.is_directory(src) and not self.volume.path_index.is_file(src)
        if is_dir:
            if not recursive:
                return ExecResult(stderr=f"cp: -r not specified; omitting directory '{paths[0]}'\n", exit_code=1)
            result = await self.volume.copy_tree(src, dest)
            if result.errors:
                msg = f"cp: {paths[0]}: failed to copy some files\n"
                return ExecResult(stderr=msg, exit_code=1)
        else:
            doc = await self.volume.get_doc(src)
            if not doc:
                return ExecResult(stderr=f"cp: {paths[0]}: No such file or directory\n", exit_code=1)
            await self.volume.add_doc(dest, doc.content)
        return ExecResult()

    async def _cmd_touch(self, args: list[str], stdin: str) -> ExecResult:
        paths = [a for a in args if not a.startswith("-")]
        for p in paths:
            path = self._resolve(p)
            existing = await self.volume.get_doc(path)
            if not existing:
                await self.volume.add_doc(path, " ")
        return ExecResult()

    async def _cmd_stat(self, args: list[str], stdin: str) -> ExecResult:
        paths = [a for a in args if not a.startswith("-")]
        parts = []
        for p in paths:
            path = self._resolve(p)
            s = await self.volume.stat_doc(path)
            if not s:
                return ExecResult(stderr=f"stat: {p}: No such file or directory\n", exit_code=1)
            kind = "directory" if s.is_directory else "regular file"
            mode = "0755" if s.is_directory else "0644"
            parts.append(f"  File: {p}\n  Size: {s.size}\t{kind}\nAccess: ({mode})")
        return ExecResult(stdout="\n".join(parts) + "\n")

    async def _cmd_pwd(self, args: list[str], stdin: str) -> ExecResult:
        return ExecResult(stdout=self.cwd + "\n")

    async def _cmd_cd(self, args: list[str], stdin: str) -> ExecResult:
        target = args[0] if args else "/home/user"
        path = self._resolve(target)
        if not self.volume.path_index.is_directory(path):
            stat = await self.volume.stat_doc(path)
            if not stat or not stat.is_directory:
                return ExecResult(stderr=f"cd: {target}: No such file or directory\n", exit_code=1)
        self.cwd = path
        return ExecResult()

    async def _cmd_test(self, args: list[str], stdin: str) -> ExecResult:
        if args and args[-1] == "]":
            args = args[:-1]
        if not args:
            return ExecResult(exit_code=1)

        if len(args) == 1:
            return ExecResult(exit_code=0 if args[0] else 1)

        op = args[0]
        if op == "-f":
            path = self._resolve(args[1])
            stat = await self.volume.stat_doc(path)
            return ExecResult(exit_code=0 if stat and stat.is_file else 1)
        if op == "-d":
            path = self._resolve(args[1])
            return ExecResult(exit_code=0 if self.volume.path_index.is_directory(path) else 1)
        if op == "-e":
            path = self._resolve(args[1])
            stat = await self.volume.stat_doc(path)
            return ExecResult(exit_code=0 if stat else 1)
        if op == "-z":
            return ExecResult(exit_code=0 if not args[1] else 1)
        if op == "-n":
            return ExecResult(exit_code=0 if args[1] else 1)
        if op == "!":
            inner = await self._cmd_test(args[1:], stdin)
            return ExecResult(exit_code=0 if inner.exit_code != 0 else 1)

        if len(args) >= 3:
            lhs, op, rhs = args[0], args[1], args[2]
            if op == "=":
                return ExecResult(exit_code=0 if lhs == rhs else 1)
            if op == "!=":
                return ExecResult(exit_code=0 if lhs != rhs else 1)
            if op == "-eq":
                return ExecResult(exit_code=0 if int(lhs) == int(rhs) else 1)
            if op == "-ne":
                return ExecResult(exit_code=0 if int(lhs) != int(rhs) else 1)
            if op == "-gt":
                return ExecResult(exit_code=0 if int(lhs) > int(rhs) else 1)
            if op == "-lt":
                return ExecResult(exit_code=0 if int(lhs) < int(rhs) else 1)
            if op == "-ge":
                return ExecResult(exit_code=0 if int(lhs) >= int(rhs) else 1)
            if op == "-le":
                return ExecResult(exit_code=0 if int(lhs) <= int(rhs) else 1)

        return ExecResult(exit_code=2, stderr="test: unrecognized condition\n")

    async def _cmd_grep(self, args: list[str], stdin: str) -> ExecResult:
        ignore_case = "-i" in args
        show_numbers = "-n" in args
        count_only = "-c" in args
        files_only = "-l" in args
        invert = "-v" in args
        recursive = "-r" in args or "-R" in args
        positional = [a for a in args if not a.startswith("-")]

        if not positional:
            return ExecResult(stderr="grep: missing pattern\n", exit_code=2)

        pattern = positional[0]
        targets = positional[1:]
        flags = re.IGNORECASE if ignore_case else 0
        try:
            regex = re.compile(pattern, flags)
        except re.error:
            regex = re.compile(re.escape(pattern), flags)

        async def grep_content(content: str, label: str | None) -> tuple[list[str], int]:
            lines = content.split("\n")
            matches = []
            count = 0
            for i, line in enumerate(lines):
                found = bool(regex.search(line))
                if invert:
                    found = not found
                if found:
                    count += 1
                    prefix = f"{label}:" if label else ""
                    num = f"{i+1}:" if show_numbers else ""
                    matches.append(f"{prefix}{num}{line}")
            return matches, count

        if not targets:
            matches, count = await grep_content(stdin, None)
            if count_only:
                return ExecResult(stdout=f"{count}\n", exit_code=0 if count else 1)
            return ExecResult(
                stdout="\n".join(matches) + ("\n" if matches else ""),
                exit_code=0 if matches else 1,
            )

        all_matches: list[str] = []
        total_count = 0
        multi = len(targets) > 1 or recursive

        for t in targets:
            path = self._resolve(t)
            if recursive and self.volume.path_index.is_directory(path):
                prefix = path if path.endswith("/") else f"{path}/"
                summaries = await self.volume.list_by_prefix(prefix, with_content=True)
                for s in summaries:
                    if s.content is None:
                        continue
                    label = s.filepath if multi else None
                    ms, c = await grep_content(s.content, label)
                    if files_only and c > 0:
                        all_matches.append(s.filepath)
                    elif count_only:
                        total_count += c
                    else:
                        all_matches.extend(ms)
            else:
                doc = await self.volume.get_doc(path)
                if not doc:
                    return ExecResult(stderr=f"grep: {t}: No such file or directory\n", exit_code=2)
                content = doc.content if isinstance(doc.content, str) else doc.content.decode()
                label = path if multi else None
                ms, c = await grep_content(content, label)
                if files_only and c > 0:
                    all_matches.append(path)
                elif count_only:
                    total_count += c
                else:
                    all_matches.extend(ms)

        if count_only:
            return ExecResult(stdout=f"{total_count}\n", exit_code=0 if total_count else 1)
        return ExecResult(
            stdout="\n".join(all_matches) + ("\n" if all_matches else ""),
            exit_code=0 if all_matches else 1,
        )

    async def _cmd_sgrep(self, args: list[str], stdin: str) -> ExecResult:
        if "--help" in args or "-h" in args:
            return ExecResult(stdout=(
                "Usage: sgrep QUERY [PATH]\n"
                "       sgrep [-p PATH] QUERY\n"
                "  Semantic search across the Supermemory container.\n"
            ))

        filepath = None
        positional = []
        i = 0
        while i < len(args):
            if args[i] == "-p" and i + 1 < len(args):
                filepath = args[i + 1]
                i += 2
            elif args[i].startswith("-"):
                return ExecResult(stderr=f"sgrep: unknown flag '{args[i]}'\n", exit_code=2)
            else:
                positional.append(args[i])
                i += 1

        if not positional:
            return ExecResult(stderr="sgrep: missing QUERY (try --help)\n", exit_code=2)

        if filepath is None and len(positional) >= 2 and positional[-1].startswith("/"):
            filepath = positional.pop()

        query = " ".join(positional)
        resp = await self.volume.search(query, filepath=filepath)

        if not resp.results:
            return ExecResult()

        lines = []
        for r in resp.results:
            fp = r.filepath or "(unknown)"
            content = r.memory or r.chunk or ""
            if not content:
                continue
            escaped = content.replace("\\", "\\\\").replace("\r", "\\r").replace("\n", "\\n")
            lines.append(f"{fp}:{escaped}")
        return ExecResult(stdout="\n\n".join(lines) + "\n" if lines else "")

    async def _cmd_wc(self, args: list[str], stdin: str) -> ExecResult:
        lines_only = "-l" in args
        words_only = "-w" in args
        bytes_only = "-c" in args
        paths = [a for a in args if not a.startswith("-")]

        def count(text: str) -> tuple[int, int, int]:
            ls = text.count("\n")
            ws = len(text.split())
            bs = len(text.encode())
            return ls, ws, bs

        if not paths:
            ls, ws, bs = count(stdin)
            if lines_only:
                return ExecResult(stdout=f"{ls}\n")
            if words_only:
                return ExecResult(stdout=f"{ws}\n")
            if bytes_only:
                return ExecResult(stdout=f"{bs}\n")
            return ExecResult(stdout=f"  {ls}  {ws}  {bs}\n")

        parts = []
        for p in paths:
            path = self._resolve(p)
            doc = await self.volume.get_doc(path)
            if not doc:
                return ExecResult(stderr=f"wc: {p}: No such file or directory\n", exit_code=1)
            text = doc.content if isinstance(doc.content, str) else doc.content.decode()
            ls, ws, bs = count(text)
            if lines_only:
                parts.append(f"  {ls} {p}")
            elif words_only:
                parts.append(f"  {ws} {p}")
            elif bytes_only:
                parts.append(f"  {bs} {p}")
            else:
                parts.append(f"  {ls}  {ws}  {bs} {p}")
        return ExecResult(stdout="\n".join(parts) + "\n")

    async def _cmd_sort(self, args: list[str], stdin: str) -> ExecResult:
        reverse = "-r" in args
        numeric = "-n" in args
        unique = "-u" in args
        paths = [a for a in args if not a.startswith("-")]

        text = stdin
        if paths:
            parts = []
            for p in paths:
                path = self._resolve(p)
                doc = await self.volume.get_doc(path)
                if not doc:
                    return ExecResult(stderr=f"sort: {p}: No such file or directory\n", exit_code=1)
                parts.append(doc.content if isinstance(doc.content, str) else doc.content.decode())
            text = "".join(parts)

        lines = text.split("\n")
        if lines and lines[-1] == "":
            lines.pop()
        if numeric:
            def key_fn(s: str) -> float:
                m = re.match(r"[-+]?\d*\.?\d+", s)
                return float(m.group()) if m else 0
            lines.sort(key=key_fn, reverse=reverse)
        else:
            lines.sort(reverse=reverse)
        if unique:
            seen: set[str] = set()
            deduped = []
            for l in lines:
                if l not in seen:
                    seen.add(l)
                    deduped.append(l)
            lines = deduped
        return ExecResult(stdout="\n".join(lines) + "\n" if lines else "")

    async def _cmd_uniq(self, args: list[str], stdin: str) -> ExecResult:
        count_mode = "-c" in args
        lines = stdin.split("\n")
        if lines and lines[-1] == "":
            lines.pop()
        result = []
        prev = None
        cnt = 0
        for line in lines:
            if line == prev:
                cnt += 1
            else:
                if prev is not None:
                    result.append((cnt, prev))
                prev = line
                cnt = 1
        if prev is not None:
            result.append((cnt, prev))
        if count_mode:
            out = "\n".join(f"  {c} {l}" for c, l in result)
        else:
            out = "\n".join(l for _, l in result)
        return ExecResult(stdout=out + "\n" if out else "")

    async def _cmd_find(self, args: list[str], stdin: str) -> ExecResult:
        base = "."
        name_pattern = None
        type_filter = None
        i = 0
        while i < len(args):
            if args[i] == "-name" and i + 1 < len(args):
                name_pattern = args[i + 1]
                i += 2
            elif args[i] == "-type" and i + 1 < len(args):
                type_filter = args[i + 1]
                i += 2
            elif not args[i].startswith("-"):
                base = args[i]
                i += 1
            else:
                i += 1

        base_path = self._resolve(base)
        prefix = base_path if base_path.endswith("/") else f"{base_path}/"
        summaries = await self.volume.list_by_prefix(prefix)

        results: list[str] = [base_path]
        seen_dirs: set[str] = set()
        for s in summaries:
            rest = s.filepath[len(prefix):]
            parts = rest.split("/")
            cur = base_path
            for part in parts[:-1]:
                cur += f"/{part}"
                if cur not in seen_dirs:
                    seen_dirs.add(cur)
                    if type_filter != "f":
                        name = cur.rsplit("/", 1)[-1]
                        if name_pattern is None or fnmatch.fnmatch(name, name_pattern):
                            results.append(cur)
            if type_filter != "d":
                name = s.filepath.rsplit("/", 1)[-1]
                if name_pattern is None or fnmatch.fnmatch(name, name_pattern):
                    results.append(s.filepath)

        return ExecResult(stdout="\n".join(results) + "\n")

    async def _cmd_sed(self, args: list[str], stdin: str) -> ExecResult:
        in_place = "-i" in args
        positional = [a for a in args if a != "-i"]
        if not positional:
            return ExecResult(stderr="sed: missing expression\n", exit_code=1)

        expr = positional[0]
        files = positional[1:]

        m = re.match(r"s(.)(.+?)\1(.*?)\1(g?)", expr)
        if not m:
            return ExecResult(stderr=f"sed: unsupported expression: {expr}\n", exit_code=1)

        pattern = m.group(2)
        replacement = m.group(3)
        global_flag = bool(m.group(4))

        def apply_sed(text: str) -> str:
            if global_flag:
                return re.sub(pattern, replacement, text)
            return re.sub(pattern, replacement, text, count=1)

        if not files:
            lines = stdin.split("\n")
            out = "\n".join(apply_sed(l) for l in lines)
            return ExecResult(stdout=out)

        parts = []
        for f in files:
            path = self._resolve(f)
            doc = await self.volume.get_doc(path)
            if not doc:
                return ExecResult(stderr=f"sed: {f}: No such file or directory\n", exit_code=1)
            content = doc.content if isinstance(doc.content, str) else doc.content.decode()
            result = "\n".join(apply_sed(l) for l in content.split("\n"))
            if in_place:
                await self.volume.add_doc(path, result)
            else:
                parts.append(result)
        return ExecResult(stdout="".join(parts) if not in_place else "")

    async def _cmd_tee(self, args: list[str], stdin: str) -> ExecResult:
        append = "-a" in args
        paths = [a for a in args if not a.startswith("-")]
        for p in paths:
            path = self._resolve(p)
            if append:
                existing = await self.volume.get_doc(path)
                prev = ""
                if existing:
                    prev = existing.content if isinstance(existing.content, str) else existing.content.decode()
                await self.volume.add_doc(path, prev + stdin)
            else:
                await self.volume.add_doc(path, stdin)
        return ExecResult(stdout=stdin)

    async def _cmd_basename(self, args: list[str], stdin: str) -> ExecResult:
        if not args:
            return ExecResult(stderr="basename: missing operand\n", exit_code=1)
        name = args[0].rstrip("/").rsplit("/", 1)[-1]
        if len(args) > 1:
            suffix = args[1]
            if name.endswith(suffix):
                name = name[: -len(suffix)]
        return ExecResult(stdout=name + "\n")

    async def _cmd_dirname(self, args: list[str], stdin: str) -> ExecResult:
        if not args:
            return ExecResult(stderr="dirname: missing operand\n", exit_code=1)
        path = args[0].rstrip("/")
        idx = path.rfind("/")
        return ExecResult(stdout=(path[:idx] if idx > 0 else ("/" if path.startswith("/") else ".")) + "\n")

    async def _cmd_true(self, args: list[str], stdin: str) -> ExecResult:
        return ExecResult(exit_code=0)

    async def _cmd_false(self, args: list[str], stdin: str) -> ExecResult:
        return ExecResult(exit_code=1)

    async def _cmd_cut(self, args: list[str], stdin: str) -> ExecResult:
        delimiter = "\t"
        fields_str = ""
        i = 0
        while i < len(args):
            if args[i] == "-d" and i + 1 < len(args):
                delimiter = args[i + 1]
                i += 2
            elif args[i].startswith("-d"):
                delimiter = args[i][2:]
                i += 1
            elif args[i] == "-f" and i + 1 < len(args):
                fields_str = args[i + 1]
                i += 2
            elif args[i].startswith("-f"):
                fields_str = args[i][2:]
                i += 1
            else:
                i += 1

        if not fields_str:
            return ExecResult(stderr="cut: missing field list\n", exit_code=1)

        field_indices: list[int] = []
        for part in fields_str.split(","):
            if "-" in part:
                lo, hi = part.split("-", 1)
                lo_i = int(lo) if lo else 1
                hi_i = int(hi) if hi else 999
                field_indices.extend(range(lo_i, hi_i + 1))
            else:
                field_indices.append(int(part))

        lines = stdin.split("\n")
        result = []
        for line in lines:
            parts = line.split(delimiter)
            selected = [parts[f - 1] for f in field_indices if 0 < f <= len(parts)]
            result.append(delimiter.join(selected))
        return ExecResult(stdout="\n".join(result))

    async def _cmd_tr(self, args: list[str], stdin: str) -> ExecResult:
        delete = "-d" in args
        positional = [a for a in args if not a.startswith("-")]
        if not positional:
            return ExecResult(stderr="tr: missing operand\n", exit_code=1)

        set1 = positional[0]
        if delete:
            out = "".join(c for c in stdin if c not in set1)
            return ExecResult(stdout=out)

        if len(positional) < 2:
            return ExecResult(stderr="tr: missing operand after SET1\n", exit_code=1)
        set2 = positional[1]
        table = str.maketrans(set1, set2[:len(set1)].ljust(len(set1), set2[-1] if set2 else " "))
        return ExecResult(stdout=stdin.translate(table))

    async def _cmd_seq(self, args: list[str], stdin: str) -> ExecResult:
        nums = [int(a) for a in args if not a.startswith("-")]
        if len(nums) == 1:
            result = list(range(1, nums[0] + 1))
        elif len(nums) == 2:
            result = list(range(nums[0], nums[1] + 1))
        elif len(nums) == 3:
            result = list(range(nums[0], nums[2] + 1, nums[1]))
        else:
            return ExecResult(stderr="seq: invalid arguments\n", exit_code=1)
        return ExecResult(stdout="\n".join(str(n) for n in result) + "\n")

    async def _cmd_date(self, args: list[str], stdin: str) -> ExecResult:
        from datetime import datetime, timezone
        return ExecResult(stdout=datetime.now(timezone.utc).strftime("%a %b %d %H:%M:%S UTC %Y") + "\n")


def _normalize_path(path: str) -> str:
    if not path or path == "/":
        return "/"
    segments = path.split("/")
    stack: list[str] = []
    for seg in segments:
        if seg in ("", "."):
            continue
        if seg == "..":
            if stack:
                stack.pop()
        else:
            stack.append(seg)
    return "/" + "/".join(stack)
