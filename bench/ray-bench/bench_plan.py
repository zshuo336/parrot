"""Workload data; preparing reference inputs never starts Ray."""

from fractions import Fraction


class Plan:
    def __init__(self, small=False, scale="1"):
        self.small = small
        self.scale = Fraction(scale)
        if not 1 <= self.scale <= 1_000_000:
            raise ValueError("scale must be between 1 and 1000000")

    def count(self, full, check):
        return check if self.small else full

    def iterations(self, full):
        n = full
        # Exact nearest integer, ties to even, matching round without float error.
        q, r = divmod(n * self.scale.denominator, self.scale.numerator)
        result = max(
            1,
            q
            + (
                2 * r > self.scale.numerator
                or (2 * r == self.scale.numerator and q % 2 == 1)
            ),
        )
        return min(result, 5000) if self.small else result

    def keys(self):
        it, c = self.iterations, self.count
        keys = {(it(200_000), i) for i in range(c(100, 6))}
        keys.update(
            (it(200_000), a * 1000 + k) for a in range(c(4, 2)) for k in range(c(25, 3))
        )
        keys.update(
            (it(n), s)
            for n, s in [
                (50_000_000, 1),
                (2_000_000_000, 7),
                (1_000_000_000, 7),
                (3_500_000_000, 1),
                (6_000_000_000, 7),
            ]
        )
        keys.update((it(4_400_000_000), a) for a in range(c(4, 2)))
        keys.update((it(26_000_000), k) for k in range(c(10, 2)))
        keys.update((it(26_000_000), k) for k in range(4))
        keys.update((it(1000), k) for k in range(c(300, 8)))
        keys.update(
            (it(170_000), a * 10000 + k) for a in range(4) for k in range(c(1250, 8))
        )
        return keys
