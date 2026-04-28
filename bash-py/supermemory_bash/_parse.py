"""Bridge between just-bash-py's AST and our shell execution layer."""
from __future__ import annotations

from dataclasses import dataclass, field

from ._vendor.just_bash import parse as _jb_parse, ParseException
from ._vendor.just_bash.ast.types import (
    ScriptNode,
    StatementNode,
    PipelineNode,
    SimpleCommandNode,
    WordNode,
    RedirectionNode,
    AssignmentNode,
    HereDocNode,
    LiteralPart,
    SingleQuotedPart,
    DoubleQuotedPart,
    EscapedPart,
    ParameterExpansionPart,
    TildeExpansionPart,
    DefaultValueOp,
    UseAlternativeOp,
    AssignDefaultOp,
    ErrorIfUnsetOp,
    CommandSubstitutionPart,
    ArithmeticExpansionPart,
    GlobPart,
)


class UnsupportedSyntaxError(Exception):
    pass


@dataclass
class Redirect:
    op: str  # ">" | ">>" | "<"
    path: str | None = None
    fd: int = 1
    content: str = ""


def parse_command(cmd: str) -> ScriptNode:
    try:
        return _jb_parse(cmd)
    except ParseException as e:
        raise UnsupportedSyntaxError(str(e)) from e


def expand_word(word: WordNode, env: dict[str, str]) -> str:
    parts: list[str] = []
    for p in word.parts:
        parts.append(_expand_part(p, env))
    return "".join(parts)


def _expand_part(part: object, env: dict[str, str]) -> str:
    if isinstance(part, LiteralPart):
        return part.value

    if isinstance(part, SingleQuotedPart):
        return part.value

    if isinstance(part, DoubleQuotedPart):
        return "".join(_expand_part(p, env) for p in part.parts)

    if isinstance(part, EscapedPart):
        return part.value

    if isinstance(part, ParameterExpansionPart):
        name = part.parameter
        if name == "?":
            return env.get("?", "0")
        val = env.get(name)
        op = part.operation
        if op is None:
            return val if val is not None else ""
        if isinstance(op, DefaultValueOp):
            default = expand_word(op.word, env) if op.word else ""
            if op.check_empty:
                return val if val else default
            return val if val is not None else default
        if isinstance(op, UseAlternativeOp):
            alt = expand_word(op.word, env) if op.word else ""
            if op.check_empty:
                return alt if val else ""
            return alt if val is not None else ""
        if isinstance(op, AssignDefaultOp):
            default = expand_word(op.word, env) if op.word else ""
            if (op.check_empty and not val) or (not op.check_empty and val is None):
                env[name] = default
                return default
            return val or ""
        if isinstance(op, ErrorIfUnsetOp):
            if (op.check_empty and not val) or (not op.check_empty and val is None):
                msg = expand_word(op.word, env) if op.word else f"{name}: parameter null or not set"
                raise UnsupportedSyntaxError(msg)
            return val or ""
        return val if val is not None else ""

    if isinstance(part, TildeExpansionPart):
        return env.get("HOME", "/home/user")

    if isinstance(part, GlobPart):
        return part.pattern

    if isinstance(part, CommandSubstitutionPart):
        raise UnsupportedSyntaxError("command substitution ($(...) or `...`) is not supported")

    if isinstance(part, ArithmeticExpansionPart):
        raise UnsupportedSyntaxError("arithmetic expansion ($((...))) is not supported")

    # Unknown part — return empty rather than crash
    return ""


def expand_words(words: tuple[WordNode, ...], env: dict[str, str]) -> list[str]:
    return [expand_word(w, env) for w in words]


def extract_redirects(
    redirections: tuple[RedirectionNode, ...],
    env: dict[str, str],
) -> list[Redirect]:
    out: list[Redirect] = []
    for r in redirections:
        op = r.operator

        if isinstance(r.target, HereDocNode):
            content = expand_word(r.target.content, env) if r.target.content else ""
            out.append(Redirect(op="<", fd=0, content=content))
            continue

        target_path = expand_word(r.target, env) if isinstance(r.target, WordNode) else str(r.target)

        if op in (">", ">|"):
            fd = r.fd if r.fd is not None else 1
            out.append(Redirect(op=">", path=target_path, fd=fd))
        elif op == ">>":
            fd = r.fd if r.fd is not None else 1
            out.append(Redirect(op=">>", path=target_path, fd=fd))
        elif op == "<":
            out.append(Redirect(op="<", path=target_path, fd=0))
        elif op == "<<" or op == "<<-":
            content = expand_word(r.target.content, env) if hasattr(r.target, "content") and r.target.content else ""
            out.append(Redirect(op="<", fd=0, content=content))
        elif op == "&>" or op == "&>>":
            # Redirect both stdout and stderr
            append = op == "&>>"
            redir_op = ">>" if append else ">"
            out.append(Redirect(op=redir_op, path=target_path, fd=1))
            out.append(Redirect(op=redir_op, path=target_path, fd=2))
        elif op == ">&" or op == "<&":
            # fd duplication — for "2>&1" just treat stderr same as stdout
            pass
        else:
            out.append(Redirect(op=">", path=target_path, fd=r.fd or 1))

    return out


def extract_assignments(
    assignments: tuple[AssignmentNode, ...],
    env: dict[str, str],
) -> dict[str, str]:
    out: dict[str, str] = {}
    for a in assignments:
        if a.value:
            out[a.name] = expand_word(a.value, env)
        else:
            out[a.name] = ""
    return out
