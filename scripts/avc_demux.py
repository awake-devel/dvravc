#!/usr/bin/env python3
"""Demux a 4-channel DVR ".avc" recording into per-channel MP4s + a 2x2 mosaic.

Container layout (reverse-engineered):
  file header: "VA" ... then "CHn.Title=NAME\n" lines
  frame header (little endian):
    0  ff 3d 01 e0 (video) | ff bd 01 e0 (audio)
    4  u32 sequence
    8  u32 payload size
    12 u16 width, u16 height
    16 u32 timestamp (ms)
    20 u16 channel (1..4 video, 0 audio)
  video: payload at +46 (after an 8-byte "VA" sub-header), raw H.264 Annex B
  audio: payload at +62, G.711 mu-law, 8 kHz mono
"""
import re
import struct
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

src = Path(sys.argv[1])
out = Path(sys.argv[2]) if len(sys.argv) > 2 else Path.cwd() / "output"
out.mkdir(parents=True, exist_ok=True)
d = src.read_bytes()

titles = dict(re.findall(rb"CH(\d)\.Title=([^\n]*)\n", d[:0x200]))
titles = {int(k): v.decode(errors="replace") for k, v in titles.items()}

video = defaultdict(list)  # ch -> [(ts, bytes)]
audio = []
o = d.find(b"\x01\xe0") - 2
while 0 <= o and o + 46 <= len(d):
    if d[o] != 0xFF or d[o + 1] not in (0x3D, 0xBD) or d[o + 2:o + 4] != b"\x01\xe0":
        n = d.find(b"\x01\xe0", o + 1)
        o = n - 2 if n >= 0 else -1
        continue
    size, = struct.unpack_from("<I", d, o + 8)
    ts, ch = struct.unpack_from("<IH", d, o + 16)
    if d[o + 1] == 0x3D:
        video[ch].append((ts, d[o + 46:o + 46 + size]))
        o += 46 + size
    else:
        audio.append(d[o + 62:o + 62 + size])
        o += 62 + size

all_ts = [ts for frames in video.values() for ts, _ in frames]
t0 = min(all_ts)
dur = (max(all_ts) - t0) / 1000

(out / "audio.ulaw").write_bytes(b"".join(audio))
outputs = []
for ch in sorted(video):
    frames = video[ch]
    name = f"CH{ch}_{titles.get(ch, '')}".rstrip("_")
    h264 = out / f"{name}.h264"
    h264.write_bytes(b"".join(f for _, f in frames))
    # Per-frame timestamps via concat demuxer would be overkill; frame rate is steady.
    span = (frames[-1][0] - frames[0][0]) / 1000
    fps = (len(frames) - 1) / span if span else 3
    mp4 = out / f"{name}.mp4"
    subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-r", f"{fps:.4f}", "-i", str(h264),
                    "-itsoffset", f"{-(frames[0][0] - t0) / 1000:.3f}",
                    "-f", "mulaw", "-ar", "8000", "-ac", "1", "-i", str(out / "audio.ulaw"),
                    "-map", "0:v", "-map", "1:a", "-c:v", "libx264", "-pix_fmt", "yuv420p",
                    "-c:a", "aac", "-shortest", str(mp4)], check=True)
    outputs.append((ch, mp4, frames[0][0] - t0))
    print(f"{mp4.name}: {len(frames)} frames, {fps:.2f} fps, starts +{(frames[0][0] - t0) / 1000:.1f}s")

# 2x2 mosaic aligned on wall-clock start, with channel titles
inputs, filt = [], []
for i, (ch, mp4, off) in enumerate(outputs):
    inputs += ["-itsoffset", f"{off / 1000:.3f}", "-i", str(mp4)]
    label = titles.get(ch, f"CH{ch}").replace(":", r"\:")
    filt.append(f"[{i}:v]tpad=start_mode=clone:start_duration={off / 1000:.3f}:stop_mode=clone:stop_duration=60,"
                f"drawtext=text='CH{ch} {label}':x=8:y=8:fontsize=18:fontcolor=white:box=1:boxcolor=black@0.5[v{i}]")
n = len(outputs)
layout = "|".join(["0_0", "w0_0", "0_h0", "w0_h0"][:n])
filt.append("".join(f"[v{i}]" for i in range(n)) + f"xstack=inputs={n}:layout={layout}[v]")
subprocess.run(["ffmpeg", "-y", "-loglevel", "error", *inputs,
                "-f", "mulaw", "-ar", "8000", "-ac", "1", "-i", str(out / "audio.ulaw"),
                "-filter_complex", ";".join(filt), "-map", "[v]", "-map", f"{n}:a",
                "-c:v", "libx264", "-r", "10", "-pix_fmt", "yuv420p", "-c:a", "aac",
                "-t", f"{dur:.3f}", str(out / "mosaic.mp4")], check=True)
print(f"mosaic.mp4: {dur:.1f}s")
