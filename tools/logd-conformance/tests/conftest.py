"""Put the harness package on the path so the tests import it without an install."""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
