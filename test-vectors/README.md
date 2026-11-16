# ATV test vectors

One known input, the ATV videos made from it, and the copies that **WhatsApp and YouTube actually returned**. Use them to check a new phone, a site's new compression, or a change to ATV against the same data, and to see how much damage a real copy carries.

Every file's SHA-256 is in [`SHA256SUMS`](SHA256SUMS).

## The input

`input/data-v3.bin`: 2,000,000 bytes of random data (from `/dev/urandom`), SHA-256 `5422137b73e5c378b885e59272b99cc6b5bcfa5e28584fa4915542495c66725e`. Random data does not compress, so every bit of it travels through the video.

## The originals

Made on 14 Nov 2026 with the ATV development build that introduced video format 3 (the format of release v0.1.0), using ffmpeg 8.1 (gyan.dev full build, libavcodec 62.28.100, libx264). None is encrypted or compressed, so anyone can restore them without a password.

| File | ATV setting | Command |
|---|---|---|
| `original/W3.mp4` | WhatsApp: 1080p, 8 × 8 blocks, strong repair, 10 fps, H.264 | `atv encode data-v3.bin -o W3.mp4 --res 1080p --mode robust --cell 8 --parity 64 --fps 10 --no-encrypt --no-compress` |
| `original/W3-30.mp4` | The same at 30 fps (a bitrate WhatsApp re-compresses) | same, with `--fps 30` |
| `original/Y3.mp4` | YouTube: 4K, 4 × 4 blocks, standard repair, H.264 | `atv encode data-v3.bin -o Y3.mp4 --res 4k --mode robust --cell 4 --parity 32 --no-encrypt --no-compress` |
| `original/Y3-AV1.mp4` | The same with AV1 | same, with `--av1` |

Re-running the W3 and Y3 commands with release **v0.1.0** and the same ffmpeg build on 16 Nov 2026 gave **byte-identical** files (same SHA-256). Other ffmpeg builds can produce different bytes with the same frames.

## The received copies

| File | How it was obtained |
|---|---|
| `received/whatsapp/W3.mp4` | `W3.mp4` sent from an Android phone in a WhatsApp chat, media upload quality "Standard", 14 Nov 2026; the file as received. At this bitrate (898 kbit/s) WhatsApp sent it untouched. |
| `received/whatsapp/W3-30.mp4` | `W3-30.mp4` sent the same way. WhatsApp re-compressed it to 854 × 480 H.264 at about 4.8 Mbit/s. |
| `received/youtube/Y3-*.mp4`, `Y3-*.webm` | `Y3.mp4` uploaded to YouTube on 14 Nov 2026; each copy YouTube made downloaded with yt-dlp by its format id: 136 (720p H.264), 137 (1080p H.264), 247 (720p VP9), 313 (2160p VP9). |
| `received/youtube/Y3-AV1-*` | The same for `Y3-AV1.mp4`. |

## Results with ATV v0.1.0 (checked 16 Nov 2026)

Every copy restores to `data-v3.bin` exactly (matching SHA-256). Wrong bits are counted by `atv measure` against the original, over the data frames (600 for W3, 45 for Y3).

| Received copy | Wrong bits, average | Worst frame | Restored |
|---|---|---|---|
| whatsapp/W3.mp4 | 0% | 0% | yes |
| whatsapp/W3-30.mp4 (480p re-compression) | 0.258% | 0.401% | yes |
| youtube/Y3-720p-h264.mp4 | 0.034% | 0.066% | yes |
| youtube/Y3-1080p-h264.mp4 | 0% | 0% | yes |
| youtube/Y3-720p-vp9.webm | 0.329% | 0.385% | yes |
| youtube/Y3-2160p-vp9.webm | 0% | 0% | yes |
| youtube/Y3-AV1-720p-h264.mp4 | 0.024% | 0.028% | yes |
| youtube/Y3-AV1-1080p-h264.mp4 | 0% | 0% | yes |
| youtube/Y3-AV1-720p-vp9.webm | 0.096% | 0.120% | yes |
| youtube/Y3-AV1-2160p-vp9.webm | 0% | 0% | yes |

The 720p and 1080p copies are smaller than the 4K original; ATV scales them back before reading.

## Check it yourself

```
sha256sum -c SHA256SUMS                                   # the files are the published ones
atv decode received/whatsapp/W3-30.mp4 -o restored        # restore a copy
sha256sum restored/data-v3.bin                            # 5422137b…725e
atv measure original/W3-30.mp4 received/whatsapp/W3-30.mp4   # wrong bits, worst frame, lost frames
```

To test a new phone or a changed site: send `original/W3.mp4` or `original/W3-30.mp4` (or upload `original/Y3.mp4`), save what comes back, and run the same three commands. Reports of new results are welcome as issues.
