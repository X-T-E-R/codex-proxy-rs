#!/usr/bin/env python3
"""下载并校验归档平台对应的固定 Node.js 官方可执行文件"""

import argparse
import hashlib
import io
from pathlib import Path
import tarfile
from urllib.request import urlopen

VERSION = "24.18.0"  # 与 deploy/Dockerfile 的 node-runtime 阶段版本一致
TARGETS = {
    ("linux", "amd64"): ("linux-x64", "55aa7153f9d88f28d765fcdad5ae6945b5c0f98a36881703817e4c450fa76742"),
    ("linux", "arm64"): ("linux-arm64", "58c9520501f6ae2b52d5b210444e24b9d0c029a58c5011b797bc1fe7105886f6"),
    ("darwin", "arm64"): ("darwin-arm64", "4477b9f78efb77744cf5eb57a0e9594dba66466b38b4e93fa9f35cb907a095a6"),
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
