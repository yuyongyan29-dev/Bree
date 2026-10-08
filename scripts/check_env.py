"""Explicit terminal settings and private homes for Bree verification children."""
import contextlib
import os
from pathlib import Path
import tempfile


def bree_environment(home_dir):
    """Use an explicit HOME, never the caller's home or color settings."""
    home_dir = Path(home_dir)
    if not home_dir.is_absolute():
        raise ValueError("HOME must be absolute")
    env = dict(os.environ, TERM="xterm-256color", COLORTERM="truecolor",
               HOME=str(home_dir))
    env.pop("NO_COLOR", None)
    env.pop("COLORFGBG", None)
    return env


@contextlib.contextmanager
def empty_bree_environment(output=None):
    """Keep a fresh private home beside evidence until owned children finish."""
    parent = (Path(output).resolve().parent if output is not None else
              Path(__file__).resolve().parent.parent / ".artifacts/stability")
    parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="bree-check-", dir=parent) as temporary:
        yield bree_environment(Path(temporary) / "home")
