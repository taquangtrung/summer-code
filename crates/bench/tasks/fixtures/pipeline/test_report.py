"""What the pipeline must do. Run with `python3 -m unittest discover`."""

import unittest

from records import parse_row
from report import format_total, load, total_cents


class TestPipeline(unittest.TestCase):
    def test_a_row_becomes_a_record(self):
        record = parse_row("widget, 3, 250")
        self.assertEqual(record.name, "widget")
        self.assertEqual(record.subtotal(), 750)

    def test_the_total_sums_subtotals(self):
        records = load(["widget,3,250", "", "bolt,10,5"])
        self.assertEqual(total_cents(records), 800)

    def test_the_total_formats_as_an_amount(self):
        self.assertEqual(format_total(load(["widget,3,250"])), "total: 7.50")

    def test_a_bad_row_is_rejected(self):
        # `parse_row` must refuse a row it cannot honestly turn into a record,
        # by raising ValueError. It currently accepts all three of these.
        for row in ("  , 3, 250", "widget, 0, 250", "widget, 3, -1"):
            with self.assertRaises(ValueError):
                parse_row(row)


if __name__ == "__main__":
    unittest.main()
