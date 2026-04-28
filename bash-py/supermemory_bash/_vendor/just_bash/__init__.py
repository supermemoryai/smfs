"""Vendored subset of just-bash-py (parser + AST only). Apache-2.0 licensed."""

from .parser import Parser, parse, ParseException
from .ast.types import ScriptNode

__all__ = ["Parser", "parse", "ParseException", "ScriptNode"]
