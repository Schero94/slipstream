def chunked(iterable, size):
    """Yield lists containing at most size values."""
    iterator = iter(iterable)
    while True:
        chunk = []
        for _ in range(size):
            try:
                chunk.append(next(iterator))
                next(iterator, None)
            except StopIteration:
                break
        if not chunk:
            return
        yield chunk
