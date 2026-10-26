# OpenAI harness worker notices

This directory is the Provider-owned Node worker. Runtime bundles include exactly the
pinned source files it executes, plus the required upstream license texts. The source
snapshot is checked against `source-manifest.json` and the OAuth source hashes in
`deploy/harness-sources/assemble.py`.

| Source | Commit | License | Copyright |
| --- | --- | --- | --- |
| OpenCode `packages/ai`, `packages/schema`, `packages/core/src/plugin/provider/chatgpt.ts` | `ab4d1f3ab1a05278b4d0392124b9e7f1c8131f17` | MIT | Copyright (c) 2025 opencode |
| pi `packages/ai`, `packages/telemetry` | `f9daca3480732d0042b1df3a6e27221f597a620d` | MIT | Copyright (c) 2025 Mario Zechner |

Runtime packages installed from `package-lock.json`:

| Package | Version | License |
| --- | --- | --- |
| `effect` | 4.0.1 | MIT, Copyright (c) 2023 Effectful Technologies Inc |
| `jose` | 6.0.11 | MIT, Copyright (c) 2018 Filip Skokan |
| `openai` | 7.19.0 | Apache-2.0, Copyright OpenAI |
| `partial-json` | 0.1.7 | MIT, Copyright Muspi Merol |

Node.js >= 22.19.0 is required. Official runtime images and release archives include
Node.js 26.12.0, the worker with packages installed by `npm ci --ignore-scripts
--omit=dev`, and a pinned OpenCode/pi source snapshot. `node-LICENSE` travels with
the Node executable; upstream `LICENSE` files travel under `sources/opencode` and
`sources/pi`. The installed packages retain their own license files in `node_modules`.
For local development, install the locked packages with
`npm ci --ignore-scripts --prefix backend/crates/providers/openai/harness-worker`.
The MIT grants above require the copyright and permission notice to travel with
substantial portions of the corresponding software.

```
MIT License

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
