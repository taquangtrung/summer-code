"""Small helpers over lists of numbers."""


def total(xs):
    # BUG: sums a name that does not exist instead of the argument.
    return sum(x)


def mean(xs):
    return total(xs) / len(xs) if xs else 0.0
