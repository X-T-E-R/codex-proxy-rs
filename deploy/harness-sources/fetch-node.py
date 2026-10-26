#!/usr/bin/env python3
"""下载并校验归档平台对应的固定 Node.js 官方可执行文件"""

import argparse
import hashlib
import io
from pathlib import Path
import tarfile
from urllib.request import urlopen

VERSION = "26.12.0"  # 与 deploy/Dockerfile 的 node-runtime 阶段版本一致
TARGETS = {
    ("linux", "amd64"): ("linux-x64", "fd47865ecd6391ebb431fc815e01b9135bf83ded5cf373247e9331e196b7aded"),
    ("linux", "arm64"): ("linux-arm64", "fd6cc729fa4448c2ac27c3fca5606019ced63c65a15c152c8df03e0cbce7f4c5"),
    ("darwin", "arm64"): ("darwin-arm64", "a2e255e35347f727f29aa158c552a60140b17b6ba68719d5d6fc46c7d058aa75"),
}

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("os", choices=["linux", "darwin"])
parser.add_argument("arch", choices=["amd64", "arm64"])
parser.add_argument("output", type=Path, help="new output directory")
args = parser.parse_args()
target, expected_hash = TARGETS[(args.os, args.arch)]
base = f"https://nodejs.org/dist/v{VERSION}/"
filename = f"node-v{VERSION}-{target}.tar.xz"
with urlopen(base + "SHASUMS256.txt", timeout=60) as response:
    checksums = dict(line.split(maxsplit=1)[::-1] for line in response.read().decode().splitlines() if len(line.split()) == 2)
assert checksums[filename] == expected_hash, f"Node.js publisher checksum drift: {filename}"
with urlopen(base + filename, timeout=120) as response:
    data = response.read()
assert hashlib.sha256(data).hexdigest() == expected_hash, f"Node.js archive digest mismatch: {filename}"
assert not args.output.exists(), "Node output directory already exists"
args.output.mkdir(parents=True)
with tarfile.open(fileobj=io.BytesIO(data), mode="r:xz") as archive:
    for suffix, destination in [("bin/node", "node"), ("LICENSE", "node-LICENSE")]:
        member = archive.getmember(f"node-v{VERSION}-{target}/{suffix}")
        assert member.isfile(), f"missing {suffix}"
        path = args.output / destination
        path.write_bytes(archive.extractfile(member).read())
        path.chmod(0o755 if destination == "node" else 0o644)
print(f"Installed Node.js v{VERSION} for {args.os}-{args.arch}: {filename}")
