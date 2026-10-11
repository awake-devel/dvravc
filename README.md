# dvravc

Convert proprietary 4-channel DVR `.avc` surveillance recordings into standard MP4 files.

[![CI](https://github.com/awake-devel/dvravc/actions/workflows/ci.yml/badge.svg)](https://github.com/awake-devel/dvravc/actions/workflows/ci.yml)

Some stand-alone CCTV recorders export footage as a single `.avc` file that
interleaves four camera streams and one audio track in an undocumented
container. Ordinary players can't open it. `dvravc` demuxes the container and
uses `ffmpeg` to produce normal MP4s.

## Features

- Three front ends: a **CLI** for scripting, a **TUI** (terminal) and a **GUI** (desktop window).
- **One MP4 per camera channel**, named after the channel title stored in the file (e.g. `CH1_FRONTDOOR.mp4`).
- **2x2 mosaic** of all channels, aligned on the recording start and labelled with each camera's title.
- **Audio**: the G.711 μ-law track is muxed in as AAC, offset to match each channel's start.
- **Batch**: pass several files or whole folders; every `*.avc` inside is converted.
- Runs on Linux, macOS and Windows.

## Requirements

`ffmpeg` must be on your `PATH` (with `libx264`, which every common build has).

| OS      | Install ffmpeg                                                          |
|---------|-------------------------------------------------------------------------|
| Linux   | `sudo apt install ffmpeg` · `sudo dnf install ffmpeg` · `sudo pacman -S ffmpeg` |
| macOS   | `brew install ffmpeg`                                                   |
| Windows | `winget install Gyan.FFmpeg` or `choco install ffmpeg`                  |

If ffmpeg is missing, all three front ends tell you so and print these hints.

## Installation

### Prebuilt binaries

Download the binary for your OS from the
[Releases page](https://github.com/awake-devel/dvravc/releases). They are built by CI for every `v*` tag.

### With cargo

```sh
cargo install --git https://github.com/awake-devel/dvravc
```

### From source

You need Rust 1.88 or newer (edition 2024).

```sh
git clone https://github.com/awake-devel/dvravc
cd dvravc
cargo build --release
# binary: target/release/dvravc   (dvravc.exe on Windows)
```

System libraries for the GUI (eframe/egui) and file dialogs (rfd):

- **Debian/Ubuntu**: `sudo apt install build-essential pkg-config libgtk-3-dev libxkbcommon-dev libwayland-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev`
- **Fedora**: `sudo dnf install gcc pkgconf-pkg-config gtk3-devel libxkbcommon-devel wayland-devel libxcb-devel`
- **Arch**: `sudo pacman -S base-devel gtk3 libxkbcommon wayland libxcb`
- **macOS**: Xcode command line tools (`xcode-select --install`).
- **Windows**: the MSVC toolchain (Visual Studio Build Tools, "Desktop development with C++").

At run time on Linux the file dialogs go through the XDG desktop portal, so a
portal backend (`xdg-desktop-portal-gtk`, `-kde`, `-gnome`, `-hyprland`, …) should be installed.

## Usage

### `convert`

```sh
dvravc convert recording.avc                    # -> ./output/recording/
dvravc convert a.avc b.avc -o ~/Videos/cctv     # several files, custom output root
dvravc convert /media/usb/DVR/                  # every *.avc in a folder
dvravc convert recording.avc --no-mosaic        # only per-channel files
dvravc convert recording.avc --no-channels      # only the mosaic
```

### `info`

```text
$ dvravc info 20240101120000_SITE.avc
File:     20240101120000_SITE.avc
Start:    2024-01-01 11:59:47.120
End:      2024-01-01 12:00:00.004
Duration: 44.6s (from frame timestamps)
Audio:    352 block(s) of G.711 mu-law 8 kHz
CH1 FRONTDOOR    352x288    140 frames   3.12 fps  starts +0.0s
CH2 GARDEN       352x288    141 frames   3.22 fps  starts +0.9s
CH3 DRIVEWAY     352x288    113 frames   3.18 fps  starts +4.4s
CH4 GARAGE       352x288    136 frames   3.07 fps  starts +0.3s
```

### `tui`

```sh
dvravc tui              # lists the .avc files in the current directory
dvravc tui /media/usb   # or in another directory
```

| Key            | Action                                          |
|----------------|-------------------------------------------------|
| `↑` `↓` / `j` `k` | Move                                          |
| `Space`        | Select / unselect the current file              |
| `a`            | Select / unselect all                           |
| `c`            | Toggle per-channel output                       |
| `m`            | Toggle mosaic output                            |
| `o`            | Edit the output directory (Enter to confirm, Esc to cancel) |
| `Enter`        | Convert the selected files (or the current one) |
| `r`            | Rescan the directory                            |
| `q` / `Esc`    | Quit                                            |

Conversion runs in the background; progress appears in the log pane.

### `gui`

```sh
dvravc gui
```

Add files with **Add files…** or drag and drop `.avc` files (or folders) onto
the window, pick an output folder, choose the outputs and press **Convert**.

## Output layout

```text
output/
└── 20240101120000_SITE/        # one folder per input file (its file stem)
    ├── CH1_FRONTDOOR.mp4       # H.264 + AAC, native resolution (352x288)
    ├── CH2_GARDEN.mp4
    ├── CH3_DRIVEWAY.mp4
    ├── CH4_GARAGE.mp4
    └── mosaic.mp4              # 2x2 grid (704x576), 10 fps, labelled
```

Channel titles are sanitised (only letters, digits, space, `-` and `_` are
kept), so file names are valid on every OS.

## File format notes

Reverse-engineered from real recordings; all integers are little-endian.

**File header**

| Offset | Type            | Meaning                                            |
|--------|-----------------|----------------------------------------------------|
| 0x00   | `"VA"`          | magic                                              |
| 0x08   | 8 × u16         | Windows `SYSTEMTIME` (year, month, day-of-week, day, hour, min, sec, ms): recording start |
| 0x18   | 8 × u16         | second `SYSTEMTIME`, probably the end              |
| …      | text            | `CHn.Title=NAME\n` lines, one per camera           |

**Frame records** follow the header. Each starts with a 4-byte marker:
`ff 3d 01 e0` for video, `ff bd 01 e0` for audio.

| Offset | Type   | Meaning                                         |
|--------|--------|-------------------------------------------------|
| +0     | 4 B    | marker                                          |
| +4     | u32    | sequence number                                 |
| +8     | u32    | payload size                                    |
| +12    | u16    | width (e.g. 352)                                |
| +14    | u16    | height (e.g. 288)                               |
| +16    | u32    | timestamp, milliseconds                         |
| +20    | u16    | channel: 1–4 for video, 0 for audio             |
| +38    | 8 B    | video only: `"VA"` sub-header                   |
| +46    | …      | video payload: raw H.264 Annex B                |
| +62    | …      | audio payload: G.711 μ-law, 8 kHz mono (usually 1024 bytes) |

The next record starts right after the payload. If the bytes at the expected
position are not a valid marker, the parser resynchronises on the next
`01 e0`; a truncated final record is ignored.

## Compatible devices

The recorder that writes this format has **not been identified**. `dvravc`
was built from a single 4-channel PAL recorder (CIF 352×288, about 3 fps per
channel, recordings from 2015). `.avc` is a generic extension, so a matching
extension alone doesn't mean `dvravc` will read the file.

**Confirmed:** none yet.

**Unverified candidates.** These vendors use `.avc` for exports, but no public
source documents their internal layout, so they may be a different format:

- **AVTECH** (EagleEyes / AVTECH Video Player). Users report `.avc` downloads
  that only AVTECH's player opens
  ([CCTVForum](https://www.cctvforum.com/topic/19628-avtech-674-how-to-convert-avc-file/)),
  sometimes written as a `.avc` + `.grp` + `.idx` trio
  ([VideoHelp](https://forum.videohelp.com/threads/368428-convert-avc-to-mp4)).
  The `.grp` file is something this format doesn't have.
- **IDIS** is linked to `.avc` exports by a converter site, which describes
  them as raw H.264 with no header
  ([convert.guru](https://convert.guru/avc-converter)). That doesn't match the
  `VA` header described here.

**How to recognise the format:**

- Files come in pairs: `YYYYMMDDHHMMSS_<NAME>.avc` plus an `.idx` file with the
  same base name. Records in the `.idx` are 0x24 bytes each.
- The file starts with ASCII `VA`, and `CH1.Title=` … `CH4.Title=` lines
  appear near the top.
- Frame markers `ff 3d 01 e0` (video) and `ff bd 01 e0` (audio) follow.

```sh
hexdump -C recording.avc | head -n 20
# 00000000  56 41 ...                    |VA..............|   <- "VA" magic
# ...       43 48 31 2e 54 69 74 6c 65 3d |CH1.Title=......|   <- camera names
# ...       ff 3d 01 e0 ...              |.=..............|   <- first video record
```

You can also run `dvravc info recording.avc`. It prints the header, channels
and frame counts, or fails if the file isn't this format.

**Have one of these recorders?** Please
[open an issue](https://github.com/awake-devel/dvravc/issues/new) with the
brand and model (from the label or the setup menu) and whether `dvravc`
converted the files. Don't attach private footage. A hexdump of the first
few hundred bytes is enough. Reports like these are how this list will grow.

## Limitations

- **Frame rate is averaged.** Each channel is encoded at a constant rate
  derived from its first and last timestamps; jitter between individual
  frames is not preserved.
- **Tested with one DVR model only.** Other recorders using a similar `.avc`
  export may differ in header layout or record fields.
- **Brand unknown.** The recorder that produces this format has not been
  identified; the format notes above are inferred from sample files. See
  [Compatible devices](#compatible-devices).

## License

[MIT](LICENSE)
