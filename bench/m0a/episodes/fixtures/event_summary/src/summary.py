from collections.abc import Iterable

from src.events import parse_event


def summarize_events(lines: Iterable[str]) -> dict[str, object]:
    """Summarize parsed event records."""
    raise NotImplementedError
