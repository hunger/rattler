#!/usr/bin/env python3
"""Rebuild this channel's fixture packages and the repodata that indexes them.

The fixtures are real conda packages, so changing what one of them prints means
rebuilding an archive and updating the hashes that describe it: the ones in its
own `info/paths.json` and the ones in `noarch/repodata.json`. Doing that by hand
is how a fixture ends up with a hash that no longer matches its contents.

Two kinds of package live here:

- `foobar-detect`, the virtual package plugin this channel registers. It reports
  fixed verdicts so detection can be exercised without the hardware.
- `foobar-probe`, in two flavours, for checking by hand whether a virtual package
  reached the solver. See `probe_packages`.

Run it from anywhere; it writes only inside this directory.
"""

import hashlib
import io
import json
import tarfile
from dataclasses import dataclass
from pathlib import Path

CHANNEL = Path(__file__).resolve().parent
PLUGIN = "foobar-detect"
PROBE = "foobar-probe"
VERSION = "1.0.0"

# Fixed so the archives are byte-for-byte reproducible.
TIMESTAMP = 1700000000

REPORT = {
    "virtual_packages": {
        "__foobar": {"version": "1.2.3"},
        "__foobar_arch": {"version": "0", "build_string": "gen4"},
    },
    "cache": {"ttl_seconds": 3600},
}

PLUGIN_PREAMBLE = (
    "Test fixture: reports fixed verdicts for the virtual packages this channel "
    f"registers {PLUGIN} for, so detection can be exercised without the hardware. "
    "Real plugins inspect the system here."
)


@dataclass(frozen=True)
class Package:
    """One fixture package: a `noarch: generic` archive and its repodata entry."""

    name: str
    build_string: str
    build_number: int
    depends: tuple[str, ...]
    files: dict[str, str]

    @property
    def archive_name(self) -> str:
        return f"{self.name}-{VERSION}-{self.build_string}.tar.bz2"

    def index(self) -> dict[str, object]:
        return {
            "build": self.build_string,
            "build_number": self.build_number,
            "depends": list(self.depends),
            "name": self.name,
            "noarch": "generic",
            "subdir": "noarch",
            "timestamp": TIMESTAMP * 1000,
            "version": VERSION,
        }


def scripts(name: str, comment: str, message: str) -> dict[str, str]:
    """One entry point per platform, both printing `message`."""
    return {
        f"bin/{name}": (
            f"#!/bin/sh\n# {comment}\nprintf '%s\\n' '{message}'\nexit 0\n"
        ),
        f"Scripts/{name}.bat": (
            f"@echo off\r\nREM {comment}\r\necho {message}\r\nexit /b 0\r\n"
        ),
    }


def plugin_package() -> Package:
    """The virtual package plugin this channel registers."""
    return Package(
        name=PLUGIN,
        build_string="h0000000_0",
        build_number=0,
        depends=(),
        files=scripts(PLUGIN, PLUGIN_PREAMBLE, json.dumps(REPORT)),
    )


def probe_packages() -> list[Package]:
    """Two flavours of one package, telling you which one the solver could take.

    Both are `foobar-probe 1.0.0`. The only differences are that one depends on
    `__foobar` and the other does not, and that the depending one has the higher
    build number -- so a solver takes it whenever it can, and falls back to the
    other only when `__foobar` is missing.

    Running `foobar-probe` from the resulting environment therefore reports
    whether the virtual package reached the solver. Which is the point: the
    verdict is decided at solve time and merely read back afterwards, so it stays
    true even though the script itself checks nothing.
    """
    common = (
        f"Test fixture: one of the two {PROBE} builds. Which one a solve picks "
        "depends on whether __foobar was offered to the solver."
    )
    return [
        Package(
            name=PROBE,
            build_string="with_foobar",
            build_number=1,
            depends=("__foobar >=1.0",),
            files=scripts(
                PROBE,
                common,
                "__foobar WAS available at solve time "
                '(the "with_foobar" build was installable).',
            ),
        ),
        Package(
            name=PROBE,
            build_string="without_foobar",
            build_number=0,
            depends=(),
            files=scripts(
                PROBE,
                common,
                "__foobar was NOT available at solve time "
                '(fell back to the "without_foobar" build).',
            ),
        ),
    ]


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


def write(package: Package) -> dict[str, object]:
    """Writes the archive and returns the repodata entry describing it."""
    files = {name: body.encode() for name, body in package.files.items()}
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
    files["info/index.json"] = (json.dumps(package.index(), indent=2) + "\n").encode()
    files["info/paths.json"] = (json.dumps(paths, indent=2) + "\n").encode()

    archive = build_archive(files)
    (CHANNEL / "noarch" / package.archive_name).write_bytes(archive)
    print(f"wrote noarch/{package.archive_name} ({len(archive)} bytes)")

    return package.index() | {
        "md5": hashlib.md5(archive).hexdigest(),
        "sha256": sha256(archive),
        "size": len(archive),
    }


def main() -> None:
    packages = [plugin_package(), *probe_packages()]
    entries = {package.archive_name: write(package) for package in packages}

    repodata_path = CHANNEL / "noarch" / "repodata.json"
    repodata = json.loads(repodata_path.read_text())
    repodata["packages"] = dict(sorted((repodata["packages"] | entries).items()))
    repodata_path.write_text(json.dumps(repodata, indent=2) + "\n")


if __name__ == "__main__":
    main()
