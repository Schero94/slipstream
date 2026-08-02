"""OpenAI ``tool_choice`` enforcement overlay for Slipstream's oMLX fork."""

from __future__ import annotations

from copy import deepcopy
from typing import Any


def _tool_dict(tool: Any) -> dict[str, Any]:
    if isinstance(tool, dict):
        return tool
    dump = getattr(tool, "model_dump", None)
    if callable(dump):
        return dump(exclude_none=True)
    raise ValueError(f"unsupported tool definition: {type(tool).__name__}")


def _tool_name(tool: Any) -> str:
    function = _tool_dict(tool).get("function") or {}
    return str(function.get("name") or "")


def _specific_name(tool_choice: Any) -> str | None:
    if not isinstance(tool_choice, dict):
        return None
    if tool_choice.get("type") != "function":
        raise ValueError("tool_choice.type must be 'function'")
    function = tool_choice.get("function")
    if not isinstance(function, dict) or not str(function.get("name") or "").strip():
        raise ValueError("tool_choice.function.name is required")
    return str(function["name"]).strip()


def _inject_instruction(messages: list[Any], instruction: str) -> list[Any]:
    result = deepcopy(messages)
    if result and isinstance(result[0], dict) and result[0].get("role") == "system":
        existing = str(result[0].get("content") or "")
        result[0]["content"] = f"{existing}\n\n{instruction}" if existing else instruction
    else:
        result.insert(0, {"role": "system", "content": instruction})
    return result


def enforce_tool_choice(
    messages: list[Any],
    tool_choice: Any,
    tools: list[Any] | None,
) -> tuple[list[Any], list[Any] | None]:
    """Translate required/specific OpenAI tool choice into the model prompt.

    oMLX already passes tool schemas into the chat template, but upstream only
    interpreted ``tool_choice='none'``.  Qwen therefore saw required/specific
    requests as ``auto``.  This overlay makes the choice explicit at the same
    prompt boundary and hides non-selected tools for a specific-function call.
    """
    if tool_choice in (None, "auto", "none"):
        return deepcopy(messages), deepcopy(tools)
    if not tools:
        raise ValueError("tool_choice requires at least one tool definition")

    if tool_choice == "required":
        instruction = (
            "Tool choice is mandatory. You MUST call one available function. "
            "Do not answer with text. Emit exactly one valid tool call."
        )
        return _inject_instruction(messages, instruction), deepcopy(tools)

    name = _specific_name(tool_choice)
    if name is None:
        raise ValueError("tool_choice must be 'auto', 'none', 'required', or a function object")
    selected = [tool for tool in tools if _tool_name(tool) == name]
    if not selected:
        raise ValueError(f"tool_choice function {name!r} is not present in tools")
    instruction = (
        f"Tool choice is mandatory. You MUST call the `{name}` function. "
        "Do not answer with text. Emit exactly one valid tool call."
    )
    return _inject_instruction(messages, instruction), deepcopy(selected)
