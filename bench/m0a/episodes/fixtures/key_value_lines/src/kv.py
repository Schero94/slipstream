def parse_lines(lines):
    """Parse KEY=VALUE lines."""
    return dict(line.split("=") for line in lines)
