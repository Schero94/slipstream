def parse_duration(text):
    """Parse SS, MM:SS, or HH:MM:SS into seconds."""
    return sum(int(part) for part in text.split(":"))
