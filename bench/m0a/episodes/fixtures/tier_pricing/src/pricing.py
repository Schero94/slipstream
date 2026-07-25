def quote(quantity, customer):
    """Return quantity, customer, subtotal, discount, and total."""
    if quantity <= 10:
        subtotal = quantity * 12
        if customer == "regular":
            discount = 0
        elif customer == "partner":
            discount = subtotal * 0.10
        else:
            discount = subtotal
    elif quantity <= 50:
        subtotal = quantity * 9
        if customer == "regular":
            discount = 0
        elif customer == "partner":
            discount = subtotal * 0.10
        else:
            discount = subtotal
    else:
        subtotal = quantity * 7
        if customer == "regular":
            discount = 0
        elif customer == "partner":
            discount = subtotal * 0.10
        else:
            discount = subtotal
    return {
        "quantity": quantity,
        "customer": customer,
        "subtotal": subtotal,
        "discount": discount,
        "total": subtotal - discount,
    }
