#!/usr/bin/env python3
"""Create and restore-check a source archive on Linux, macOS, or Windows."""

import argparse
from dataclasses import dataclass
from datetime import datetime, timezone
from fnmatch import fnmatchcase
from hashlib import sha256
import os
from pathlib import Path, PurePosixPath
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
from uuid import uuid4


IGNORED_DIRECTORIES = {
    ".git", ".agent-backups", ".aws", ".next", ".venv", "__pycache__", "node_modules",
    "profile-backups",
}
IGNORED_ROOT_DIRECTORIES = {"build", "data", "dist", "out", "portable", "target"}
IGNORED_FILES = {".ds_store", "next-env.d.ts", "credentials.json", "secrets.json", "id_rsa", "id_ed25519"}
RELEASE_PATTERNS = ("suonelo-*-windows-x64", "suonelo-*-linux-x86_64*", "portable-*")
PLAN_DIRECTORIES = {"plan", "plans", "planning"}


@dataclass(frozen=True)
class Entry:
    mode: int = 0
    digest: str = ""
    link: str | None = None
    directory_link: bool = False


def digest(path):
    result = sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(block)
    return result.hexdigest()


def is_plan(parts):
    name = parts[-1]
    return (
        bool(PLAN_DIRECTORIES.intersection(parts[:-1]))
        or name in {"plan.md", "plans.md", "planning.md"}
        or name.endswith((".plan.md", "-plan.md"))
        or (name.startswith("plan-") and name.endswith(".md"))
        or parts == ("docs", "linux-port.md")
    )


def excludes(path, output_path, extra, include_plans):
    parts = tuple(part.lower() for part in path.parts)
    name = parts[-1]
    if output_path and (path == output_path or output_path in path.parents):
        return True
    if (
        IGNORED_DIRECTORIES.intersection(parts)
        or parts[0] in IGNORED_ROOT_DIRECTORIES
        or parts[0].startswith(".source-backup-")
        or parts[:2] in {("src-tauri", "gen"), ("src-tauri", "target")}
        or any(fnmatchcase(parts[0], pattern) for pattern in RELEASE_PATTERNS)
    ):
        return True
    if name in IGNORED_FILES or name.endswith((".swp", ".swo", ".tsbuildinfo", ".key", ".p12", ".pfx")):
        return True
    if (name == ".env" or name.startswith(".env.")) and not name.endswith((".example", ".sample", ".template")):
        return True
    if len(parts) == 1 and name.endswith((".zip", ".tar.gz", ".sha256", ".deb", ".appimage", ".dmg", ".exe", ".msi")):
        return True
    if not include_plans and is_plan(parts):
        return True
    return any(fnmatchcase(item.as_posix(), pattern) for item in (path, *path.parents) for pattern in extra)


def names(project, excluded):
    def walk(start):
        for directory, folders, files in os.walk(start):
            parent = Path(directory)
            for folder in folders[:]:
                child = parent / folder
                if excluded(PurePosixPath(child.relative_to(project).as_posix())):
                    folders.remove(folder)
                elif child.is_symlink():
                    folders.remove(folder)
                    yield child
            yield from (parent / file for file in files)

    if (project / ".git").exists():
        listed = subprocess.check_output(["git", "ls-files", "-co", "--exclude-standard", "-z"], cwd=project)
        candidates = [project / os.fsdecode(item) for item in listed.split(b"\0") if item]
    else:
        candidates = walk(project)

    found = set()
    for path in candidates:
        relative = PurePosixPath(path.relative_to(project).as_posix())
        if excluded(relative):
            continue
        if path.is_symlink() or path.is_file():
            found.add(relative.as_posix())
        elif path.is_dir():
            for child in walk(path):
                relative_child = PurePosixPath(child.relative_to(project).as_posix())
                if not excluded(relative_child):
                    found.add(relative_child.as_posix())
    return sorted(found)


def snapshot(project, paths):
    inventory = {}
    for name in paths:
        path = project / name
        info = path.lstat()
        if stat.S_ISLNK(info.st_mode):
            target = os.readlink(path)
            if Path(target).anchor or not path.resolve().is_relative_to(project):
                raise ValueError(f"Source link points outside the project: {name}")
            inventory[name] = Entry(link=target, directory_link=path.is_dir())
        elif stat.S_ISREG(info.st_mode) and path.resolve().is_relative_to(project):
            inventory[name] = Entry(mode=stat.S_IMODE(info.st_mode), digest=digest(path))
        else:
            raise ValueError(f"Unsupported source entry: {name}")
    return inventory


def create_archive(project, stage, entries):
    with tarfile.open(stage, "w:gz") as archive:
        for name, entry in entries.items():
            source = project / name
            member = tarfile.TarInfo(name)
            member.mtime = source.lstat().st_mtime
            if entry.link is not None:
                member.type = tarfile.SYMTYPE
                member.linkname = entry.link
                archive.addfile(member)
            else:
                member.mode = entry.mode
                with source.open("rb") as stream:
                    member.size = os.fstat(stream.fileno()).st_size
                    archive.addfile(member, stream)


def verify_restore(stage, restoration, entries):
    with tarfile.open(stage, "r:gz") as archive:
        if archive.getnames() != list(entries):
            raise ValueError("Archive membership differs from the source inventory")
        for member in archive.getmembers():
            entry = entries[member.name]
            restored = restoration / member.name
            restored.parent.mkdir(parents=True, exist_ok=True)
            if entry.link is not None:
                if not member.issym() or member.linkname != entry.link:
                    raise ValueError(f"Archive link differs: {member.name}")
                continue
            if not member.isfile() or member.mode != entry.mode:
                raise ValueError(f"Archive file metadata differs: {member.name}")
            with archive.extractfile(member) as source, restored.open("xb") as destination:
                shutil.copyfileobj(source, destination)
            restored.chmod(member.mode)
        for name, entry in entries.items():
            if entry.link is not None:
                (restoration / name).symlink_to(entry.link, target_is_directory=entry.directory_link)
    if snapshot(restoration, entries) != entries:
        raise ValueError("Restored files differ from the source inventory")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--project", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--output", type=Path, default=Path(".agent-backups"), help="Directory relative to the project or an absolute path")
    parser.add_argument("--include-plans", action="store_true", help="Include planning files in a full pre-edit snapshot")
    parser.add_argument("--exclude", action="append", default=[], metavar="GLOB", help="Exclude a project-relative glob. May be repeated.")
    args = parser.parse_args()
    project = args.project.resolve(strict=True)
    output = (project / args.output).resolve()
    relative_output = None
    if output != project and output.is_relative_to(project):
        relative_output = PurePosixPath(output.relative_to(project).as_posix())

    def excluded(path):
        return excludes(path, relative_output, args.exclude, args.include_plans)

    entries = snapshot(project, names(project, excluded))
    if not entries:
        raise ValueError("No source files were found")
    output.mkdir(mode=0o700, parents=True, exist_ok=True)
    timestamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    filename = f"{project.name}-source-{timestamp}-{uuid4().hex[:8]}.tar.gz"
    with tempfile.TemporaryDirectory(dir=output, prefix=".source-backup-") as temporary:
        stage = Path(temporary) / filename
        restored = Path(temporary).resolve() / "restored"
        restored.mkdir()
        create_archive(project, stage, entries)
        verify_restore(stage, restored, entries)
        if snapshot(project, names(project, excluded)) != entries:
            raise ValueError("The live source tree changed while the backup was running")
        checksum = digest(stage)
        checksum_file = stage.with_name(filename + ".sha256")
        checksum_file.write_bytes(f"{checksum}  {filename}\n".encode("utf-8"))
        stage.chmod(0o600)
        os.replace(stage, output / filename)
        os.replace(checksum_file, output / checksum_file.name)
    print(f"Verified {len(entries)} source files and links through isolated restoration")
    print(f"Archive {output / filename}")
    print(f"SHA-256 {checksum}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, tarfile.TarError, subprocess.CalledProcessError) as error:
        sys.exit(f"Source backup failed: {error}")
