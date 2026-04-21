# decorated.py -- decorators + imports must survive normalization verbatim.

from dataclasses import dataclass
from functools import lru_cache
import logging as log


@lru_cache(maxsize=128)
def cached_square(n: int) -> int:
    return n * n


@dataclass
class Point:
    x: int
    y: int

    @staticmethod
    def origin() -> "Point":
        return Point(0, 0)

    @property
    def magnitude(self) -> int:
        return self.x * self.x + self.y * self.y
