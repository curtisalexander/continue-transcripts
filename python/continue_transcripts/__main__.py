"""Allow running as `python -m continue_transcripts`."""

import os
import shutil
import subprocess
import sys


def _run():
    name = "continue-transcripts.exe" if sys.platform == "win32" else "continue-transcripts"
    binary = shutil.which(name)
    if binary is None:
        print(f"Error: could not find '{name}' on PATH.", file=sys.stderr)
        print(
            "Reinstall the package: pip install continue-transcripts",
            file=sys.stderr,
        )
        sys.exit(1)
    if sys.platform == "win32":
        sys.exit(subprocess.call([binary, *sys.argv[1:]]))
    else:
        os.execvp(binary, [binary, *sys.argv[1:]])


if __name__ == "__main__":
    _run()
