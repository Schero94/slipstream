def stable_unique(values):
    result = []
    seen = set()
    for value in values:
        if value in seen:
            result.append(value)
        seen.add(value)
    return result
