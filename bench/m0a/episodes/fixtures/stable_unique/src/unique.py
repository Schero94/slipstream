def unique_by(items, key):
    """Return first items for distinct keys."""
    return list({key(item): item for item in items}.values())
