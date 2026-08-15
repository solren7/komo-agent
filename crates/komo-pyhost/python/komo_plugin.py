"""The SDK a komo plugin is written against.

A plugin is a `.py` file under `~/.komo/plugins/` that decorates functions with
`@tool`. The host imports the file, collects what the decorators registered, and
tells komo about it; komo mounts them into its tool catalog, and the model can
call them like any built-in.

    from komo_plugin import tool

    @tool("Return the length of a string.")
    def strlen(text: str) -> int:
        return len(text)

Parameter names and annotations become the JSON schema the model is shown, so
the signature is the contract: annotate the arguments and write a docstring or
pass a description. Everything here is stdlib-only on purpose — a plugin that
needs a third-party package installs it into the interpreter komo runs, and a
plugin that needs none must not have to.
"""

import inspect
import json
from typing import Any, Callable, Dict, List, Optional

__all__ = ["tool", "hook", "registered_tools", "registered_hooks", "clear"]

# Name → registration. Ordered by insertion, but komo sorts by name before the
# model ever sees it, so definition order carries no meaning.
_REGISTRY: "Dict[str, _Registration]" = {}


class _Registration:
    def __init__(self, fn: Callable[..., Any], description: str, schema: Dict[str, Any]):
        self.fn = fn
        self.description = description
        self.schema = schema


def tool(description: Optional[str] = None, *, name: Optional[str] = None):
    """Register a function as a tool komo can call.

    `description` is what the model reads to decide whether to call it; the
    docstring is used when it is omitted. `name` overrides the function name.

    A duplicate name raises: two tools answering to one name would make which
    code runs depend on file load order.
    """

    def decorate(fn: Callable[..., Any]) -> Callable[..., Any]:
        tool_name = name or fn.__name__
        text = description or (inspect.getdoc(fn) or "").strip()
        if not text:
            raise ValueError(
                f"tool `{tool_name}` needs a description (pass one, or give the "
                f"function a docstring) — it is what the model reads to decide "
                f"whether to call it"
            )
        if tool_name in _REGISTRY:
            raise ValueError(
                f"tool `{tool_name}` is already registered; two tools cannot "
                f"share a name"
            )
        _REGISTRY[tool_name] = _Registration(fn, text, _schema_of(fn))
        return fn

    # Bare `@tool` (no parentheses) hands the function straight in.
    if callable(description):
        fn, description = description, None
        return decorate(fn)
    return decorate


# Hook name → the functions registered for it, in registration order.
_HOOKS: "Dict[str, List[Callable[..., Any]]]" = {}

# What a `pre_step` hook may return, beyond `None`/a string.
PRE_STEP = "pre_step"
_HOOK_POINTS = (PRE_STEP,)


def hook(point: str):
    """Register a function to run at one of komo's hook points.

    The only point today is `pre_step`, called between the rounds of a turn —
    after tools have run and before their results go back to the model. Return
    a string to put text in front of the model for that round, `None` to do
    nothing, or `stop("...")` to end the turn with an answer:

        from komo_plugin import hook, stop

        @hook("pre_step")
        def remind(session_id, round):
            if round >= 5:
                return "You have taken several rounds; wrap up."

    Injected text reaches the model the way a user's mid-turn message does —
    appended, never rewriting what came before — so it costs the provider's
    prompt cache nothing. There is deliberately no way to rewrite the history
    or the system prompt from here.
    """
    if point not in _HOOK_POINTS:
        raise ValueError(
            f"unknown hook point `{point}`; known points: {', '.join(_HOOK_POINTS)}"
        )

    def decorate(fn: Callable[..., Any]) -> Callable[..., Any]:
        _HOOKS.setdefault(point, []).append(fn)
        return fn

    return decorate


class Stop(str):
    """A hook's answer that ends the turn. Build one with `stop(...)`."""


def stop(reply: str) -> Stop:
    """End the turn with `reply` instead of driving another round."""
    return Stop(reply)


def registered_hooks() -> List[str]:
    """Which hook points have at least one function.

    komo asks so it can skip calling a host that registered none: a hook point
    nobody uses must cost a running turn nothing at all.
    """
    return sorted(point for point, fns in _HOOKS.items() if fns)


def run_hook(point: str, payload: Dict[str, Any], warn: Optional[Callable[[str], None]] = None):
    """Run every function registered at `point` and combine their answers.

    Injections accumulate in registration order; the first `stop` wins and the
    functions after it do not run — the same shape komo's own hook ladder has.

    A function that raises is skipped, and reported through `warn`, rather than
    taking the round down with it: a hook runs on every round of every turn, so
    a broken one must cost its own effect and nothing more. A function that
    returns the wrong *type* is the same kind of mistake and is treated the
    same way.
    """
    injected: List[str] = []
    for fn in _HOOKS.get(point, []):
        name = getattr(fn, "__name__", point)
        try:
            answer = fn(**payload)
            if answer is None:
                continue
            if isinstance(answer, Stop):
                return {"stop": str(answer)}
            if not isinstance(answer, str):
                raise TypeError(
                    f"returned {type(answer).__name__}; return a string, None, "
                    f"or stop(...)"
                )
            if answer.strip():
                injected.append(answer)
        except Exception as error:  # noqa: BLE001 - reported, never fatal
            if warn is not None:
                warn(f"hook `{name}` failed and was skipped: {error}")
    return {"inject": "\n".join(injected)} if injected else {}


def registered_tools() -> List[Dict[str, Any]]:
    """The manifest komo is told about: name, description, and JSON schema."""
    return [
        {
            "name": name,
            "description": reg.description,
            "parameters": reg.schema,
        }
        for name, reg in _REGISTRY.items()
    ]


def clear() -> None:
    """Forget every registration — the host calls this before a reload."""
    _REGISTRY.clear()
    _HOOKS.clear()


def call(name: str, args: Dict[str, Any]) -> str:
    """Run a registered tool and render its result as the model-facing text.

    A `str` is returned as-is; anything else is JSON-encoded, so a tool can
    return a dict or a list without every plugin re-implementing formatting.
    """
    reg = _REGISTRY.get(name)
    if reg is None:
        raise KeyError(f"no tool named `{name}`")
    result = reg.fn(**args)
    if inspect.isawaitable(result):
        raise TypeError(
            f"tool `{name}` returned an awaitable; komo plugins are synchronous "
            f"(run your own event loop inside the function if you need one)"
        )
    if isinstance(result, str):
        return result
    if result is None:
        return "(no output)"
    return json.dumps(result, ensure_ascii=False, default=str)


# Annotation → JSON-schema type. Anything else is left untyped rather than
# guessed at: an over-specific schema makes the model's correct call invalid.
_TYPES = {
    str: "string",
    int: "integer",
    float: "number",
    bool: "boolean",
    list: "array",
    dict: "object",
}


def _schema_of(fn: Callable[..., Any]) -> Dict[str, Any]:
    """Derive the model-facing argument schema from the signature.

    A parameter with a default is optional; everything else is required. `*args`
    and `**kwargs` are skipped — the model calls with named arguments, so a
    catch-all it cannot address does not belong in the schema.
    """
    properties: Dict[str, Any] = {}
    required: List[str] = []
    for param in inspect.signature(fn).parameters.values():
        if param.kind in (param.VAR_POSITIONAL, param.VAR_KEYWORD):
            continue
        prop: Dict[str, Any] = {}
        json_type = _TYPES.get(param.annotation)
        if json_type:
            prop["type"] = json_type
        properties[param.name] = prop
        if param.default is inspect.Parameter.empty:
            required.append(param.name)
    return {"type": "object", "properties": properties, "required": required}
