# Third-party notices

scryglass is MIT licensed, but binaries bundle work from other projects.
This file collects the notices that travel with a release build. The full
machine-readable inventory of Rust dependencies is available from the
source tree with `cargo deny list`.

## Statically linked C and C++ libraries

Prebuilt release binaries embed the following libraries, built through
vcpkg with the exact configuration pinned in
`.github/workflows/release.yml`:

- **FFmpeg** (libavcodec, libavformat, libavutil, libswscale,
  libswresample): LGPL-2.1-or-later. Built with the default LGPL
  configuration. No GPL components (such as x264 or x265) are enabled.
  Source for the exact version is available through the pinned vcpkg
  revision, and the relinking terms of the LGPL are satisfied by this
  application being open source under MIT.
- **libheif**: LGPL-3.0-or-later. Built with the `core` feature set only.
- **libde265** (HEVC decoding for HEIF): LGPL-3.0-or-later. Decode only.
  The x265 encoder is not present in distributed binaries.
- **dav1d** (AV1 decoding for AVIF stills and AV1 video, linked into
  FFmpeg): BSD-2-Clause.
- **UnRAR** (RAR extraction, vendored by the `unrar-ng` crate): freeware
  license, reproduced verbatim below as its terms require.

## Notable Rust dependencies

- **rawler** (camera RAW metadata and embedded previews): LGPL-2.1.
  Statically linked like all Rust crates. Source availability and the
  open MIT licensing of this application satisfy its relink terms.
- The remaining roughly 600 crates in the dependency tree are licensed
  under MIT, Apache-2.0, BSD, ISC, Zlib, Unicode-3.0, MPL-2.0, BSL-1.0,
  CC0, or Unlicense terms. Run `cargo deny list` in the repository for
  the complete per-crate breakdown.

## UnRAR license

```
      The source code of UnRAR utility is freeware. This means:

   1. All copyrights to RAR and the utility UnRAR are exclusively
      owned by the author - Alexander Roshal.

   2. UnRAR source code may be used in any software to handle
      RAR archives without limitations free of charge, but cannot be
      used to develop RAR (WinRAR) compatible archiver and to
      re-create RAR compression algorithm, which is proprietary.
      Distribution of modified UnRAR source code in separate form
      or as a part of other software is permitted, provided that
      full text of this paragraph, starting from "UnRAR source code"
      words, is included in license, or in documentation if license
      is not available, and in source code comments of resulting package.

   3. The UnRAR utility may be freely distributed. It is allowed
      to distribute UnRAR inside of other software packages.

   4. THE RAR ARCHIVER AND THE UnRAR UTILITY ARE DISTRIBUTED "AS IS".
      NO WARRANTY OF ANY KIND IS EXPRESSED OR IMPLIED.  YOU USE AT
      YOUR OWN RISK. THE AUTHOR WILL NOT BE LIABLE FOR DATA LOSS,
      DAMAGES, LOSS OF PROFITS OR ANY OTHER KIND OF LOSS WHILE USING
      OR MISUSING THIS SOFTWARE.

   5. Installing and using the UnRAR utility signifies acceptance of
      these terms and conditions of the license.

   6. If you don't agree with terms of the license you must remove
      UnRAR files from your storage devices and cease to use the
      utility.

      Thank you for your interest in RAR and UnRAR.

                                            Alexander L. Roshal
```
