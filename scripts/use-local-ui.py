#!/usr/bin/env python3
"""Use an exact, locally available ochub-ui commit without publishing it first.

Only the ignored .cargo/config.toml and .cargo/local-sources are written. Cargo's
source replacement keeps the Git identity in Cargo.lock; normal builds continue
to use the pinned upstream revision when this local configuration is absent.
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import tomllib


BEGIN = "# BEGIN ochub local UI source\n"
END = "# END ochub local UI source\n"


def git(repository: Path, *args: str) -> bytes:
    return subprocess.check_output(
        ["git", "-C", str(repository), *args], stderr=subprocess.PIPE
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repository", type=Path, help="local ochub-ui Git checkout")
    parser.add_argument(
        "--gpui-repository", type=Path,
        help="also cache the pinned GPUI commit from this local Zed checkout",
    )
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    dependency = tomllib.loads((root / "Cargo.toml").read_text())["workspace"][
        "dependencies"
    ]["ochub-ui"]
    gpui = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["dependencies"]["gpui"]
    if args.gpui_repository:
        args.gpui_repository = args.gpui_repository.resolve()
        try:
            pinned = git(args.gpui_repository, "rev-parse", gpui["rev"] + "^{commit}").decode().strip()
        except subprocess.CalledProcessError:
            parser.error(f"the local GPUI repository cannot provide {gpui['rev']}")
        if pinned != gpui["rev"]:
            parser.error("Cargo.toml must pin the full GPUI commit ID")
    repository = args.repository.resolve()
    try:
        revision = git(repository, "rev-parse", dependency["rev"] + "^{commit}").decode().strip()
        archive = git(repository, "archive", "--format=tar", revision)
    except subprocess.CalledProcessError:
        parser.error(
            f"the local UI repository cannot provide pinned commit {dependency['rev']}; "
            "use a checkout that contains this commit"
        )
    if revision != dependency["rev"]:
        parser.error("Cargo.toml must pin the full ochub-ui commit ID")

    cargo_dir = root / ".cargo"
    config_path = cargo_dir / "config.toml"
    original = config_path.read_text() if config_path.exists() else ""
    if BEGIN in original or END in original:
        if (
            original.count(BEGIN) != 1
            or original.count(END) != 1
            or original.index(BEGIN) > original.index(END)
        ):
            parser.error("local source markers in .cargo/config.toml are malformed")
        before, remainder = original.split(BEGIN, 1)
        _, after = remainder.split(END, 1)
        original = before + after
    existing_sources = tomllib.loads(original).get("source", {})
    if any(
        name in {"ochub-ui-pinned", "ochub-ui-local"}
        or source.get("git") == dependency["git"]
        for name, source in existing_sources.items()
    ):
        parser.error(".cargo/config.toml already configures this UI source")

    sources = cargo_dir / "local-sources"
    sources.mkdir(parents=True, exist_ok=True)
    destination = sources / revision
    with tempfile.TemporaryDirectory(prefix="prepare-", dir=sources) as temporary:
        prepared = Path(temporary)
        package = prepared / "ochub-ui"
        package.mkdir()
        with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
            tar.extractall(package, filter="data")
        manifest = tomllib.loads((package / "Cargo.toml").read_text())["package"]
        if manifest["name"] != "ochub-ui" or manifest["version"] != dependency["version"]:
            parser.error("the pinned commit does not match the expected ochub-ui package")
        checksums = {
            path.relative_to(package).as_posix(): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in sorted(package.rglob("*"))
            if path.is_file()
        }
        (package / ".cargo-checksum.json").write_text(
            json.dumps({"files": checksums, "package": None}, sort_keys=True) + "\n"
        )
        if destination.exists():
            shutil.rmtree(destination)
        prepared.rename(destination)

    block = (
        BEGIN
        + f'[source.ochub-ui-pinned]\ngit = {json.dumps(dependency["git"])}\n'
        + f'rev = {json.dumps(revision)}\nreplace-with = "ochub-ui-local"\n\n'
        + "[source.ochub-ui-local]\n"
        + f"directory = {json.dumps(str(destination))}\n"
        + END
    )
    updated = original.rstrip() + ("\n\n" if original.strip() else "") + block
    temporary_config = config_path.with_suffix(".toml.tmp")
    temporary_config.write_text(updated)
    temporary_config.replace(config_path)
    if args.gpui_repository:
        # Git URL rewriting applies only to this child fetch. Cargo retains the
        # public source identity and verifies the exact pinned commit.
        environment = os.environ.copy()
        count = int(environment.get("GIT_CONFIG_COUNT", "0"))
        environment[f"GIT_CONFIG_KEY_{count}"] = f"url.{args.gpui_repository.as_uri()}.insteadOf"
        environment[f"GIT_CONFIG_VALUE_{count}"] = gpui["git"]
        environment["GIT_CONFIG_COUNT"] = str(count + 1)
        environment["CARGO_NET_GIT_FETCH_WITH_CLI"] = "true"
        host = next(
            line.removeprefix("host: ")
            for line in subprocess.check_output(["rustc", "-vV"], text=True).splitlines()
            if line.startswith("host: ")
        )
        subprocess.run(
            ["cargo", "fetch", "--config", str(config_path), "--locked", "--target", host],
            cwd=root, env=environment, check=True,
        )
        print(f"Local GPUI commit cached: {gpui['rev']}")
    print(f"Local ochub-ui source ready: {revision}")
    print("Cargo.toml and Cargo.lock retain the pinned upstream Git source.")


if __name__ == "__main__":
    main()
