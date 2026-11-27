#!/usr/bin/env python3
"""Remove build and dependency folders using the host OS shell (bash or PowerShell)."""

from __future__ import annotations

import argparse
from pathlib import Path
import platform
import shlex
import shutil
import subprocess
import sys


TARGET_FOLDERS = (
    Path(".next"),
    Path("node_modules"),
    Path("out"),
    Path("src-tauri") / "target",
)

# User-populated QA data that may live under src-tauri/target must not be deleted.
PROTECTED_PATHS = (
    Path("src-tauri") / "target" / "native-qa" / "data",
)


def detect_shell(system_name: str | None = None) -> tuple[str, str, str]:
    """Return (os_label, shell_kind, executable) for the current or given host OS."""
    system = system_name if system_name is not None else platform.system()
    normalized = system.strip().lower()
    if normalized == "windows":
        executable = shutil.which("pwsh") or shutil.which("powershell") or "powershell"
        return ("Windows", "powershell", executable)
    if normalized == "darwin":
        executable = shutil.which("bash") or "bash"
        return ("macOS", "bash", executable)
    if normalized == "linux":
        executable = shutil.which("bash") or "bash"
        return ("Linux", "bash", executable)
    raise RuntimeError(f"Unsupported host OS environment: {system!r}")


def build_delete_command(shell_kind: str, executable: str, target: Path) -> list[str]:
    """Build the OS-specific shell command that recursively removes target."""
    target_str = str(target)
    if shell_kind == "powershell":
        escaped = target_str.replace("'", "''")
        ps_command = f"Remove-Item -LiteralPath '{escaped}' -Recurse -Force -ErrorAction Stop"
        return [executable, "-NoProfile", "-NonInteractive", "-Command", ps_command]
    if shell_kind == "bash":
        return [executable, "-c", 'rm -rf -- "$1"', "_", target_str]
    raise ValueError(f"Unsupported shell kind: {shell_kind!r}")


def format_command(shell_kind: str, command: list[str]) -> str:
    """Format a command list cleanly for logging or dry-run output."""
    if shell_kind == "powershell":
        return subprocess.list2cmdline(command)
    return shlex.join(command)


def path_exists(path: Path) -> bool:
    """Return True if path exists or is a symlink (including broken symlinks)."""
    return path.exists() or path.is_symlink()


def expand_deletion_targets(
    root: Path,
    relative_target: Path,
    protected_relatives: tuple[Path, ...] = PROTECTED_PATHS,
) -> tuple[list[Path], list[Path]]:
    """Resolve concrete paths to delete for relative_target while preserving protected paths."""
    target = root / relative_target
    if not path_exists(target):
        return ([], [])

    active_protected: list[Path] = []
    for rel_protected in protected_relatives:
        candidate = root / rel_protected
        if path_exists(candidate) and (candidate == target or target in candidate.parents):
            active_protected.append(candidate)

    if not active_protected:
        return ([target], [])

    protected_set = set(active_protected)
    ancestors: set[Path] = set()
    for protected in active_protected:
        current = protected.parent
        while current != target.parent:
            ancestors.add(current)
            if current == target:
                break
            current = current.parent

    to_delete: list[Path] = []

    def collect(directory: Path) -> None:
        if directory in protected_set:
            return
        if directory not in ancestors:
            to_delete.append(directory)
            return
        for child in sorted(directory.iterdir(), key=lambda item: item.name.lower()):
            if child in protected_set:
                continue
            if child in ancestors and child.is_dir() and not child.is_symlink():
                collect(child)
            else:
                to_delete.append(child)

    collect(target)
    return (to_delete, sorted(active_protected))


def ensure_within_root(root: Path, target: Path) -> None:
    """Guard against accidentally deleting the root itself or any path outside root."""
    if target == root or root not in target.parents:
        raise ValueError(f"Refusing to delete path outside project root: {target}")


def cleanup(
    root: Path,
    *,
    dry_run: bool = False,
    system_name: str | None = None,
) -> int:
    """Detect the host OS shell and delete configured build/dependency folders under root."""
    resolved_root = root.resolve(strict=True)
    if not resolved_root.is_dir():
        raise NotADirectoryError(f"Project root is not a directory: {resolved_root}")

    os_label, shell_kind, executable = detect_shell(system_name)
    print(f"Detected host OS: {os_label} -> using {shell_kind} ({executable})")
    print(f"Project root: {resolved_root}")

    deleted_count = 0
    skipped_count = 0
    preserved_all: list[Path] = []

    for relative_folder in TARGET_FOLDERS:
        display_rel = relative_folder.as_posix()
        items, preserved = expand_deletion_targets(resolved_root, relative_folder)
        if not items and not preserved:
            print(f"[skip] {display_rel} (not found)")
            skipped_count += 1
            continue

        for protected_path in preserved:
            preserved_all.append(protected_path)
            print(f"[preserve] {protected_path.relative_to(resolved_root).as_posix()} (protected user data)")

        for item in items:
            ensure_within_root(resolved_root, item)
            rel_item = item.relative_to(resolved_root).as_posix()
            command = build_delete_command(shell_kind, executable, item)
            if dry_run:
                print(f"[dry-run] {rel_item} -> {format_command(shell_kind, command)}")
            else:
                print(f"[delete] {rel_item}")
                subprocess.run(command, cwd=resolved_root, check=True)
            deleted_count += 1

    action = "Would delete" if dry_run else "Deleted"
    print(
        f"Done. {action} {deleted_count} item(s), skipped {skipped_count} missing folder(s), "
        f"preserved {len(preserved_all)} protected path(s)."
    )
    return 0


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--project",
        type=Path,
        default=Path(__file__).resolve().parents[1],
        help="Project root directory (defaults to the repository root).",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the detected OS, shell, and commands without deleting anything.",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    return cleanup(args.project, dry_run=args.dry_run)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError, ValueError, subprocess.CalledProcessError) as error:
        sys.exit(f"Cleanup failed: {error}")
