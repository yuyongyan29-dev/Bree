"""Explicit terminal settings and private stores for Bree verification children."""
import contextlib
import os
from pathlib import Path
import tempfile


def bree_environment(data_dir):
    """Use an explicit fixture/store, never the caller's data or color settings."""
    data_dir = Path(data_dir)
    if not data_dir.is_absolute():
        raise ValueError("BREE_DATA_DIR must be absolute")
    env = dict(os.environ, TERM="xterm-256color", COLORTERM="truecolor",
               BREE_DATA_DIR=str(data_dir))
    env.pop("NO_COLOR", None)
    env.pop("COLORFGBG", None)
    return env


@contextlib.contextmanager
def empty_bree_environment(output=None):
    """Keep a fresh private store beside evidence until owned children finish."""
    parent = (Path(output).resolve().parent if output is not None else
              Path(__file__).resolve().parent.parent / ".artifacts/stability")
    parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="bree-check-", dir=parent) as temporary:
        yield bree_environment(Path(temporary) / "store")
