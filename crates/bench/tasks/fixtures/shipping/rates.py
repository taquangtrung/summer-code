"""Shipping rates, in whole cents per kilogram."""

RATES = {
    "ground": 450,
    "air": 1200,
}


def quote(service, kilos):
    return RATES[service] * kilos
