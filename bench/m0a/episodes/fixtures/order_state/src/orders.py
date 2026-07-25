def transition(state, event):
    """Return the next order state."""
    if event == "cancel":
        return "cancelled"
    if event == "pay":
        return "paid"
    return "shipped"
