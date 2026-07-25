def parse_bool(value):
    if value == "true": return True
    if value == "false": return False
    raise ValueError("invalid boolean")

def parse_port(value):
    port = int(value)
    if not 1 <= port <= 65535: raise ValueError("invalid port")
    return port

SCHEMA = {"debug": parse_bool, "port": parse_port, "name": str}
