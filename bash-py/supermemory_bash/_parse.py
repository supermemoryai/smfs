"""Bridge between just-bash-py's AST and our shell execution layer."""
from __future__ import annotations

from collections.abc import Awaitable, Callable
from dataclasses import dataclass

from ._vendor.just_bash import ParseException
from ._vendor.just_bash import parse as _jb_parse
from ._vendor.just_bash.ast.types import (
    ArithAssignmentNode,
    ArithBinaryNode,
    ArithExpr,
    ArithGroupNode,
    ArithmeticExpansionPart,
    ArithNestedNode,
    ArithNumberNode,
    ArithTernaryNode,
    ArithUnaryNode,
    ArithVariableNode,
    AssignDefaultOp,
    AssignmentNode,
    CommandSubstitutionPart,
    DefaultValueOp,
    DoubleQuotedPart,
    ErrorIfUnsetOp,
    EscapedPart,
    GlobPart,
    HereDocNode,
    LiteralPart,
    ParameterExpansionPart,
    RedirectionNode,
    ScriptNode,
    SingleQuotedPart,
    TildeExpansionPart,
    UseAlternativeOp,
    WordNode,
)


class UnsupportedSyntaxError(Exception):
    pass


@dataclass
class Redirect:
    op: str  # ">" | ">>" | "<"
    path: str | None = None
    fd: int = 1
    content: str = ""


CommandSubstitutionRunner = Callable[[ScriptNode], Awaitable[str]]


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


async def expand_word_async(
    word: WordNode,
    env: dict[str, str],
    command_runner: CommandSubstitutionRunner,
) -> str:
    parts: list[str] = []
    for p in word.parts:
        parts.append(await _expand_part_async(p, env, command_runner))
    return "".join(parts)


async def _expand_part_async(
    part: object,
    env: dict[str, str],
    command_runner: CommandSubstitutionRunner,
) -> str:
    if isinstance(part, LiteralPart):
        return part.value

    if isinstance(part, SingleQuotedPart):
        return part.value

    if isinstance(part, DoubleQuotedPart):
        expanded = [await _expand_part_async(p, env, command_runner) for p in part.parts]
        return "".join(expanded)

    if isinstance(part, EscapedPart):
        return part.value

    if isinstance(part, ParameterExpansionPart):
        return await _expand_parameter_async(part, env, command_runner)

    if isinstance(part, TildeExpansionPart):
        return env.get("HOME", "/home/user")

    if isinstance(part, GlobPart):
        return part.pattern

    if isinstance(part, CommandSubstitutionPart):
        if part.body is None:
            return ""
        # Bash removes trailing newlines from command substitution output.
        return (await command_runner(part.body)).rstrip("\n")

    if isinstance(part, ArithmeticExpansionPart):
        return str(eval_arithmetic_expansion(part, env))

    return ""


async def _expand_parameter_async(
    part: ParameterExpansionPart,
    env: dict[str, str],
    command_runner: CommandSubstitutionRunner,
) -> str:
    name = part.parameter
    if name == "?":
        return env.get("?", "0")
    val = env.get(name)
    op = part.operation
    if op is None:
        return val if val is not None else ""
    if isinstance(op, DefaultValueOp):
        default = await expand_word_async(op.word, env, command_runner) if op.word else ""
        if op.check_empty:
            return val if val else default
        return val if val is not None else default
    if isinstance(op, UseAlternativeOp):
        alt = await expand_word_async(op.word, env, command_runner) if op.word else ""
        if op.check_empty:
            return alt if val else ""
        return alt if val is not None else ""
    if isinstance(op, AssignDefaultOp):
        default = await expand_word_async(op.word, env, command_runner) if op.word else ""
        if (op.check_empty and not val) or (not op.check_empty and val is None):
            env[name] = default
            return default
        return val or ""
    if isinstance(op, ErrorIfUnsetOp):
        if (op.check_empty and not val) or (not op.check_empty and val is None):
            msg = (
                await expand_word_async(op.word, env, command_runner)
                if op.word
                else f"{name}: parameter null or not set"
            )
            raise UnsupportedSyntaxError(msg)
        return val or ""
    return val if val is not None else ""


def _word_has_unquoted_command_substitution(word: WordNode) -> bool:
    return any(isinstance(part, CommandSubstitutionPart) for part in word.parts)


async def expand_words_async(
    words: tuple[WordNode, ...],
    env: dict[str, str],
    command_runner: CommandSubstitutionRunner,
) -> list[str]:
    out: list[str] = []
    for word in words:
        expanded = await expand_word_async(word, env, command_runner)
        if _word_has_unquoted_command_substitution(word):
            out.extend(expanded.split())
        else:
            out.append(expanded)
    return out


async def extract_redirects_async(
    redirections: tuple[RedirectionNode, ...],
    env: dict[str, str],
    command_runner: CommandSubstitutionRunner,
) -> list[Redirect]:
    out: list[Redirect] = []
    for r in redirections:
        op = r.operator

        if isinstance(r.target, HereDocNode):
            content = (
                await expand_word_async(r.target.content, env, command_runner)
                if r.target.content
                else ""
            )
            out.append(Redirect(op="<", fd=0, content=content))
            continue

        target_path = (
            await expand_word_async(r.target, env, command_runner)
            if isinstance(r.target, WordNode)
            else str(r.target)
        )

        if op in (">", ">|"):
            fd = r.fd if r.fd is not None else 1
            out.append(Redirect(op=">", path=target_path, fd=fd))
        elif op == ">>":
            fd = r.fd if r.fd is not None else 1
            out.append(Redirect(op=">>", path=target_path, fd=fd))
        elif op == "<":
            out.append(Redirect(op="<", path=target_path, fd=0))
        elif op == "<<" or op == "<<-":
            out.append(Redirect(op="<", fd=0, content=""))
        elif op == "&>" or op == "&>>":
            redir_op = ">>" if op == "&>>" else ">"
            out.append(Redirect(op=redir_op, path=target_path, fd=1))
            out.append(Redirect(op=redir_op, path=target_path, fd=2))
        elif op == ">&" or op == "<&":
            pass
        else:
            out.append(Redirect(op=">", path=target_path, fd=r.fd or 1))

    return out


async def extract_assignments_async(
    assignments: tuple[AssignmentNode, ...],
    env: dict[str, str],
    command_runner: CommandSubstitutionRunner,
) -> dict[str, str]:
    out: dict[str, str] = {}
    for a in assignments:
        if a.value:
            out[a.name] = await expand_word_async(a.value, env, command_runner)
        else:
            out[a.name] = ""
    return out


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

        target_path = (
            expand_word(r.target, env)
            if isinstance(r.target, WordNode)
            else str(r.target)
        )

        if op in (">", ">|"):
            fd = r.fd if r.fd is not None else 1
            out.append(Redirect(op=">", path=target_path, fd=fd))
        elif op == ">>":
            fd = r.fd if r.fd is not None else 1
            out.append(Redirect(op=">>", path=target_path, fd=fd))
        elif op == "<":
            out.append(Redirect(op="<", path=target_path, fd=0))
        elif op == "<<" or op == "<<-":
            out.append(Redirect(op="<", fd=0, content=""))
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


def eval_arithmetic_expansion(part: ArithmeticExpansionPart, env: dict[str, str]) -> int:
    expr = part.expression.expression if part.expression else None
    return _eval_arithmetic(expr, env)


def _env_int(env: dict[str, str], name: str) -> int:
    try:
        return int(env.get(name, "0") or "0", 0)
    except ValueError:
        return 0


def _trunc_div(lhs: int, rhs: int) -> int:
    if rhs == 0:
        raise UnsupportedSyntaxError("division by zero")
    return int(lhs / rhs)


def _eval_arithmetic(expr: ArithExpr | None, env: dict[str, str]) -> int:
    if expr is None:
        return 0

    if isinstance(expr, ArithNumberNode):
        return expr.value

    if isinstance(expr, ArithVariableNode):
        return _env_int(env, expr.name)

    if isinstance(expr, ArithGroupNode):
        return _eval_arithmetic(expr.expression, env)

    if isinstance(expr, ArithNestedNode):
        return _eval_arithmetic(expr.expression, env)

    if isinstance(expr, ArithTernaryNode):
        branch = expr.consequent if _eval_arithmetic(expr.condition, env) != 0 else expr.alternate
        return _eval_arithmetic(branch, env)

    if isinstance(expr, ArithUnaryNode):
        return _eval_arithmetic_unary(expr, env)

    if isinstance(expr, ArithBinaryNode):
        return _eval_arithmetic_binary(expr, env)

    if isinstance(expr, ArithAssignmentNode):
        return _eval_arithmetic_assignment(expr, env)

    raise UnsupportedSyntaxError(f"unsupported arithmetic expression: {type(expr).__name__}")


def _eval_arithmetic_unary(expr: ArithUnaryNode, env: dict[str, str]) -> int:
    if isinstance(expr.operand, ArithVariableNode) and expr.operator in ("++", "--"):
        old = _env_int(env, expr.operand.name)
        new = old + 1 if expr.operator == "++" else old - 1
        env[expr.operand.name] = str(new)
        return new if expr.prefix else old

    value = _eval_arithmetic(expr.operand, env)
    if expr.operator == "-":
        return -value
    if expr.operator == "+":
        return value
    if expr.operator == "!":
        return 0 if value else 1
    if expr.operator == "~":
        return ~value
    raise UnsupportedSyntaxError(f"unsupported arithmetic operator: {expr.operator}")


def _eval_arithmetic_binary(expr: ArithBinaryNode, env: dict[str, str]) -> int:
    op = expr.operator

    if op == "&&":
        return (
            1
            if _eval_arithmetic(expr.left, env) != 0
            and _eval_arithmetic(expr.right, env) != 0
            else 0
        )
    if op == "||":
        return (
            1
            if _eval_arithmetic(expr.left, env) != 0
            or _eval_arithmetic(expr.right, env) != 0
            else 0
        )

    lhs = _eval_arithmetic(expr.left, env)
    rhs = _eval_arithmetic(expr.right, env)

    if op == "+":
        return lhs + rhs
    if op == "-":
        return lhs - rhs
    if op == "*":
        return lhs * rhs
    if op == "/":
        return _trunc_div(lhs, rhs)
    if op == "%":
        if rhs == 0:
            raise UnsupportedSyntaxError("division by zero")
        return lhs % rhs
    if op == "**":
        return int(pow(lhs, rhs))
    if op == "<<":
        return lhs << rhs
    if op == ">>":
        return lhs >> rhs
    if op == "<":
        return 1 if lhs < rhs else 0
    if op == "<=":
        return 1 if lhs <= rhs else 0
    if op == ">":
        return 1 if lhs > rhs else 0
    if op == ">=":
        return 1 if lhs >= rhs else 0
    if op == "==":
        return 1 if lhs == rhs else 0
    if op == "!=":
        return 1 if lhs != rhs else 0
    if op == "&":
        return lhs & rhs
    if op == "|":
        return lhs | rhs
    if op == "^":
        return lhs ^ rhs
    if op == ",":
        return rhs

    raise UnsupportedSyntaxError(f"unsupported arithmetic operator: {op}")


def _eval_arithmetic_assignment(expr: ArithAssignmentNode, env: dict[str, str]) -> int:
    if expr.subscript is not None:
        raise UnsupportedSyntaxError("arithmetic array assignment is not supported")

    current = _env_int(env, expr.variable)
    value = _eval_arithmetic(expr.value, env)

    if expr.operator == "=":
        result = value
    elif expr.operator == "+=":
        result = current + value
    elif expr.operator == "-=":
        result = current - value
    elif expr.operator == "*=":
        result = current * value
    elif expr.operator == "/=":
        result = _trunc_div(current, value)
    elif expr.operator == "%=":
        if value == 0:
            raise UnsupportedSyntaxError("division by zero")
        result = current % value
    elif expr.operator == "<<=":
        result = current << value
    elif expr.operator == ">>=":
        result = current >> value
    elif expr.operator == "&=":
        result = current & value
    elif expr.operator == "|=":
        result = current | value
    elif expr.operator == "^=":
        result = current ^ value
    else:
        raise UnsupportedSyntaxError(f"unsupported arithmetic operator: {expr.operator}")

    env[expr.variable] = str(result)
    return result
