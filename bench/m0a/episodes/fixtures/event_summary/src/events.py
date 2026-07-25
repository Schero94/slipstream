LEVELS = {"info", "warning", "error"}


def parse_event(line: str) -> tuple[str, str, str]:
    """Parse LEVEL|COMPONENT|MESSAGE into a validated tuple."""
    parts = [part.strip() for part in line.split("|", 2)]
    if len(parts) != 3 or parts[0] not in LEVELS or not parts[1] or not parts[2]:
        raise ValueError(f"invalid event: {line!r}")
    return parts[0], parts[1], parts[2]
