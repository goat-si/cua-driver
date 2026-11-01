# Third-party notices

Parts of the Cua Driver Rust workspace are derived from, or embed, these
MIT-licensed projects. Each project's copyright notice is below, followed by the MIT
License text they share.

trope-cua's cursor motion is itself a port of Cua Driver's Swift cursor
implementation (`CursorMotionPath.swift`, `Bezier.swift` and the Dubins glide in
`AgentCursorRenderer.swift`, April 2026; MIT, Copyright (c) 2025 Cua AI, Inc.).
In `cursor-overlay`, the motion knobs, their defaults, the arc formula and the
Dubins glide originate in that Swift code; trope-cua is credited below only for
the items listed.

| Project                                                         | Copyright                                                                                                                              | Used in                                                                                                                                                                                                                                                                                                                                                                                                  | What                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| --------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [trope-cua](https://github.com/voctory/trope-cua)               | Copyright (c) 2026 Victor Vannara                                                                                                      | `crates/platform-windows` (`src/overlay.rs`, `src/input/`, `src/tools/impl_.rs`); `crates/cursor-overlay/src/motion.rs` (specific items only)                                                                                                                                                                                                                                                            | Windows cursor overlay, background input and layered pixel-click dispatch; in `cursor-overlay`, the `press_duration_ms` knob, the 80 ms dwell default, the former 20 s idle default and the `with_overrides` method shape                                                                                                                                                                                                                           |
| [Interface-Agent](https://github.com/francedot/Interface-Agent) | Copyright (c) 2024 Francesco Bonacci                                                                                                   | `crates/platform-windows` (`src/uia/`, `src/win32/`, `src/tools/impl_.rs`)                                                                                                                                                                                                                                                                                                                               | Windows UI Automation tree walk, app and window enumeration, InvokePattern click and ValuePattern set-value                                                                                                                                                                                                                                                                                                                                         |
| [yabai](https://github.com/koekeishiya/yabai)                   | Copyright (c) 2019 Åsmund Vikane                                                                                                       | `crates/platform-macos/src/input/skylight.rs` (`activate_without_raise`)                                                                                                                                                                                                                                                                                                                                 | The focus-without-raise SkyLight event-record sequence                                                                                                                                                                                                                                                                                                                                                                                              |
| [QuickJS-ng](https://github.com/quickjs-ng/quickjs)             | Copyright (c) 2017-2026 Fabrice Bellard; (c) 2017-2024 Charlie Gordon; (c) 2023-2026 Ben Noordhuis; (c) 2023-2026 Saúl Ibarra Corretgé | `crates/cua-driver-core/src/script_tool/` (compiled in through rquickjs-sys)                                                                                                                                                                                                                                                                                                                             | The JavaScript engine behind the experimental `run_script` tool                                                                                                                                                                                                                                                                                                                                                                                     |
| [rquickjs](https://github.com/DelSkayn/rquickjs)                | Copyright (c) 2020 Mees Delzenne; (c) 2025 Rquickjs Contributors                                                                       | `crates/cua-driver-core/src/script_tool/`                                                                                                                                                                                                                                                                                                                                                                | Rust bindings to QuickJS-ng                                                                                                                                                                                                                                                                                                                                                                                                                         |
| [T3 Code](https://github.com/pingdotgg/t3code)                  | Copyright (c) 2026 T3 Tools Inc.                                                                                                       | `crates/cua-driver-sdk/src/embedded.rs` (`allowed_environment_name`); `crates/platform-linux/src/wayland/shell_helper.rs` and `shell_helper/transport.rs`; `crates/platform-linux/src/wayland/libei.rs` (input acknowledgement); `crates/platform-linux/src/wayland/mod.rs` and `src/tools/impl_.rs` (GNOME window capture); `../wayland-helper/winrects@cua/` (`CaptureWindow`, `CaptureWindowPreview`) | Linux fixes ported from T3 Code's patch to Cua Driver 0.34.0 ([pingdotgg/t3code#16975](https://github.com/pingdotgg/t3code/pull/16975), `patches/cua-driver-linux/0.34.0.patch`): the `CUA_DRIVER_RS_ENABLE_WAYLAND` embedded-host pass-through, the persistent session-bus transport for the GNOME helper, waiting for EIS to acknowledge libei input, and per-window GNOME capture with compositor-side preview downscaling (helper API 9 and 10) |
| virtual-keyboard-unstable-v1 Wayland protocol                   | Copyright (c) 2008-2011 Kristian Høgsberg; (c) 2010-2013 Intel Corporation; (c) 2012-2013 Collabora, Ltd.; (c) 2018 Purism SPC         | `crates/platform-linux` (`protocol/virtual-keyboard-unstable-v1.xml`, bindings generated in `src/wayland/virtual_keyboard.rs`)                                                                                                                                                                                                                                                                           | The protocol definition the Linux virtual-keyboard input bindings are generated from                                                                                                                                                                                                                                                                                                                                                                |

## MIT License

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
## BSD-3-Clause: hyprland-toplevel-export-v1

`crates/platform-linux` generates its Hyprland window-capture bindings
(`src/wayland/hyprland_capture.rs`) from the hyprland-toplevel-export-v1 protocol
(`protocols/hyprland-toplevel-export-v1.xml`, from
[hyprland-protocols](https://github.com/hyprwm/hyprland-protocols)).

Copyright © 2022 Vaxry
All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this
   list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

3. Neither the name of the copyright holder nor the names of its
   contributors may be used to endorse or promote products derived from
   this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

## SIL Open Font License 1.1: Inter

`crates/cursor-overlay` embeds the [Inter](https://github.com/rsms/inter) typeface
(`assets/Inter.ttf`, version 4.001) to draw the cursor's session badge, so it is part of every
driver binary and SDK library. Its license follows.

Copyright 2016 The Inter Project Authors (https://github.com/rsms/inter)

This Font Software is licensed under the SIL Open Font License, Version 1.1.
This license is copied below, and is also available with a FAQ at:
https://scripts.sil.org/OFL


-----------------------------------------------------------
SIL OPEN FONT LICENSE Version 1.1 - 26 February 2007
-----------------------------------------------------------

PREAMBLE
The goals of the Open Font License (OFL) are to stimulate worldwide
development of collaborative font projects, to support the font creation
efforts of academic and linguistic communities, and to provide a free and
open framework in which fonts may be shared and improved in partnership
with others.

The OFL allows the licensed fonts to be used, studied, modified and
redistributed freely as long as they are not sold by themselves. The
fonts, including any derivative works, can be bundled, embedded,
redistributed and/or sold with any software provided that any reserved
names are not used by derivative works. The fonts and derivatives,
however, cannot be released under any other type of license. The
requirement for fonts to remain under this license does not apply
to any document created using the fonts or their derivatives.

DEFINITIONS
"Font Software" refers to the set of files released by the Copyright
Holder(s) under this license and clearly marked as such. This may
include source files, build scripts and documentation.

"Reserved Font Name" refers to any names specified as such after the
copyright statement(s).

"Original Version" refers to the collection of Font Software components as
distributed by the Copyright Holder(s).

"Modified Version" refers to any derivative made by adding to, deleting,
or substituting -- in part or in whole -- any of the components of the
Original Version, by changing formats or by porting the Font Software to a
new environment.

"Author" refers to any designer, engineer, programmer, technical
writer or other person who contributed to the Font Software.

PERMISSION & CONDITIONS
Permission is hereby granted, free of charge, to any person obtaining
a copy of the Font Software, to use, study, copy, merge, embed, modify,
redistribute, and sell modified and unmodified copies of the Font
Software, subject to the following conditions:

1) Neither the Font Software nor any of its individual components,
in Original or Modified Versions, may be sold by itself.

2) Original or Modified Versions of the Font Software may be bundled,
redistributed and/or sold with any software, provided that each copy
contains the above copyright notice and this license. These can be
included either as stand-alone text files, human-readable headers or
in the appropriate machine-readable metadata fields within text or
binary files as long as those fields can be easily viewed by the user.

3) No Modified Version of the Font Software may use the Reserved Font
Name(s) unless explicit written permission is granted by the corresponding
Copyright Holder. This restriction only applies to the primary font name as
presented to the users.

4) The name(s) of the Copyright Holder(s) or the Author(s) of the Font
Software shall not be used to promote, endorse or advertise any
Modified Version, except to acknowledge the contribution(s) of the
Copyright Holder(s) and the Author(s) or with their explicit written
permission.

5) The Font Software, modified or unmodified, in part or in whole,
must be distributed entirely under this license, and must not be
distributed under any other license. The requirement for fonts to
remain under this license does not apply to any document created
using the Font Software.

TERMINATION
This license becomes null and void if any of the above conditions are
not met.

DISCLAIMER
THE FONT SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO ANY WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT
OF COPYRIGHT, PATENT, TRADEMARK, OR OTHER RIGHT. IN NO EVENT SHALL THE
COPYRIGHT HOLDER BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
INCLUDING ANY GENERAL, SPECIAL, INDIRECT, INCIDENTAL, OR CONSEQUENTIAL
DAMAGES, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
FROM, OUT OF THE USE OR INABILITY TO USE THE FONT SOFTWARE OR FROM
OTHER DEALINGS IN THE FONT SOFTWARE.

## MPL-2.0 components and their source

These components are licensed under the [Mozilla Public License 2.0](https://www.mozilla.org/MPL/2.0/)
and are compiled into the release binaries, unmodified unless noted. Their source code is
available at the locations below. goat/notices/check_notices.py keeps this table in step with
the locked dependency graph.

| Component | Version | In | Source |
| --- | --- | --- | --- |
| uniffi | 0.31.0 | every driver binary and SDK library | <https://crates.io/crates/uniffi/0.31.0>, <https://github.com/mozilla/uniffi-rs> |
| uniffi_core | 0.31.0 | every driver binary and SDK library | <https://crates.io/crates/uniffi_core/0.31.0>, <https://github.com/mozilla/uniffi-rs> |
| uniffi_pipeline | 0.31.0 | every driver binary and SDK library | <https://crates.io/crates/uniffi_pipeline/0.31.0>, <https://github.com/mozilla/uniffi-rs> |
| option-ext | 0.2.0 | Linux driver binary and SDK library | <https://crates.io/crates/option-ext/0.2.0>, <https://github.com/soc/option-ext> |
| uniffi-runtime-core, uniffi-runtime-napi | uniffi-bindgen-react-native 0.31.0-3 | `cua_driver_node_runtime.node` (modified, see below) | <https://www.npmjs.com/package/uniffi-bindgen-react-native/v/0.31.0-3> (`runtimes/core`, `runtimes/napi`), <https://github.com/jhugman/uniffi-bindgen-react-native> |

`cua_driver_node_runtime.node` is built from uniffi-bindgen-react-native's N-API runtime with the
changes `scripts/build-node-runtime.mjs` makes, which copy RustBuffers into JavaScript-owned
arrays so the runtime loads in Electron. The modified source is that script, from this
repository at the release tag that matches these binaries, applied to the package above.

## Hyprland plugin

The Hyprland plugin (`hyprland-plugin/`) is built from source and is not part of the release
archives. It credits:

- **Omarchy** (MIT, Copyright (c) David Heinemeier Hansson): the chord comparison in
  `src/keymap_equivalence.hpp` is adapted from Spencer Bull's `independent-keymaps.patch` in
  [omacom/omarchy-pkgs#473](https://github.com/omacom/omarchy-pkgs/pull/473). The MIT License
  text is above.
- **Dillon DuPont**: the independent-seat design in `src/input_experiment.cpp` is adapted from his
  Hyprland prototype.

