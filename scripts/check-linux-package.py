"""Verify the Linux release .deb on its Ubuntu 22.04 build host."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile


def output(*command):
    return subprocess.check_output(command, text=True).strip()


def require(condition, message):
    if not condition:
        raise SystemExit(message)


def verify(package):
    root = Path(__file__).resolve().parent.parent
    version = json.loads((root / "package.json").read_text())["version"]
    for key, expected in {"Package": "suonelo", "Version": version, "Architecture": "amd64"}.items():
        require(output("dpkg-deb", "-f", package, key) == expected, f"Unexpected Debian {key}")
    dependencies = output("dpkg-deb", "-f", package, "Depends")
    for library in ["libc6", "libdbus-1-3", "libwebkit2gtk-4.1-0", "libgtk-3-0"]:
        require(library in dependencies, f"Missing Debian dependency: {library}")

    with tempfile.TemporaryDirectory(prefix="suonelo-deb-") as directory:
        unpacked = Path(directory)
        subprocess.run(["dpkg-deb", "-x", package, unpacked], check=True)
        files = {path.relative_to(unpacked).as_posix() for path in unpacked.rglob("*") if path.is_file()}
        expected = {
            "usr/bin/suonelo",
            "usr/share/applications/suonelo.desktop",
            "usr/share/doc/suonelo/ASSET-NOTICES.md",
            "usr/share/doc/suonelo/changelog.gz",
            "usr/share/doc/suonelo/copyright",
            *(f"usr/share/doc/suonelo/licenses/{license.name}" for license in (root / "licenses").glob("*.txt")),
        }
        icons = {f"usr/share/icons/hicolor/{size}x{size}/apps/suonelo.png" for size in [32, 128, 256]}
        require(files == expected | icons, f"Unexpected Debian package contents: {sorted(files ^ (expected | icons))}")
        require(not any(path.is_symlink() for path in unpacked.rglob("*")), "The Debian package contains symlinks")
        desktop = unpacked / "usr/share/applications/suonelo.desktop"
        subprocess.run(["desktop-file-validate", desktop], check=True)
        entry = desktop.read_text().splitlines()
        require("Icon=suonelo" in entry and any(line in entry for line in ("Exec=suonelo", "Exec=/usr/bin/suonelo")), "Unexpected icon or executable in desktop entry")

        executable = unpacked / "usr/bin/suonelo"
        header = output("readelf", "-h", executable)
        require("Advanced Micro Devices X86-64" in header, "The executable is not x86-64")
        sections = output("readelf", "-SW", executable)
        require(not re.search(r"\.(?:debug_|zdebug_|symtab\b)", sections), "The executable includes debug sections or symbols")
        version_info = output("readelf", "--version-info", executable)
        glibc = [tuple(map(int, match.split("."))) for match in re.findall(r"\bGLIBC_(\d+(?:\.\d+)+)\b", version_info)]
        require(glibc and max(glibc) <= (2, 35), "The executable exceeds the Ubuntu 22.04 glibc baseline")
        declared_glibc = re.search(r"libc6 \(>= (\d+(?:\.\d+)+)\)", dependencies)
        require(declared_glibc and tuple(map(int, declared_glibc[1].split("."))) >= max(glibc), "Debian libc6 dependency understates the ELF requirement")
        require("GLIBC_ABI_DT_RELR" not in version_info, "The executable requires a newer glibc loader")
        linked = output("ldd", executable)
        require("not found" not in linked, "The executable has unresolved runtime libraries")
        dynamic = output("readelf", "-d", executable)
        require(not re.search(r"(libFLAC|libavcodec|libavformat|libsndfile)", dynamic, re.IGNORECASE), "The executable directly links an external audio codec")
        payload = executable.read_bytes()
        for variable in ("HOME", "GITHUB_WORKSPACE", "RUNNER_TEMP", "CARGO_HOME", "RUSTUP_HOME"):
            value = os.environ.get(variable)
            if value and len(value) > 5:
                require(os.fsencode(value) not in payload, f"The executable includes a private {variable} path")
        require(os.fsencode(root) not in payload, "The executable includes the local checkout path")

        print(f"Verified {package.name}, {executable.stat().st_size:,} executable bytes, GLIBC {'.'.join(map(str, max(glibc)))}")
        print(f"Debian dependencies: {dependencies}")
        print(dynamic)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("package", type=Path)
    verify(parser.parse_args().package)
