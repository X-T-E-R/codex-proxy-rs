# OpenAI harness worker notices

This directory is the Provider-owned Node worker. It executes the pinned upstream
sources in place and does not vendor their trees.

| Source | Commit | License | Copyright |
| --- | --- | --- | --- |
| OpenCode `packages/ai`, `packages/schema` | `ab4d1f3ab1a05278b4d0392124b9e7f1c8131f17` | MIT | Copyright (c) 2025 opencode |
| pi `packages/ai`, `packages/telemetry` | `f9daca3480732d0042b1df3a6e27221f597a620d` | MIT | Copyright (c) 2025 Mario Zechner |

Runtime packages installed from `package-lock.json`:

| Package | Version | License |
| --- | --- | --- |
| `effect` | 4.0.1 | MIT, Copyright (c) 2023 Effectful Technologies Inc |
| `openai` | 7.19.0 | Apache-2.0, Copyright OpenAI |
| `partial-json` | 0.1.7 | MIT, Copyright Muspi Merol |

Node.js >= 22.19.0 is required. Install the locked packages with
`npm ci --ignore-scripts --prefix backend/crates/providers/openai/harness-worker`.
The Docker `runtime` and `runtime-prebuilt` images do not include Node.js or this
worker. Full license texts for the installed packages are in those packages after
installation. The MIT grants above require the copyright and permission notice to
travel with substantial portions of the corresponding software.

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
