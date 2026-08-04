#!/usr/bin/env python3
"""Rebuild the `foobar-detect` plugin package and the repodata that indexes it.

The fixture is a real conda package, so changing what the plugin prints means
rebuilding an archive and updating three hashes: the two in `info/paths.json`
and the one in `noarch/repodata.json`. Doing that by hand is how a fixture ends
up with a hash that no longer matches its contents.

Run it from anywhere; it writes only inside this directory.
"""

import hashlib
import io
import json
import tarfile
from pathlib import Path

CHANNEL = Path(__file__).resolve().parent
PACKAGE = "foobar-detect"
VERSION = "1.0.0"
BUILD_STRING = "h0000000_0"
ARCHIVE = CHANNEL / "noarch" / f"{PACKAGE}-{VERSION}-{BUILD_STRING}.tar.bz2"

# Fixed so the archive is byte-for-byte reproducible.
TIMESTAMP = 1700000000

REPORT = {
    "virtual_packages": {
        "__foobar": {"version": "1.2.3"},
        "__foobar_arch": {"version": "0", "build_string": "gen4"},
    },
    "cache": {"ttl_seconds": 3600},
}

PREAMBLE = (
    "Test fixture: reports fixed verdicts for the virtual packages this channel "
    f"registers {PACKAGE} for, so detection can be exercised without the hardware. "
    "Real plugins inspect the system here."
)


def entry_points() -> dict[str, str]:
    """The plugin itself, one script per platform, printing the same report."""
    report = json.dumps(REPORT)
    return {
        f"bin/{PACKAGE}": (
            f"#!/bin/sh\n"
            f"# {PREAMBLE}\n"
            f"printf '%s\\n' '{report}'\n"
            f"exit 0\n"
        ),
        f"Scripts/{PACKAGE}.bat": (
            f"@echo off\r\n"
            f"REM {PREAMBLE}\r\n"
            f"echo {report}\r\n"
            f"exit /b 0\r\n"
        ),
    }


def sha256(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def build_archive(files: dict[str, bytes]) -> bytes:
    """A tarball with fixed metadata, so identical contents give an identical file."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w:bz2", format=tarfile.GNU_FORMAT) as archive:
        for name in sorted(files):
            info = tarfile.TarInfo(name)
            info.size = len(files[name])
            info.mtime = TIMESTAMP
            info.mode = 0o755 if name.startswith(("bin/", "Scripts/")) else 0o644
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            archive.addfile(info, io.BytesIO(files[name]))
    return raw.getvalue()


def index() -> dict[str, object]:
    return {
        "build": BUILD_STRING,
        "build_number": 0,
        "depends": [],
        "name": PACKAGE,
        "noarch": "generic",
        "subdir": "noarch",
        "timestamp": TIMESTAMP * 1000,
        "version": VERSION,
    }


def main() -> None:
    files = {name: body.encode() for name, body in entry_points().items()}
    paths = {
        "paths": [
            {
                "_path": name,
                "path_type": "hardlink",
                "sha256": sha256(files[name]),
                "size_in_bytes": len(files[name]),
            }
            for name in sorted(files)
        ],
        "paths_version": 1,
    }
    files["info/index.json"] = (json.dumps(index(), indent=2) + "\n").encode()
    files["info/paths.json"] = (json.dumps(paths, indent=2) + "\n").encode()

    archive = build_archive(files)
    ARCHIVE.write_bytes(archive)

    repodata_path = CHANNEL / "noarch" / "repodata.json"
    repodata = json.loads(repodata_path.read_text())
    repodata["packages"][ARCHIVE.name] = index() | {
        "md5": hashlib.md5(archive).hexdigest(),
        "sha256": sha256(archive),
        "size": len(archive),
    }
    repodata_path.write_text(json.dumps(repodata, indent=2) + "\n")

    print(f"wrote {ARCHIVE.relative_to(CHANNEL)} ({len(archive)} bytes)")


if __name__ == "__main__":
    main()
