def simulate_lru(capacity, accesses):
    cache = []
    for key in accesses:
        if key not in cache: cache.append(key)
    return {"hits": 0, "misses": len(cache), "evictions": [], "keys": cache}
