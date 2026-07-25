from src.policy import delay_for_attempt

def retry_plan(statuses):
    """Return delays required before a success response."""
    return [delay_for_attempt(index) for index, _ in enumerate(statuses)]
