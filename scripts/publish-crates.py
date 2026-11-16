#!/usr/bin/env python3
"""Verify and publish a zkcl workspace release, one crate at a time."""

import argparse
from dataclasses import dataclass
from datetime import datetime
from email.utils import parsedate_to_datetime
from graphlib import CycleError, TopologicalSorter
from hashlib import sha256
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import time
from urllib.error import HTTPError
from urllib.parse import quote
from urllib.request import Request, urlopen


INTERVAL = 10 * 60
API = "https://crates.io/api/v1/crates"
USER_AGENT = "zkcl-publish (https://github.com/zkcl-dev/zkcl)"
CARGO_OPTIONS = ("--locked", "--registry", "crates-io")


class ReleaseError(Exception):
    pass


@dataclass(frozen=True)
class Crate:
    name: str
    version: str

    @property
    def stem(self):
        return f"{self.name}-{self.version}"


@dataclass(frozen=True)
class Version:
    checksum: str
    created_at: float


def release_order(metadata):
    workspace = set(metadata["workspace_members"])
    members = {p["name"]: p for p in metadata["packages"] if p["id"] in workspace}
    publishable = {
        name: p
        for name, p in members.items()
        if p["publish"] is None or "crates-io" in p["publish"]
    }
    if not publishable:
        raise ReleaseError("No workspace crates can be published to crates.io")

    dependencies = {}
    for name, package in sorted(publishable.items()):
        local = set()
        for dep in package["dependencies"]:
            if not dep.get("path") or (dep["kind"] == "dev" and dep["req"] == "*"):
                continue
            target = publishable.get(dep["name"])
            if not target or Path(dep["path"]) != Path(target["manifest_path"]).parent:
                raise ReleaseError(f"{name} depends on {dep['name']}, outside this release")
            required_version = f"={target['version']}"
            if dep["req"] != required_version:
                raise ReleaseError(
                    f"{name} requires {dep['name']} {dep['req']}, expected {required_version}"
                )
            local.add(dep["name"])
        dependencies[name] = sorted(local)
    try:
        return [
            Crate(name, publishable[name]["version"])
            for name in TopologicalSorter(dependencies).static_order()
        ]
    except CycleError as error:
        raise ReleaseError(f"Cannot order the workspace dependencies: {error.args[1]}") from error


def retry_after(value):
    if not value:
        return 0
    if value.isdecimal():
        return int(value)
    try:
        return max(0, parsedate_to_datetime(value).timestamp() - time.time())
    except (TypeError, ValueError, OverflowError):
        return 0


def fetch(url, *, allow_missing=False):
    request = Request(url, headers={"User-Agent": USER_AGENT, "Cache-Control": "no-cache"})
    for attempt in range(5):
        delay = 2 ** attempt
        try:
            with urlopen(request, timeout=20) as response:
                return response.read()
        except HTTPError as error:
            if error.code == 404 and allow_missing:
                return None
            if error.code not in (408, 429) and not 500 <= error.code < 600:
                raise ReleaseError(f"HTTP {error.code} from {url}") from error
            delay = max(delay, retry_after(error.headers.get("Retry-After")))
            failure = f"HTTP {error.code}"
        except OSError as error:
            failure = str(error)
        if attempt == 4:
            raise ReleaseError(f"Cannot read {url}: {failure}")
        print(f"Retrying {url} in {delay:.0f}s ({failure})", flush=True)
        time.sleep(delay)


def checked_checksum(crate, checksum):
    if not isinstance(checksum, str) or not re.fullmatch(r"[0-9a-f]{64}", checksum):
        raise ReleaseError(f"Invalid checksum for {crate.stem}")
    return checksum


def published_version(crate):
    url = f"{API}/{quote(crate.name, safe='')}/{quote(crate.version, safe='')}"
    body = fetch(url, allow_missing=True)
    if body is None:
        return None
    entry = json.loads(body)["version"]
    if (entry["crate"], entry["num"]) != (crate.name, crate.version):
        raise ReleaseError(f"Unexpected package identity at {url}")
    if entry["yanked"] is not False:
        raise ReleaseError(f"{crate.stem} is yanked on crates.io")
    created = datetime.fromisoformat(entry["created_at"].replace("Z", "+00:00"))
    if created.tzinfo is None:
        raise ReleaseError(f"Invalid publish time for {crate.stem}")
    return Version(checked_checksum(crate, entry["checksum"]), created.timestamp())


def index_checksum(crate):
    name = crate.name.lower()
    prefixes = {1: "1", 2: "2", 3: f"3/{name[0]}"}
    prefix = prefixes.get(len(name), f"{name[:2]}/{name[2:4]}")
    body = fetch(f"https://index.crates.io/{prefix}/{name}", allow_missing=True)
    if body is None:
        return None
    if not body.strip():
        raise ReleaseError(f"Empty index entry for {crate.name}")
    for line in body.splitlines():
        entry = json.loads(line)
        if entry["vers"] == crate.version:
            if entry["name"] != crate.name or entry["yanked"] is not False:
                raise ReleaseError(f"Unexpected or yanked index entry for {crate.stem}")
            return checked_checksum(crate, entry["cksum"])
    return None


def wait_for(read, seconds):
    deadline = time.monotonic() + seconds
    while True:
        result = read()
        if result is not None:
            return result
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return None
        time.sleep(min(5, remaining))


def archive_files(data, stem):
    files = {}
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
        for member in archive:
            if member.isdir():
                continue
            prefix, _, path = member.name.partition("/")
            if prefix != stem or not path or not member.isfile() or path in files:
                raise ReleaseError(f"Unexpected entry in {stem}: {member.name}")
            # Cargo generates the lockfile and records the checkout when it packages.
            if path not in ("Cargo.lock", ".cargo_vcs_info.json"):
                with archive.extractfile(member) as source:
                    files[path] = (member.mode & 0o111, sha256(source.read()).digest())
    return files


def confirm_publish(crate, version, local):
    if sha256(local).hexdigest() != version.checksum:
        remote = fetch(f"https://static.crates.io/crates/{crate.name}/{quote(crate.stem)}.crate")
        if sha256(remote).hexdigest() != version.checksum:
            raise ReleaseError(f"Download checksum mismatch for {crate.stem}")
        if archive_files(local, crate.stem) != archive_files(remote, crate.stem):
            raise ReleaseError(f"{crate.stem} already exists with different sources or metadata")
    checksum = wait_for(lambda: index_checksum(crate), 180)
    if checksum != version.checksum:
        raise ReleaseError(f"Index has not confirmed the checksum for {crate.stem}. Rerun after it updates.")


def upload(crate, local, token):
    for attempt in range(3):
        version = published_version(crate)
        if version is not None:
            confirm_publish(crate, version, local)
            print(f"Already published {crate.stem}")
            return time.monotonic() + max(0, version.created_at + INTERVAL - time.time())
        if index_checksum(crate) is not None:
            raise ReleaseError(f"{crate.stem} is in the index but missing from the API. Rerun later.")

        print(f"Publishing {crate.stem}", flush=True)
        result = subprocess.run(
            ["cargo", "publish", *CARGO_OPTIONS, "-p", crate.name],
            env={**os.environ, "CARGO_REGISTRY_TOKEN": token},
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
        )
        output = result.stdout.replace(token, "***")
        print(output, end="", flush=True)

        # Cargo can time out after the upload has already reached the registry.
        version = wait_for(lambda: published_version(crate), 60)
        if version is not None:
            next_upload = time.monotonic() + INTERVAL
            confirm_publish(crate, version, local)
            print(f"Published {crate.stem}")
            return next_upload
        rate_limited = re.search(r"\b(?:got|HTTP(?: status)?|status code)\s+429\b", output, re.I)
        if not rate_limited or attempt == 2:
            raise ReleaseError(f"Could not confirm {crate.stem} on crates.io. Rerun to check its status.")
        header = re.search(r"^\s*retry-after:\s*([^\r\n]+)", output, re.I | re.M)
        delay = max(INTERVAL, retry_after(header[1].strip()) if header else 0)
        print(f"crates.io rate limit. Retrying in {delay:.0f}s.", flush=True)
        time.sleep(delay)


def release(dry_run, token):
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
    ))
    crates = release_order(metadata)
    published = {crate: published_version(crate) for crate in crates}
    pending = [crate for crate in crates if published[crate] is None]
    if pending and not (dry_run or token):
        raise ReleaseError("Set CARGO_REGISTRY_TOKEN to publish to crates.io")

    selection = [arg for crate in crates for arg in ("-p", crate.name)]
    package = ["cargo", "package", *CARGO_OPTIONS, *selection]
    if dry_run:
        package.append("--allow-dirty")
    subprocess.run(package, check=True)
    package_dir = Path(metadata["target_directory"]) / "package"
    archives = {crate: (package_dir / f"{crate.stem}.crate").read_bytes() for crate in crates}

    for crate, existing in published.items():
        if existing is not None:
            confirm_publish(crate, existing, archives[crate])
            print(f"Already published {crate.stem}")

    if not pending:
        print(f"All {len(crates)} workspace crate versions are already published")
        return
    if dry_run:
        for crate in pending:
            print(f"Would publish {crate.stem}")
        return

    last_upload = max((item.created_at for item in published.values() if item is not None), default=0)
    next_upload = time.monotonic() + max(0, last_upload + INTERVAL - time.time())
    for crate in pending:
        delay = next_upload - time.monotonic()
        if delay > 0:
            print(f"Waiting {delay:.0f}s before {crate.stem}", flush=True)
            time.sleep(delay)
        next_upload = upload(crate, archives[crate], token)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dry-run", action="store_true", help="Build packages and print the publish order")
    args = parser.parse_args()
    token = os.environ.pop("CARGO_REGISTRY_TOKEN", "")
    os.chdir(Path(__file__).resolve().parent.parent)
    try:
        release(args.dry_run, token)
    except (ReleaseError, OSError, KeyError, ValueError, tarfile.TarError, subprocess.CalledProcessError) as error:
        print(f"Release stopped: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
