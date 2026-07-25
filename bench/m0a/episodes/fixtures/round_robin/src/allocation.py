def allocate(items, workers):
    """Allocate items across workers."""
    return {worker: list(items) for worker in workers}
