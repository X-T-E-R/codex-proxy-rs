#!/usr/bin/env python3
"""生成并验证可离线部署的固定 OpenAI harness 源码快照"""

import argparse
import gzip
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import shutil
import subprocess
import tarfile

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
WORKER = ROOT / "backend/crates/providers/openai/harness-worker"
SNAPSHOT = HERE / "snapshot.tar.gz"
NODE_VERSION = "26.12.0"
COMMITS = {
    "opencode": "ab4d1f3ab1a05278b4d0392124b9e7f1c8131f17",
    "pi": "f9daca3480732d0042b1df3a6e27221f597a620d",
}
# OAuth 在 Responses 清单之外增加自身上游源码，数据面仍独占 source-manifest.json
OAUTH = {
    ("opencode", "LICENSE"): "625f0f619133f89bbbb2abe37369613dfa1885eba1e50d02170deb62bb42cb6b",
    ("opencode", "packages/core/src/plugin/provider/chatgpt.ts"): "27f84a6177a6e01b5a1fc15dbcd39510a1d239779034533a548162e979425064",
    ("pi", "LICENSE"): "0457f5bcec3b3b211605dfb5d1a49042fd638f3686a410fe099c24a25af13c48",
    ("pi", "packages/ai/src/auth/oauth/openai-chatgpt.ts"): "ad9dbdd7ffa52fcbaff1d1be2d0ba97b643476aaf8930e6dd67147bf35c837de",
    ("pi", "packages/ai/src/auth/oauth/pkce.ts"): "33835111577490103dbb7509b7687db1643691d7b425845428f3cbeb873fe9cf",
    ("pi", "packages/ai/src/utils/oauth-page.ts"): "a5c0f7ddd7a1a803c8fbaf65adaffb6a7710a70a75b5559a749ba811942fab40",
}


def expected_files():
    manifest = json.loads((WORKER / "source-manifest.json").read_text(encoding="utf-8"))
    for root, commit in COMMITS.items():
        assert manifest[f"{root}_commit"] == commit, f"{root} commit drift"
    entries = {(entry["root"], entry["path"]): entry["sha256"] for entry in manifest["files"]}
    assert len(entries) == len(manifest["files"]) == 158, "source manifest duplicates or changed without snapshot review"
    assert not (entries.keys() & OAUTH.keys()), "OAuth source duplicate"
    entries.update(OAUTH)
    for (root, relative), sha in entries.items():
        path = PurePosixPath(relative)
        assert root in COMMITS and not path.is_absolute() and ".." not in path.parts
        assert len(sha) == 64 and all(c in "0123456789abcdef" for c in sha)
    return entries


def verified_bytes(entries, checkouts):
    for (root, relative), sha in sorted(entries.items()):
        source = checkouts[root] / relative
        assert source.is_file() and not source.is_symlink(), f"missing or linked source: {source}"
        data = source.read_bytes()
        assert hashlib.sha256(data).hexdigest() == sha, f"pinned source drift: {root}/{relative}"
        yield f"{root}/{relative}", data


def create(checkouts):
    entries = expected_files()
    for root, checkout in checkouts.items():
        commit = subprocess.check_output(["git", "-C", str(checkout), "rev-parse", "HEAD"], text=True).strip()
        assert commit == COMMITS[root], f"{root} checkout commit drift: {commit}"
    with SNAPSHOT.open("wb") as output, gzip.GzipFile(filename="", fileobj=output, mode="wb", mtime=0, compresslevel=9) as compressed:
        with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as archive:
            for name, data in verified_bytes(entries, checkouts):
                info = tarfile.TarInfo(name)
                info.size = len(data)
                info.mode = 0o644
                info.mtime = 0
                archive.addfile(info, io.BytesIO(data))
    verify()


def verify(destination=None):
    entries = expected_files()
    seen = set()
    with tarfile.open(SNAPSHOT, "r:gz") as archive:
        for member in archive:
            path = PurePosixPath(member.name)
            assert member.isfile() and len(path.parts) > 1 and ".." not in path.parts, f"unsafe source: {member.name}"
            key = (path.parts[0], str(PurePosixPath(*path.parts[1:])))
            assert key in entries and key not in seen, f"unexpected or duplicate source: {member.name}"
            seen.add(key)
            data = archive.extractfile(member).read()
            assert hashlib.sha256(data).hexdigest() == entries[key], f"pinned source drift: {member.name}"
            if destination:
                target = destination / member.name
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(data)
    assert seen == entries.keys(), f"missing pinned sources: {entries.keys() - seen}"
    print(f"Verified {len(seen)} pinned OpenCode/pi source and license files")


def bundle_files(destination):
    manifest_path = destination / "bundle-manifest.json"
    for file in sorted(destination.rglob("*")):
        if file == manifest_path or file.is_dir():
            continue
        assert file.is_file() and not file.is_symlink(), f"linked or invalid bundle file: {file}"
        yield {"path": file.relative_to(destination).as_posix(), "sha256": hashlib.sha256(file.read_bytes()).hexdigest()}


def seal_bundle(destination):
    entries = list(bundle_files(destination))
    assert (destination / "worker/node_modules/effect/package.json").is_file(), "missing locked worker dependencies"
    assert (destination / "worker/node_modules/openai/package.json").is_file(), "missing locked worker dependencies"
    assert (destination / "worker/node_modules/partial-json/package.json").is_file(), "missing locked worker dependencies"
    pinned = expected_files()
    actual = {(entry["path"].split("/", 2)[1], entry["path"].split("/", 2)[2]): entry["sha256"]
              for entry in entries if entry["path"].startswith("sources/")}
    assert actual == pinned, "assembled source files drifted"
    manifest = {
        "schema_version": 1,
        "node_version": NODE_VERSION,
        "opencode_commit": COMMITS["opencode"],
        "pi_commit": COMMITS["pi"],
        "files": entries,
    }
    (destination / "bundle-manifest.json").write_text(json.dumps(manifest, separators=(",", ":")) + "\n", encoding="utf-8")
    print(f"Sealed harness bundle with {len(entries)} files")


def verify_bundle(destination):
    manifest = json.loads((destination / "bundle-manifest.json").read_text(encoding="utf-8"))
    assert manifest["schema_version"] == 1 and manifest["node_version"] == NODE_VERSION
    assert all(manifest[f"{name}_commit"] == commit for name, commit in COMMITS.items())
    assert manifest["files"] == list(bundle_files(destination)), "harness bundle file drift"
    print(f"Verified harness bundle with {len(manifest['files'])} files")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["snapshot", "verify", "assemble", "seal", "verify-bundle"])
    parser.add_argument("--opencode", type=Path, help="fixed OpenCode checkout, snapshot command only")
    parser.add_argument("--pi", type=Path, help="fixed pi checkout, snapshot command only")
    parser.add_argument("--output", type=Path, help="output openai-harness directory, assemble command only")
    args = parser.parse_args()
    if args.command == "snapshot":
        assert args.opencode and args.pi, "snapshot requires both upstream checkouts"
        create({"opencode": args.opencode, "pi": args.pi})
    elif args.command == "verify":
        verify()
    elif args.command == "assemble":
        assert args.output and not args.output.exists(), "assemble requires a new output directory"
        target = args.output
        target.mkdir(parents=True)
        verify(target / "sources")
        shutil.copytree(WORKER, target / "worker", ignore=shutil.ignore_patterns("node_modules"))
    elif args.command == "seal":
        seal_bundle(args.output)
    else:
        verify_bundle(args.output)
