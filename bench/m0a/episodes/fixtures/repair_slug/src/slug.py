import re


def slugify(text: str) -> str:
    """Return a URL-safe slug."""
    return re.sub(r"[^a-z0-9]+", "-", text.lower())
