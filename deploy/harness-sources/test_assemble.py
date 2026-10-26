"""固定源码与封装漂移必须被打包脚本拒绝"""

import io
from pathlib import Path
import tarfile
import tempfile
import unittest

import assemble


class SnapshotTests(unittest.TestCase):
    def test_snapshot_matches_pinned_files(self):
        assemble.verify()

    def test_rejects_drifted_source(self):
        original = assemble.SNAPSHOT
        with tempfile.TemporaryDirectory() as temp:
            changed = Path(temp) / "drift.tar.gz"
            with tarfile.open(original, "r:gz") as source, tarfile.open(changed, "w:gz") as target:
                for member in source:
                    data = source.extractfile(member).read()
                    if member.name == "opencode/packages/core/src/plugin/provider/chatgpt.ts":
                        data += b"\n// drift\n"
                        member.size = len(data)
                    target.addfile(member, io.BytesIO(data))
            try:
                assemble.SNAPSHOT = changed
                with self.assertRaisesRegex(AssertionError, "pinned source drift"):
                    assemble.verify()
            finally:
                assemble.SNAPSHOT = original

    def test_rejects_bundle_drift(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "openai-harness"
            assemble.verify(root / "sources")
            import json
            dependencies = json.loads((assemble.WORKER / "package.json").read_text(encoding="utf-8"))["dependencies"]
            worker = root / "worker"
            worker.mkdir()
            (worker / "package.json").write_text(json.dumps({"dependencies": dependencies}))
            (worker / "package-lock.json").write_text(json.dumps({"packages": {"": {"dependencies": dependencies}}}))
            for package, version in dependencies.items():
                installed = worker / "node_modules" / package / "package.json"
                installed.parent.mkdir(parents=True)
                installed.write_text(json.dumps({"version": version}))
            assemble.seal_bundle(root)
            assemble.verify_bundle(root)
            (root / "sources/opencode/packages/core/src/plugin/provider/chatgpt.ts").write_text("changed")
            with self.assertRaisesRegex(AssertionError, "bundle file drift"):
                assemble.verify_bundle(root)


if __name__ == "__main__":
    unittest.main()
