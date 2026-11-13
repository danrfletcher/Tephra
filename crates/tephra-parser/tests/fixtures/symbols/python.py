"""Module docstring."""
import os

TOP = 1
a, b = 1, 2
TYPED: int = 3
handler = lambda: 1


def free(x):
    """Doc."""
    local = 1

    class Inside:
        pass

    def nested():
        pass

    return local


@decorator
@other(1)
def decorated():
    pass


class Thing(Base):
    attr = 1

    def method(self):
        pass

    @property
    def prop(self):
        return 1

    async def amethod(self):
        pass

    class Sub:
        def deep(self):
            pass


async def coroutine():
    pass


if True:
    CONDITIONAL = 1
