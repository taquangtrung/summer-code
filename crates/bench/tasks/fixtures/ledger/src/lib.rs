//! A tiny account ledger: entries in, balance out.

/// One movement of money, in whole cents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub cents: i64,
    pub kind: Kind,
}

/// Which direction an entry moves the balance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Credit,
    Debit,
}

/// The balance after applying every entry in order, starting from zero.
pub fn balance(entries: &[Entry]) -> i64 {
    let mut total = 0;
    for entry in entries {
        total += entry.cents;
    }
    total
}

/// The largest single credit, or `None` when there are no credits.
pub fn largest_credit(entries: &[Entry]) -> Option<i64> {
    entries
        .iter()
        .filter(|entry| entry.kind == Kind::Credit)
        .map(|entry| entry.cents)
        .max()
}

/// Format whole cents as a decimal amount: `1234` becomes `"12.34"`.
pub fn format_cents(cents: i64) -> String {
    format!("{}.{:02}", cents / 100, (cents % 100).abs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credit(cents: i64) -> Entry {
        Entry {
            cents,
            kind: Kind::Credit,
        }
    }

    fn debit(cents: i64) -> Entry {
        Entry {
            cents,
            kind: Kind::Debit,
        }
    }

    #[test]
    fn a_debit_lowers_the_balance() {
        // A debit carries a positive `cents` like any other entry; its
        // direction is `Kind`, not the sign.
        let entries = vec![credit(1000), debit(250), credit(100)];
        assert_eq!(balance(&entries), 850);
    }

    #[test]
    fn the_largest_credit_ignores_debits() {
        let entries = vec![credit(500), debit(9000), credit(700)];
        assert_eq!(largest_credit(&entries), Some(700));
    }

    #[test]
    fn cents_format_with_two_places() {
        assert_eq!(format_cents(1234), "12.34");
        assert_eq!(format_cents(5), "0.05");
    }
}
