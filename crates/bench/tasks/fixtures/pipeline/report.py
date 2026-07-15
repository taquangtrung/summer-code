"""Turning records into a total."""

from records import parse_row


def load(rows):
    return [parse_row(row) for row in rows if row.strip()]


def total_cents(records):
    return sum(record.subtotal() for record in records)


def format_total(records):
    return "total: {}.{:02d}".format(total_cents(records) // 100, total_cents(records) % 100)
