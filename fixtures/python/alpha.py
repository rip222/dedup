# alpha.py -- source side of the Python Type-1 and Type-2 duplicate pairs.

from typing import List
from .types import Row, Total
import util.helpers


# Type-1 duplicate: identical to `type1_identical` in beta.py.
def type1_identical(rows: List) -> Total:
    sum_ = 0
    count = 0
    for r in rows:
        sum_ += r.value
        count += 1
    mean = sum_ / count if count > 0 else 0
    return Total(sum=sum_, count=count, mean=mean)


# Type-2 duplicate: same structure as `type2_shared` in beta.py,
# differs only in local binding and parameter names. Function name
# stays the same because Python has no anonymous function construct
# for this pattern and names are Kept by the profile.
def type2_shared(items: List) -> Total:
    tot = 0
    cnt = 0
    for item in items:
        tot += item.value
        cnt += 1
    avg = tot / cnt if cnt > 0 else 0
    return Total(sum=tot, count=cnt, mean=avg)


# Unique function -- must NOT bucket with the duplicates.
def unique_alpha() -> int:
    base = 7
    return base * 2 + 1


class Aggregator:
    def __init__(self, factor: int):
        self.factor = factor

    def scale(self, values: List) -> int:
        acc = 0
        for v in values:
            acc += v * self.factor
        return acc
