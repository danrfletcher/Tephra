import functools


@functools.lru_cache
def cached(n):
    return n


class Greeter:
    def hello(self):
        return "hi"


class Other:
    def hello(self):
        return "yo"


def documented(): """first line
    second line"""


def spaced(a):  # keep me
    return a


def backslash(a, \
              b):
    return a + b
