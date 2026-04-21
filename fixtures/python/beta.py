# beta.py -- dedupe partner for alpha.py.

from typing import List
from .types import Row, Total
import util.helpers


# Type-1 duplicate of alpha.py::type1_identical -- same everything.
def type1_identical(rows: List) -> Total:
    sum_ = 0
    count = 0
    for r in rows:
        sum_ += r.value
        count += 1
    mean = sum_ / count if count > 0 else 0
    return Total(sum=sum_, count=count, mean=mean)


# Type-2 duplicate of alpha.py::type2_shared -- locals + params
# renamed, same control flow, same attribute names, same literal shape.
def type2_shared(entries: List) -> Total:
    total = 0
    seen = 0
    for entry in entries:
        total += entry.value
        seen += 1
    average = total / seen if seen > 0 else 0
    return Total(sum=total, count=seen, mean=average)


# Unique function with a string literal that must NOT collapse into
# the number-only unique_alpha in alpha.py.
def unique_beta() -> str:
    greeting = "hello"
    return greeting.upper()
