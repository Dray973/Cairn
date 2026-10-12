# Third-party notices

This file holds the notices of third-party material that is part of Cairn's source code. Cairn's own licence is in [LICENSE](LICENSE). The licences of the runtime components the installer ships with Cairn, such as Python, Tcl/Tk, the Python packages and the Rust crates, are in the `licenses` folder of the installed app.

## WinUtil application list

The built-in app list of Updates › Install apps (`DEFAULT_APPS` and `HELD_APPS` in `crates/core/src/updates/apps.rs`) started from the winget package ids of WinUtil's application list, `config/applications.json` of <https://github.com/ChrisTitusTech/winutil>. The names, descriptions and notes of Cairn's list are Cairn's own; [docs/app-catalog.md](docs/app-catalog.md) gives Cairn's verdict on every app of WinUtil's list.

WinUtil is published under the following licence:

```text
MIT License

Copyright (c) 2022 CT Tech Group LLC

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
