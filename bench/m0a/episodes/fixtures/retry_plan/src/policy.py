SUCCESS = {200, 201, 204}
RETRYABLE = {408, 429, 500, 502, 503, 504}

def delay_for_attempt(attempt):
    return min(2 ** attempt, 16)
