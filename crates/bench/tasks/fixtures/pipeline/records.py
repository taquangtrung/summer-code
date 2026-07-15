"""Reading raw rows into records."""


class Record:
    def __init__(self, name, qty, price_cents):
        self.name = name
        self.qty = qty
        self.price_cents = price_cents

    def subtotal(self):
        return self.qty * self.price_cents


def parse_row(row):
    """Read one `name,qty,price` row into a Record.

    A blank name, a non-positive quantity, or a negative price is not a
    record; those rows are the caller's problem, not this function's.
    """
    name, qty, price = (part.strip() for part in row.split(","))
    return Record(name, int(qty), int(price))
