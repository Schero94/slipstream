def backoff_delays(attempts, base=1, cap=60):
    """Return capped exponential retry delays."""
    return [base * (2 ** index) for index in range(attempts)]
