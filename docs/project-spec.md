# ATV (Anything To Video) — project specification

This is the technical specification of ATV: the decisions behind it, the exact file format, and the measurements that back the technical choices. It was written alongside the code, and references such as "(spec 6.4)" in the source point to its sections. The build plan and the GUI design brief used during development are not published.

If you need to deviate from this spec, update this file in the same change and say why. Do not let the code and the spec drift apart.

## 1. What ATV does

ATV converts any file, folder or archive into a video and converts that video back into the original files.

- Each bit becomes a pixel (or a block of pixels): **black = 0, white = 1**.
- The data can be **encrypted** before it is drawn, so the video cannot be turned back into files without the key.
- Restored files are **bit-for-bit identical** to the originals, and the app proves it with a hash.

An input such as a zip file is treated as plain bytes. ATV does not need to understand any file type.

## 2. Decisions made with the maintainer

| Topic | Decision |
|---|---|
| Language | Rust |
| Resolutions | 1080p (1920 × 1080) and 4K (3840 × 2160) |
| Video modes | **Lossless** (1 pixel = 1 bit) and **Robust** (blocks plus error correction). Build Lossless first. |
| Encryption | Optional, on by default. Password, key file, or both together. |
| Video I/O | The `ffmpeg` and `ffprobe` programs, run as child processes. Not bundled in the app; the user installs them, or ATV downloads the official build on request (section 9.5b, maintainer's decision of 13 Nov 2026). A first-run setup wizard offers ffmpeg, yt-dlp and the JavaScript runtime together (maintainer's choice, 14 Nov 2026: in the app, not in the installer, so the installer stays small and the tools stay updatable). |
| Interface | Desktop GUI from the start. First built with **Slint**; after milestone 4 the maintainer chose **Tauri 2** instead (section 9). The engine must not depend on the GUI. |
| Platforms | macOS and Windows 11 |
| Look | Professional, glassy, macOS style. Light and dark following the system. System accent colour. |
| Layout | Left sidebar: Encode, Decode, History, Settings |
| Language of the UI | English only |
| Features in v1 | Drag and drop (files, several files, folders); live frame preview; verify after encode; optional compression before encoding |

Things added by the spec author that the maintainer has not explicitly confirmed (flag them if they cause trouble):

- A small command-line tool (`atv`) alongside the GUI, for testing and scripting. It is thin and costs little.
- Compression and verification are each a switch, both on by default.
- Robust mode offers two block sizes, 4 × 4 and 8 × 8.
- History never stores passwords or key files.

## 3. Architecture

A Cargo workspace with three crates:

```
Cargo.toml                 workspace
crates/
  atv-core/                library: all real work, no GUI dependencies
  atv-cli/                 binary `atv`: thin wrapper over atv-core
  atv-gui/                 binary: Tauri 2 app, thin wrapper over atv-core
docs/
```

Rules:

- `atv-core` knows nothing about the GUI toolkit, windows, or terminals. It exposes functions, option structs, a progress callback, and a cancel token. This is what made the move from Slint to Tauri cheap.
- `atv-core` never holds a whole input file in memory. Everything streams, so a 50 GB input works on a machine with 8 GB of RAM.
- `atv-core` returns typed errors (`thiserror`). No `unwrap`/`expect` on anything that can fail at run time.

### 3.1 Core API (shape, not final signatures)

```rust
pub enum Resolution { P1080, P2160 }
pub enum Mode { Lossless, Robust { cell: Cell, parity: u8 } }   // Cell::C4 | Cell::C8
pub struct Protection { pub password: Option<SecretString>, pub key_file: Option<PathBuf> }

pub struct EncodeOptions {
    pub inputs: Vec<PathBuf>,
    pub output: PathBuf,
    pub resolution: Resolution,
    pub mode: Mode,
    pub compress: bool,
    pub protection: Option<Protection>,
    pub verify: bool,
    pub tools: FfmpegTools,          // resolved paths to ffmpeg and ffprobe
}

pub fn encode(opts: &EncodeOptions, on_event: &mut dyn FnMut(Event), cancel: &CancelToken)
    -> Result<EncodeReport, AtvError>;

pub fn probe(video: &Path, tools: &FfmpegTools) -> Result<VideoInfo, AtvError>;   // reads the header frames, plus the first data frame for a thumbnail

pub fn decode(opts: &DecodeOptions, on_event: &mut dyn FnMut(Event), cancel: &CancelToken)
    -> Result<DecodeReport, AtvError>;

pub fn estimate(inputs_total_bytes: u64, resolution: Resolution, mode: &Mode) -> Estimate; // frames, seconds, size
pub fn measure_packed(inputs: &[PathBuf], cancel: &CancelToken) -> Result<u64, AtvError>; // compressed size, nothing kept
pub fn estimate_packed(packed_bytes: u64, resolution: Resolution, mode: &Mode) -> Estimate;
pub fn password_strength(password: &str) -> PasswordStrength; // zxcvbn score 0-4, enough, warning
pub fn generate_key_file(path: &Path) -> Result<(), AtvError>;
pub fn find_ffmpeg(override_path: Option<&Path>) -> Result<FfmpegTools, AtvError>;
```

`Event` carries: the current stage, bytes done and total, frames done, an optional small preview image (already downscaled, at most about 10 per second), and counts of repaired bytes in Robust mode.

`AtvError` variants at minimum: `FfmpegNotFound`, `FfmpegFailed { stderr }`, `NotAtvVideo`, `UnsupportedVersion`, `UnsupportedResolution`, `KeyRequired { password, key_file }`, `WrongKey`, `Damaged { frame }`, `VerifyFailed`, `Cancelled`, `Io`.

Added while building (milestone 1): `IntegrityFailed` (every frame was readable but the SHA-256 at the end does not match), `UnsafePath { path }` (an archive entry tried to leave the output folder, section 4.2), and `InvalidInput { message }` (the request itself cannot be done: no inputs, an unreadable input, a wrong output file name). `FfmpegNotFound` carries a `detail` sentence saying what is wrong (not found, cannot run, or missing an encoder).

**Stages.** Because the pipeline streams, compressing, encrypting and writing frames all happen at the same time, not one after another. `Event.stage` therefore names the step that sets the pace: `Preparing`, `WritingFrames`, `Verifying` for encode; `ReadingFrames`, `CheckingIntegrity` for decode. The GUI's stage list is drawn from these plus the options in use.

**Output file name.** ffmpeg picks the container from the file name, so `encode` requires the output to end in `.mkv` for Lossless and `.mp4` for Robust, and returns `InvalidInput` otherwise. It also refuses to write the video over, or inside, one of its own inputs.

On cancel or error during encode: kill the ffmpeg child, wait for it, and delete the partial output file.

## 4. Pipeline

Encode:

```
inputs ─► tar stream ─► [zstd] ─► append SHA-256 ─► [encrypt in chunks] ─► split into frames
       ─► [Robust: Reed-Solomon + interleave] ─► draw bits as pixels ─► pipe to ffmpeg ─► video file
```

Decode is the exact reverse, ending with the SHA-256 check and extraction of the tar stream.

### 4.1 Layers, precisely

1. **inner** = a tar stream of the inputs (`tar` crate). Each input is stored under its own base name; folders are added recursively. A single file is still wrapped in tar, so the file name always travels with the data. Details settled while building:
   - Two inputs with the same base name: the second is stored as `name (2).ext`.
   - Folders are walked in name order, so the same inputs always give the same archive. Empty folders are kept.
   - Modification times are kept. Owner and group are not stored.
   - Symbolic links found inside a folder are **not stored**; they are counted and reported. An input that is itself a link is followed, because the person picked it.
2. **packed** = `zstd(inner)` at level 3 if compression is on, else `inner`. **"Smallest video"** (maintainer's choice, 13 Nov 2026) uses level 19 instead: measured on the project's own documents and source, 20.5% of the original instead of 24.7% (17% smaller), at about 5 MB/s instead of 300 MB/s of input to compress; restoring is just as fast and the format is unchanged (any zstd level decodes the same way). It does nothing for already-compressed files.
   **Long-distance matching** (maintainer's choice, 14 Nov 2026, instead of adding ZPAQ): both levels turn on zstd's long-distance matching with a window just big enough for the archive (its estimated tar length rounded up to a power of two, between 1 KiB and **512 MiB**). Data repeated anywhere within that distance, such as copies of a file in different folders or near-identical files, is then stored once; plain level 3 only looks back 2 MiB. Measured on a 4.40 GB folder of mostly PNG, PDF and MP4 with 26,647 files: plain level 3 gave 4.072 GB in 8 s; with long-distance matching, a 128 MiB window gave 3.806 GB (13 s), **512 MiB 3.647 GB (15 s, 10.4% smaller)**, 1 GiB 3.613 GB and 2 GiB 3.598 GB. 512 MiB keeps nearly all the gain while capping the memory needed to create or restore. Through ATV itself (1080p Lossless, not encrypted): 3.81 GB of video, 14,079 frames, 7:49, made and verified in 137 s with at most 562 MB of memory, against 4.25 GB, 15,719 frames and 8:44 before. ZPAQ was considered and not adopted (not measured): its main advantage on this folder, storing repeats once, is what long-distance matching already gives, and it runs at roughly 1 to 5 MB/s both ways with a C++ dependency every restore would need. The decoder accepts windows up to **2 GiB** (the same bound as Argon2's memory, section 6.3), so a hostile video cannot make it allocate more. The format is otherwise unchanged and no header field records this; a video whose window is above zstd's default 128 MiB limit cannot be restored by ATV builds from before this change.
   **Measured size before encoding** (maintainer's choice, 14 Nov 2026): `measure_packed` compresses the inputs once at level 3 with the same settings, keeping nothing, and `estimate_packed` turns that size into frames, length and file size. The app runs it in the background whenever the Encode items change (about 15 s for the 4.40 GB folder above) and shows "≤" until it finishes, then "≈"; with "Smallest video" the measurement is an upper limit, so "≤" stays. A newer list of items, or starting an encode, stops a measurement still running.
3. **body** = `packed ‖ SHA-256(inner)`. The last 32 bytes of the body are always the hash of the tar stream.
4. **sealed** = the body encrypted in chunks (section 6) if encryption is on, else the body unchanged.
5. **frames** = the sealed stream cut into frame payloads (section 5).

Because of streaming, total sizes are not known when the first frame is written. Nothing in the header depends on totals; the last data frame carries a "last" flag.

**Verify after encode:** run the finished video through the decode pipeline up to the hash check, discarding the tar bytes instead of extracting them, and compare the hash with the one computed while encoding. Fail with `VerifyFailed` and keep the file only if the user asks.

As built: verify also checks that the header read back is byte for byte the one that was written. `EncodeOptions.keep_unverified` (off by default) is the "keep the file" switch; with it off, a video that fails verification is deleted. Cancelling during verification also deletes the video, since it was never confirmed good.

### 4.2 Extraction safety

When extracting the tar stream on decode:

- Reject absolute paths and any path containing `..`.
- Do not follow or create links that point outside the output folder.
- Never overwrite an existing file silently; extract into a new folder or pick a unique name.

How this is implemented:

- A bad path, or a link pointing outside the output folder, stops the whole restore with `UnsafePath`. Version 1 never creates links at all: a link that stays inside the output folder is skipped and counted (ATV's own encoder never writes one).
- Files are restored into a hidden temporary folder inside the output folder (`.atv-partial-…`). Only after the last frame has been read and the SHA-256 has matched are the top-level items moved into the output folder. If a name is already taken there, the restored item becomes `name (2)`. A failed or cancelled restore deletes the temporary folder, so nothing half-restored is ever left behind.

## 5. Video format (version 1)

All multi-byte integers are little-endian unless stated.

### 5.1 Grid and bit order

- `W × H` is 1920 × 1080 or 3840 × 2160. **Resized copies** (maintainer's decision after the YouTube test, section 8.4): a video with the same 16:9 shape at another size, such as the 720p copy a site makes, is read scaled back. ffmpeg scales it (Lanczos) to 1080p, and if no header is found there, to 4K; the header's CRC decides which. A video at exactly 1080p or 4K is read as it is first; if no header is found, it is tried scaled to the other ATV size, because a site can shrink a 4K video to exactly 1920 × 1080 (found while comparing ATV with YtDisk, 13 Nov 2026; YouTube serves that copy while it still makes the 4K one). Any other shape is refused with `UnsupportedResolution`. `VideoInfo.resized_from` gives the file's own size.
- A **cell** is a square of `c × c` pixels, all the same colour. `c = 1` in Lossless mode; `c = 4` or `8` in Robust mode.
- The grid has `W/c` columns and `H/c` rows. Frame capacity `C = (W/c × H/c) / 8` bytes. This is a whole number for every allowed combination.
- Bits are laid out row by row from the top-left, **most significant bit of each byte first**. Bit 1 = white (255), bit 0 = black (0).

With `c = 1` this layout is exactly ffmpeg's `monob` pixel format, so in Lossless mode the frame bytes are piped to ffmpeg unchanged.

| Resolution | Cell | Capacity `C` (bytes per frame) |
|---|---|---|
| 1080p | 1 | 259,200 |
| 1080p | 4 | 16,200 |
| 1080p | 8 | 4,050 |
| 4K | 1 | 1,036,800 |
| 4K | 4 | 64,800 |
| 4K | 8 | 16,200 |

Frame rate is 30 fps.

### 5.2 Video structure

```
frame 0   header frame
frame 1   header frame (identical copy)
frame 2…  data frames, index 0, 1, 2, … ; the final one has the "last" flag
```

### 5.3 Header frame

The header frame is **always drawn with 8 × 8 cells**, whatever the mode, so a decoder can read it before it knows the mode. Its content is the 128-byte header record below, repeated back to back to fill the frame (31 copies at 1080p); leftover bytes are zero.

The decoder thresholds the cells, then scans the copies and accepts the first one with the right magic and a valid CRC. If frame 0 has none, it tries frame 1. If neither has one, the error is `NotAtvVideo`.

Added while building: if no single copy in a frame is intact, the decoder takes a vote across all the copies for each bit and accepts the result if its CRC is right. This costs nothing in the format and lets the header survive damage that touches every copy in a different place.

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | Magic, ASCII `ATV1` |
| 4 | 1 | Format version: 1 (Lossless, and Robust before 13 Nov 2026), 2 (Robust, section 5.4a), 3 (Robust since 14 Nov 2026, section 5.4b) |
| 5 | 1 | Mode: 0 = Lossless, 1 = Robust |
| 6 | 1 | Cell size: 1, 4 or 8 |
| 7 | 1 | Flags: bit 0 compressed, bit 1 encrypted, bit 2 password used, bit 3 key file used |
| 8 | 1 | Repair strength: Reed-Solomon parity bytes per 255-byte codeword in versions 1 and 2; in version 3, 64 = strong and 32 = standard LDPC (0 in Lossless) |
| 9 | 3 | Reserved, zero |
| 12 | 4 | Argon2 memory in KiB (0 if no password) |
| 16 | 4 | Argon2 iterations |
| 20 | 4 | Argon2 parallelism |
| 24 | 16 | Salt (random; zero if not encrypted) |
| 40 | 19 | Stream nonce prefix (random; zero if not encrypted) |
| 59 | 65 | Reserved, zero |
| 124 | 4 | CRC-32 (IEEE) of bytes 0–123 |

The header is not secret. It reveals the mode and whether the video is encrypted, never the file names, sizes or content.

### 5.4 Data frames

Each data frame carries one **frame block**:

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | Frame index, starting at 0 |
| 4 | 4 | Payload length in bytes |
| 8 | 1 | Flags: bit 0 = last frame |
| 9 | 3 | Reserved, zero |
| 12 | 4 | CRC-32 of bytes 0–11 followed by the payload |
| 16 | … | Payload, then zero padding to the block size |

**Lossless mode:** block size = `C`. The block is drawn directly. Payload capacity is `C − 16` bytes.

**Robust mode:** the block is protected with Reed-Solomon over GF(2⁸) before drawing.

- Codeword length 255 bytes, of which `p` are parity and `k = 255 − p` are data. `p = 64` ("strong repair") is the default and `p = 32` ("standard repair") the option, since 13 Nov 2026: the maintainer chose strong after WhatsApp's 480p copy of a `p = 32` video failed by 2 bytes in one codeword and a `p = 64` copy restored (section 8.5). Until then 32 was the default. Every video records its own `p`, so older videos read as before.
- A frame holds `n = floor(C / 255)` codewords. Block size = `n × k`. Payload capacity is `n × k − 16`.
- **Interleaving:** byte `i` of codeword `j` is written at frame position `i × n + j`. A damaged patch of the picture is then spread thinly across all codewords instead of destroying one.
- The remaining `C − n × 255` bytes of the frame are zero.

Decoding a Robust frame: threshold cells, de-interleave, correct each codeword, then check the CRC. Report the number of corrected bytes through `Event`.

Details settled while building (milestone 3):

- **The exact code.** Reed-Solomon over GF(2⁸) with field polynomial `0x11D`, generator 2, roots 2⁰ … 2ᵖ⁻¹, and the parity bytes placed after the data bytes of each codeword. This is what the `reed-solomon` crate produces; it is written here so the format does not depend on one library.
- **Repairs are counted** only for frames that end up being used, not for a damaged frame that was replaced by a good copy.
- **Fast path.** A received word is a valid codeword exactly when its parity equals what the encoder would produce for its data, and encoding is about five times cheaper than running the decoder. The decoder is only run for codewords that fail this check.
- **Known limitation: tall, narrow damage at 1080p with 8 × 8 cells.** In that one layout a row of cells is 30 bytes and a frame holds 15 codewords, so every row starts on the same codeword. Damage that is narrow but tall (a stripe, the edge of a logo) keeps landing on the same codewords instead of being spread out: a stripe 64 pixels wide and 136 pixels tall, under 0.5% of the picture, is enough to lose a frame. The other three layouts (63, 254 and 63 codewords with rows of 60, 120 and 60 bytes) do not line up this way. Scattered damage from re-compression, which is what the measurements in section 8 are about, is not affected. Fixing it means changing the interleaving rule, which is a format change and therefore the maintainer's decision; it is pinned by a test (`known_weakness_tall_narrow_damage_at_1080p_with_8x8_cells`).

Payload per frame and resulting speed at 30 fps:

| Resolution | Mode | Payload per frame | Data per second of video |
|---|---|---|---|
| 1080p | Lossless | 259,184 B | 7.8 MB |
| 4K | Lossless | 1,036,784 B | 31.1 MB |
| 1080p | Robust 4 × 4, p = 32 | 14,033 B | 421 KB |
| 1080p | Robust 8 × 8, p = 32 | 3,329 B | 100 KB |
| 4K | Robust 4 × 4, p = 32 | 56,626 B | 1.7 MB |
| 4K | Robust 8 × 8, p = 32 | 14,033 B | 421 KB |
| 1080p | Robust 4 × 4, p = 64 | 12,017 B | 361 KB |
| 1080p | Robust 8 × 8, p = 64 | 2,849 B | 85 KB |
| 4K | Robust 4 × 4, p = 64 | 48,498 B | 1.45 MB |
| 4K | Robust 8 × 8, p = 64 | 12,017 B | 361 KB |

### 5.4a Format version 2: Robust frames in groups (13 Nov 2026)

The maintainer chose this after comparing ATV with YtDisk and replaying the real YouTube and WhatsApp damage under several byte placements (section 8.5). New Robust videos are **version 2**; Lossless videos stay version 1 (nothing about them changed). Both versions are read.

- **Groups.** Data frames come in groups of **15**. One frame block (the same 16-byte header: here the *group* index, payload length, last flag, CRC-32) fills a whole group: `15 × n × k` bytes, where `n` is the codewords per frame and `k = 255 − p`. The last group is padded to 15 frames.
- **Diagonal placement.** Within a frame, byte `i` of codeword slot `s` goes to data position `i × n + (s + i) mod n`. In version 1 a row of cells lined up with the codewords (1080p 8 × 8: 30 bytes per row, 15 codewords), so a vertical streak hit one codeword in every row; that is what failed WhatsApp's copy of A.
- **Woven across the group.** Group codeword `g` uses slot `g mod n` of every frame; its byte `i` lives in frame `(⌊g / n⌋ − i) mod 15`. So each codeword has 17 bytes in each of the 15 frames.
- **Frame tags.** Each frame carries a 16-byte tag (data-frame index, a last-group flag, 7 zero bytes, CRC-32 of the first 12) as a shortened Reed-Solomon codeword with up to 128 parity bytes (all the spare bytes allow: 30 bytes in all at 4K 4 × 4, 134 at 1080p 4 × 4 and 4K 8 × 8, 144 at 1080p 8 × 8). Its bytes sit at evenly spread positions `⌊j × capacity / tag length⌋`, in the bytes every layout has after its last codeword, so the tag costs no data capacity; data bytes take the other positions in order. Header frames and noise never pass the tag's code and CRC.
- **Reading.** Frames are placed by their tags, in any order; repeats are ignored. A frame whose tag cannot be read is kept aside: when the next readable tag arrives and the kept frames exactly fill the gap in the numbering (or follow the last tag at the end of the video, or precede the first one after the header frames), they are those frames in order, and their data is used. Otherwise nothing is guessed and they count as missing. Found with the YouTube 720p imitation: at 4K 4 x 4 the tag has only 30 bytes (7 fixable), and compression broke a few tags while the data stayed repairable; inference turned that failure into a restore with 13,208 bytes repaired. When a frame of the next group arrives (or the video ends), the group is repaired. A missing frame's bytes are **erasures** (known positions): Reed-Solomon fixes any mix with `2 × errors + erasures ≤ p`. With `p = 64` a whole lost frame (17 erasures per codeword) leaves room for 23 more wrong bytes per codeword. A whole missing group, or a group past repair, is `Damaged { frame }` with the group's first frame.
- **Measured** (section 8.5 replay, then real version-2 videos through ffmpeg): survives a lost frame in every real YouTube and WhatsApp copy we have with strong repair, a quarter of a frame blacked out (worst codeword 10 of 64), two lost frames in different groups, a repeated frame, and a WhatsApp-like 480p copy. Cost: 15 frames in memory (at most about 1 MB) and the last group's padding.
- Version 2 is decided by the header's version byte (offset 4). The damage meter (`atv measure`) judges version-2 copies group by group.

### 5.4b Format version 3: LDPC repair (14 Nov 2026)

The maintainer chose this ("build v3, safe settings") after the LDPC research and scale test in section 8.8. New Robust videos are **version 3**; versions 1 and 2 are still read; Lossless videos stay version 1. Header byte 8 keeps its values: 64 now means **strong** repair and 32 **standard** repair, and the version byte says which code that is.

- **Groups and blocks** as in version 2: 15 frames, one frame block (group index, payload length, last flag, CRC-32) per group, the last group padded to 15 frames.
- **The codes.** Regular quasi-cyclic LDPC, codewords of 8,100 bits: a 3 × L table of shifts, each standing for a Z × Z identity matrix rotated right by that many places; bit `column × Z + t` is in check `row × Z + (t + shift) mod Z`, so every bit is in exactly 3 checks. Strong: L = 20, Z = 405. Standard: L = 30, Z = 270. Each block row's checks add up to the all-ones word, so 2 of the 3Z checks repeat the others: strong has 6,887 data bits (17.6% extra), standard 7,292 (11.1% extra).
- **Encoding** is systematic. The check matrix is brought to reduced row echelon form over GF(2), choosing each pivot from the last column down (for each column from 8,099 to 0: if a not-yet-used row has a 1 there, it becomes that column's row and the column is cleared from every other row). Pivot columns hold parity bits; every other column holds a data bit, in increasing order. A codeword carries `⌊data bits / 8⌋` bytes (860 strong, 911 standard), first byte's top bit first, in the first data positions; the remaining data bits are 0 and the decoder knows them. Each parity bit is the parity of the data bits its row covers.
- **The shift tables** came from a search for no 4-cycles and few 6-cycles (strong: none; standard: 36) and are part of the format. A first design used Wi-Fi's (802.11n) staircase shape, which encodes in one pass, but its staircase bits sit in only 2 checks: with a lost frame in every group on harsh WhatsApp damage it failed 6 codewords in 5 million, against none for the regular code (section 8.8).

  | Code | Row | Shifts, column 0 to L − 1 |
  |---|---|---|
  | Strong | 0 | 126 195 159 41 29 200 358 387 352 324 386 270 370 346 95 325 182 101 330 209 |
  | Strong | 1 | 197 327 259 138 196 263 160 378 188 91 12 322 145 331 134 318 232 219 181 388 |
  | Strong | 2 | 77 55 309 182 382 268 242 284 354 58 200 303 212 104 169 109 283 291 240 174 |
  | Standard | 0 | 102 221 168 8 66 247 269 186 5 218 20 68 21 196 212 160 72 173 242 11 25 226 6 164 73 99 188 233 27 216 |
  | Standard | 1 | 145 108 250 35 29 38 37 62 199 86 202 259 63 157 265 71 197 266 196 119 132 75 31 4 234 243 209 22 206 264 |
  | Standard | 2 | 214 64 25 180 27 95 133 47 215 262 93 129 140 258 100 213 96 78 86 197 154 197 167 50 103 169 267 168 66 211 |

- **Frame tags** as in version 2 (16 bytes: index, last-group flag, 7 zero bytes, CRC-32), but always a 48-byte shortened Reed-Solomon codeword (32 parity bytes, 16 fixable), at byte positions `⌊j × capacity / 48⌋`.
- **Placement.** The data cells of a frame are the cells of all non-tag bytes, in row order, then put through a fixed shuffle: Fisher-Yates from the last entry down, entry `k` swapped with entry `next() mod (k + 1)`, `next()` being xorshift64* (shifts 12, 25, 27, multiplier `0x2545F4914F6CDD1D`) seeded with `0x41545633` ("ATV3"). A group holds `C = ⌊data cells / 540⌋` codewords. Bit `b` of codeword `c` goes to frame `f = b mod 15`, slot `((c + f) mod C) × 540 + ⌊b / 15⌋` of the shuffled cells. So every codeword has 540 bits in each frame, a lost frame takes 1/15 of every codeword spread over the whole code, and any patch of damage, of any shape, is shared out over all codewords (version 2's diagonal rule would have sent vertically neighbouring cells to the same codeword; a unit test with two blotches caught it).
- **Whitening.** Before drawing, every codeword is XORed with one fixed 8,100-bit pattern: bit `b` is the top bit of the `b`-th output of the same xorshift64* seeded with `0x41545657` ("ATVW"); the reader flips those bits' confidence back. Frames are then an even mix of black and white whatever the data. Found with the YouTube 720p harsh imitation: a 300 KB video filled a third of its one group, the zero padding drew mostly black frames, compression smeared their scattered white cells away, and data codewords got up to 125 wrong bits where an even spread gives 34. The same would hit every video's padded last group and plain unencrypted files with long runs of equal bytes.
- **Reading.** Each cell is read as in section 5.5, keeping how sure it is: `127 × (2 × sum − 255 × count) / (255 × count)` over its centre pixels, -127 surely black to 127 surely white. Frames are placed by their tags exactly as in version 2 (inference included); a missing frame's cells count as "no idea" (0). Each codeword is decoded with layered normalised min-sum (scale 0.8, at most 50 passes; a codeword whose plain bits already satisfy every check is taken as it is). If a codeword fails, its frames are treated as lost one at a time, the frame that most disagrees with the failed attempt first, and the first repair that works is kept: this rescues a frame that is garbled rather than lost (a unit test wipes 40% of a frame). The block CRC-32, and the SHA-256 of the whole stream, check every result. Repaired bytes are data bytes that differed from what was read.
- **What it survives** (scale test of the final codes and placement, section 8.8; each case draws every codeword's bits from a real group of 15 frames): strong: harsh WhatsApp damage, 0 failures in 20 million codewords; the same with a lost frame in every group, 0 in 5 million; the real WhatsApp phone copy with a lost frame in every group, 0 in 5 million. Standard: YouTube's harsh 720p imitation, 0 in 20 million, so there is no waiting for the HD copy; YouTube's real 720p copy and the harsh 1080p imitation with a lost frame in every group, 0 in 5 million each; harsh WhatsApp damage, 0 in 5 million. Standard does not survive harsh 720p damage and a lost frame together (39% of codewords failed); YouTube has dropped no frames in any test. Neither strength survives two lost frames in one group (version 2's strong repair survived three).
- **Real sites** (14 Nov 2026; 2 MB of known random data each, format 3, uploaded by the maintainer): YouTube made every size of both `Y3` (4K 4 × 4 standard, H.264, 10.9 MB) and `Y3-AV1` (the same with AV1, 5.0 MB) within minutes. All eight copies restored byte-identically, all 45 data frames present: 720p H.264 0.034% and 0.024% wrong bits, 720p VP9 **0.33%** and 0.096%, 1080p H.264 and 4K VP9 0%. Y3's 720p VP9 copy was as rough as the harsh imitation, about ten times the earlier real 720p copies, and still restored. WhatsApp from the phone (Standard) sent `W3` (1080p 8 × 8 strong, 10 fps, 898 kbit/s, 6.8 MB) untouched: same stream, 0% wrong bits, restored identical. To exercise WhatsApp's 480p re-compression, the same data at 30 fps (`W3-30`, 2.69 Mbit/s) was sent too: WhatsApp shrank it to 854 × 480 at 4.8 Mbit/s, all 602 frames kept, 0.26% wrong bits on average (worst frame 0.40%, about the real copy of A), 41,633 bytes repaired, restored byte-identically.
- **Published test vectors** (16 Nov 2026, after a request from a reader): `test-vectors/` holds `data-v3.bin`, the four originals (`W3`, `W3-30`, `Y3`, `Y3-AV1`) and the ten real copies above, with SHA-256 sums, how each copy was obtained, and the results with release v0.1.0: every copy restores, with the same wrong-bit figures. Re-encoding W3 and Y3 with v0.1.0 and the same ffmpeg 8.1 build gave byte-identical files.
- **Speed** (20 MB of random data, this PC): WhatsApp setting encode 49 s (mostly H.264 "slow"), decode 7.8 s, video 3.2 times the data; YouTube setting (H.264) encode 15 s, decode 5.5 s, 3.9 times the data. Both restored identical.

Payload per frame (block per group ÷ 15):

| Layout | Codewords per group | Strong | Standard |
|---|---|---|---|
| 1080p 8 × 8 | 59 | 3,383 B (v2: 2,865) | 3,583 B (v2: 3,345) |
| 1080p 4 × 4, 4K 8 × 8 | 239 | 13,703 B (v2: 12,033) | 14,515 B (v2: 14,049) |
| 4K 4 × 4 | 959 | 54,983 B (v2: 48,514) | 58,243 B (v2: 56,642) |

So the WhatsApp setting (1080p 8 × 8, strong) needs 15% fewer frames than in version 2, and the YouTube setting (4K 4 × 4, standard) 3% fewer.

### 5.5 Reading pixels back

- Ask ffmpeg for 8-bit grey frames and threshold them in Rust. Do not ask ffmpeg for `monob` output; that conversion may dither.
- Lossless: a pixel is 1 if its value is ≥ 128.
- Robust: average the centre `c/2 × c/2` pixels of each cell; the bit is 1 if the average is above 127.5. Edges of cells are the most damaged part after re-compression, so they are ignored.

### 5.6 Frame order problems

A site that re-encodes a video may duplicate or drop frames. The decoder uses the frame index:

- Same index as the previous good frame: skip it.
- A frame that fails its check, followed by another frame with the same index: use the second.
- A gap in the index, or a failed frame with no good copy: `Damaged { frame }`.

Version 2 (section 5.4a) relaxes this: inside a group, a lost or unreadable frame is repaired from the others.

## 6. Encryption

Encryption happens before pixels are drawn, so an encrypted video is indistinguishable from random static.

### 6.1 Keys

- **Password key:** `Argon2id(password, salt, m, t, p)` → 32 bytes. Normalise the password to Unicode NFC before hashing. Defaults: `m = 65536 KiB` (64 MiB), `t = 3`, `p = 4`. The values used are stored in the header, so defaults can change later without breaking old videos.
- **Key file key:** 32 random bytes from the operating system's secure random source.
- **Stream key:** `HKDF-SHA256(salt = header salt, ikm = password key ‖ key file key, info = "ATV v1 stream key")` → 32 bytes. A part that is not in use is left out of `ikm`; the header flags record which parts are in use, so the combination is unambiguous.

Key file format, extension `.atvkey`, plain text:

```
ATV-KEY-1
<64 hexadecimal characters>
```

### 6.2 Stream cipher

XChaCha20-Poly1305 in the STREAM construction (Hoang, Reyhanitabar, Rogaway, Vizár, 2015). It is short enough to write directly on top of the `chacha20poly1305` crate, which avoids depending on helper APIs that have moved between crate versions:

- Plaintext is cut into chunks of 65,536 bytes. Each chunk becomes ciphertext of the same length plus a 16-byte tag.
- Nonce (24 bytes) = `prefix (19 bytes, from the header) ‖ chunk counter (u32, big-endian, from 0) ‖ last flag (1 byte: 0x01 for the final chunk, else 0x00)`.
- Associated data for every chunk = header bytes 0–123. This binds the header to the content.
- There is always a final chunk, even if it is empty. A stream that ends without one is truncated and must be rejected.

The decoder recognises the final chunk by reaching the end of the sealed stream, so it reads one chunk ahead.

### 6.3 Behaviour

- If the first chunk fails to authenticate: `WrongKey`, shown as "Wrong password or key file, or the video is damaged."
- If a later chunk fails: `Damaged`. If the **final** chunk fails after good ones, `Truncated` ("the end of the video is missing: it was cut short, not fully copied, or changed"): every frame has passed its own checks by then, so the stream stopping early is the cause (added 14 Nov 2026 after the tamper test).
- The frame reader also says `Truncated` when the video file ends before the frame with the "last" flag and no unreadable frame was skipped on the way (Robust: when nothing of the next group exists); with an unreadable frame skipped it stays `Damaged`.
- If the data, once decrypted, is not a valid zstd stream or tar archive: `Corrupt` ("the data inside the video cannot be read: the video is damaged or was changed"), unless an earlier error (damage, a wrong key, cancel) already explains it. This is what a header edited to claim "not encrypted" gives.
- `probe` reports from the header flags which of password and key file are needed, so the interface asks for the right things.
- Secrets are wrapped so they are wiped from memory when dropped (`zeroize`). Passwords and keys are never logged, never written to history, never put in error messages.

Details settled while building (milestone 2):

- `SecretString` is a small type in `atv-core` built on `zeroize`; its `Debug` output hides the value.
- An empty password counts as no password. Encryption switched on with neither a password nor a key file is refused with `InvalidInput`.
- `KeyRequired { password, key_file }` says what the **video** needs (both flags come from the header), whichever of them was missing. A secret the video does not use is ignored.
- The first encrypted chunk is opened before anything is written, so `WrongKey` and `KeyRequired` never leave files or a new folder behind.
- **A key file is never replaced.** `generate_key_file` fails if the path exists, because overwriting a key file would lock every video made with it for good. On macOS and Linux the file is created readable by its owner only.
- **The header is not trusted.** When unlocking, Argon2 settings above 2 GiB of memory, 64 iterations or 64 lanes are refused, so a hostile video cannot make the app use all the machine's memory just to check a password.
- Random values (salt, nonce prefix, key file) come from the operating system through the `getrandom` crate.

### 6.4 Password strength

Maintainer's decision (14 Nov 2026), after a guessing test cracked a video locked with `123456` on the 20th guess (about 0.6 s a guess through the ATV program, most of it starting ffmpeg): **a password that is a video's only secret must be hard to guess, or the encode is refused** with `WeakPassword` ("this password is too easy to guess: use a longer phrase, or add a key file").

- Strength is judged with **zxcvbn** (crate `zxcvbn` 3.1.1), which knows common passwords, words, names, dates, keyboard patterns and letter substitutions, and estimates the guesses needed. Its score runs 0-4; the minimum is **4, more than 10^10 guesses** (`MIN_PASSWORD_SCORE`). Score 3 was not enough: `Zayneb2024` and `Ahmed@1998`, a name and a year, score 3. Measured cost of guessing (argon2 0.6.0 at ATV's settings, this PC, 20 threads): **61 guesses a second**, 0.08 s per guess on one thread. So a password at the minimum (10^10 guesses) takes about **5 years** of this PC to search fully, and an attacker with many machines or graphics cards could be hundreds of times faster; a typical passphrase such as `Copper-Lantern-8314` (about 10^14) takes about 50,000 years here. Raising the minimum to 10^12 and Argon2 to 256 MiB were offered and declined (maintainer's decision, 14 Nov 2026): the rule stays at score 4. Examples that pass: `Copper-Lantern-8314`, `correct horse battery staple`, `MyCatIsCalledMishmish`. Examples refused: `123456`, `Password1!`, `qwerty123`, `Zayneb2024`.
- **With a key file, any password is allowed**: the key file alone is 256 random bits, and the password is a second lock.
- **Only encoding judges passwords.** Restoring never does, so a video made before this rule, or with a key file and a short password, still opens.
- The check is in `crypto::lock`, so the app and the CLI follow the same rule. The CLI checks right after the first "New password:" prompt and prints zxcvbn's reason. The app's Encode screen shows zxcvbn's score as the strength meter (Weak, Fair, Almost, Strong), the reason in red under it, and keeps Create Video disabled ("Choose a harder password, or add a key file") until the password scores 4 or a key file is added.
- **Tamper test** (14 Nov 2026, against Lossless videos, with the frame and header CRCs recomputed so only the cryptography could notice): the header changed to say "not encrypted"; Argon2 cut to 8 KiB, 1 pass, 1 lane, with the right password; the key-file flag, or the password flag, removed from a video that needs both; one ciphertext bit flipped; the last frame cut off and the one before marked last; the two data frames swapped and renumbered; a frame spliced in from another video with the same password; the header of another same-password video; salt and nonce prefix zeroed. Every one was refused, with no file restored, no plaintext anywhere and no temporary folder left; the untouched video restored. No password or key-file content was found in ATV's settings, history, cache or WebView2 storage after use, in UTF-8 or UTF-16. Two refusals had unclear messages, fixed the same day: a header claiming "not encrypted" gave zstd's "Unknown frame descriptor" (now `Corrupt`), and a cut-off end gave "data frame 0 could not be recovered" (now `Truncated`).
- zxcvbn looks at the first 100 characters only, so a pasted essay cannot stall it. The app sends the password to the Rust side on each keystroke only to judge it; it is wiped after (`Zeroizing`) and never kept or logged. zxcvbn's own working copies are ordinary memory, freed but not wiped.

## 7. ffmpeg

### 7.1 Commands

Lossless encode (frame bytes piped to stdin):

```
ffmpeg -y -hide_banner -loglevel error
       -f rawvideo -pix_fmt monob -s {W}x{H} -r 30 -i -
       -c:v ffv1 -level 3 -coder 1 -context 1 -g 1 -slices 16 -slicecrc 1 -pix_fmt gray
       out.mkv
```

Robust encode (the same `monob` input, with cells already expanded to `c × c` pixels):

```
ffmpeg -y -hide_banner -loglevel error
       -f rawvideo -pix_fmt monob -s {W}x{H} -r 30 -i -
       -c:v libx264 -preset medium -crf 12 -g 30 -pix_fmt yuv420p -movflags +faststart
       out.mp4
```

Decode (frames read from stdout, `W × H` bytes each):

```
ffmpeg -hide_banner -loglevel error -i in -map 0:v:0 -fps_mode passthrough
       -f rawvideo -pix_fmt gray -
```

`-fps_mode passthrough` stops ffmpeg from inventing or dropping frames. On older ffmpeg versions the equivalent is `-vsync 0`; check the installed version and pick the right flag.

Probe:

```
ffprobe -v error -select_streams v:0
        -show_entries stream=width,height,nb_frames,avg_frame_rate,duration,codec_name -of json in
```

`nb_frames` is often missing for MKV. Use duration × frame rate as an estimate, for progress display only. MKV also keeps the duration on the file rather than on the stream, so the code asks for `format=duration` as well (`-show_entries stream=…:format=duration`).

**AV1 for Robust videos** (maintainer's decision, 13 Nov 2026; `VideoCodec::Av1`, CLI `--av1`, app "Smaller upload (slower)"):

```
-c:v libaom-av1 -cpu-used 6 -crf 50 -row-mt 1 -tiles 2x2 -enable-palette 1
-aom-params tune-content=screen -g 30 -pix_fmt yuv420p -movflags +faststart
```

AV1's screen-content tools (palette mode) are made for two-colour pictures. Measured on random blocks: 1.61-1.71 bits of video per 8 x 8 block and 1.58-1.81 per 4 x 4 block, against 3.05 and 3.73 for the H.264 above (42-56% of the size), with 0 wrong bits read back and the same survival through the site imitations (section 8.6). It is 10-20x slower (about 4-5 frames a second at 1080p and 1 at 4K on the development PC), so the app uses it automatically only for the YouTube destination when the job takes at most about 5 minutes; a switch forces it on or off. `find_ffmpeg` records whether the build has `libaom-av1` (`FfmpegTools.av1`); asking for AV1 without it is refused before anything is written. Both the gyan.dev full and essentials builds have it. Tried and not better: H.265 (107%), SVT-AV1 with screen mode (86%).

**Real check (13 Nov 2026):** the maintainer uploaded `AV1-test.mp4` (2 MB of known data, format v2, 4K 4 x 4, p 64, AV1: 4.8 MB, against 8.8 MB for the same data as H.264 version 1). YouTube accepted it and made every size up to 4K. The 4K (VP9), 1440p (VP9) and 1080p (H.264) copies all measured **0.0000% wrong bits** and restored byte-identically, the smaller two read scaled back.

**Tuned H.264 for 8 x 8 blocks:** `-preset slow -qp 40 -x264-params aq-mode=0:psy-rd=0,0:no-deblock=1:trellis=2`, measured 10% smaller than CRF 12 with the same read-back; the WhatsApp setting still passes the harsh WhatsApp imitation. With 4 x 4 blocks the same settings failed the harsh YouTube 720p imitation, so 4 x 4 keeps CRF 12.

### 7.2 Process handling

- Write to stdin and read stderr on separate threads, or the child can block on a full pipe.
- Keep stderr; put its tail into `FfmpegFailed`.
- Treat a non-zero exit status as failure even if all frames were written.

### 7.3 Finding ffmpeg

Look in this order: the path saved in Settings, ATV's own copy in its tools folder (section 9.5b), `PATH`, then common install locations (`/opt/homebrew/bin`, `/usr/local/bin` on macOS; typical winget and Chocolatey locations on Windows). Run `ffmpeg -version` and check that the `ffv1` and `libx264` encoders are present (`ffmpeg -encoders`). If anything is missing, the GUI shows the "ffmpeg not found" state with install instructions (for example `brew install ffmpeg` on macOS; confirm the current recommended command for Windows before writing it into the app).

## 8. Measurements behind these choices

Measured with ffmpeg 6.1.1 on a Linux cloud machine, random data, 1080p. Speeds will differ on other hardware.

### 8.1 Lossless codec

| Codec and settings | Video size ÷ data size | Exact round trip |
|---|---|---|
| **FFV1, `-coder 1 -context 1`** | **1.04** | yes |
| FFV1, `-coder 1` | 1.09 | yes |
| FFV1, default settings | 1.55 | yes |
| H.264 lossless (`libx264 -qp 0`), medium | 8.69 | yes |
| H.264 lossless, ultrafast | 12.28 | yes |

FFV1 with the range coder and the large context is the clear choice: the video is only about 4% bigger than the data. Lossless H.264 is almost nine times bigger and should not be used. `monob` input converted to grey gave exactly the values 0 and 255. Encoding ran at roughly 22–33 frames per second at 1080p, about 6–8 MB of data per second.

### 8.2 Robust mode under re-compression

The ATV video was encoded with `libx264 -crf 12`, then re-encoded to imitate a hosting site, then decoded with centre sampling. BER is the fraction of wrong bits before error correction.

Quality-based re-encoding (x264 at CRF 23 and CRF 30): **zero bit errors** for 2 × 2, 4 × 4 and 8 × 8 cells.

Bitrate-capped re-encoding (x264, 30 fps unless stated):

| Cells | Raw cell data rate | Cap (actual output) | Average BER | Worst frame BER | Within reach of p = 32? |
|---|---|---|---|---|---|
| 8 × 8 | 0.97 Mbit/s | 3 Mbit/s (2.8) | 0.0007 | 0.0024 | yes |
| 8 × 8 | 0.97 Mbit/s | 2 Mbit/s (2.1) | 0.21 | 0.42 | no |
| 8 × 8 at 10 fps | 0.32 Mbit/s | 1.5 Mbit/s (1.0) | 0.0008 | 0.016 | no: the worst frame is past the limit |
| 8 × 8 at 10 fps | 0.32 Mbit/s | 1 Mbit/s (1.0) | 0.07 | 0.29 | no |
| 4 × 4 | 3.9 Mbit/s | 3 Mbit/s (3.0) | 0.31 | 0.50 | no |
| 4 × 4 at 10 fps | 1.3 Mbit/s | 3 Mbit/s (3.2) | 0.02 | 0.19 | no |

The last column is a judgement from the error rates, not a run of the real decoder: `p = 32` corrects up to 16 wrong bytes in 255 (about 6%), which corresponds to roughly 0.8% wrong bits if they are evenly spread.

What this means:

- Survival depends on the **site's bitrate compared with the data rate**. In these tests the output needed about three times the cell data rate to stay correctable, and at exactly three times one test was already borderline. Near twice, errors jump to a level no error correction can fix.
- Reed-Solomon handles the light, scattered errors of a mild re-encode. It does not rescue a video that was squeezed too hard.
- Errors cluster in the worst frames, which is why interleaving matters. Spreading codewords across several frames would help further and is a candidate for a later format version.
- Lowering the frame rate lowers the data rate and helps. The core should accept a frame rate for Robust mode (default 30); the GUI does not expose it in v1.

**Not tested:** real uploads to YouTube or any other site. A VP9 test here was not valid because the encoder ignored the bitrate cap. Robust mode must be tested against real sites before the app claims it works there. Until then the interface should describe Robust mode as "made to survive re-compression", not as guaranteed.

Size of ATV's own Robust output at CRF 12: about 3.1 times the data for 8 × 8 cells and 3.8 times for 4 × 4. CRF 12 was not tuned; try higher values to shrink the file.

### 8.3 Measured on the finished Robust mode (milestone 3)

Windows 11, 20 threads, ffmpeg 8.1, random data, `p = 32`.

- **Size:** 3.5 times the data for 8 × 8 cells and 4.2 times for 4 × 4 at 1080p (a little above the figures above, which did not include block headers and parity).
- **Re-encode with `libx264 -crf 28`:** decodes correctly for both cell sizes and both parity settings, with **0 bytes needing repair**. This agrees with the zero-error result above for quality-based re-encoding.
- **A second compression is not always smaller:** 8 × 8 cells line up with H.264's own blocks and are already cheap to store.
- **Frame-rate changes:** converting to 60 fps (every frame shown twice) decodes correctly; converting to 15 fps (every other frame dropped) is reported as `Damaged`, as it must be.

Speed, in frames per second:

| Layout | Encode (x264 sets the pace) | ffmpeg decoding alone | ATV decode, whole pipeline |
|---|---|---|---|
| 1080p, 4 × 4 | 96 | 816 | 378 |
| 1080p, 8 × 8 | 139 | 1208 | 629 |
| 4K, 4 × 4 | 24 | 213 | 119 |
| 4K, 8 × 8 | 32 | 283 | 164 |

Reed-Solomon alone: adding parity runs at 550 to 16,000 frames per second, so it is never the limit when encoding. Checking undamaged frames ran at 187 frames per second at 4K with 4 × 4 cells before the fast path (about 45% of decode time, the largest single cost) and 974 after it (about 12%). The worst case that still decodes, every codeword at its limit, runs at 72 frames per second at 4K with 4 × 4 cells and `p = 32` (34 with `p = 64`), which is still faster than the video plays. The `reed-solomon` crate was kept.

### 8.4 YouTube, measured (milestone 6, 13 Nov 2026)

The maintainer uploaded the 8 Robust test videos (2 MB of random data each, not encrypted, not compressed) to YouTube and downloaded them again.

**What came back:** all 8 copies were H.264 at **1280 × 720**, including the 1080p and 4K uploads. Every frame survived and the frame rates were kept (for example 604 frames in, 604 out; 10 fps stayed 10 fps). Whether the 720p size came from YouTube still processing the HD and 4K versions, or from the way the copies were downloaded, is not known yet.

**As downloaded:** ATV refuses all 8 with `UnsupportedResolution`, as section 5.1 says it must.

**Experiment:** each copy scaled back to its original size with ffmpeg (`scale=W:H:flags=lanczos`, then lossless FFV1 so nothing more was lost), then measured with `atv measure` and decoded:

| Video | Upload | Wrong bits, average | Worst frame | Frames past repair | Repaired bytes | Restored files |
|---|---|---|---|---|---|---|
| A | 1080p, 8 × 8, p 32, 30 fps | 0.0000% | 0.0000% | 0 of 602 | 0 | identical |
| B | 1080p, 4 × 4, p 32, 30 fps | 0.0344% | 0.1235% | 0 of 143 | 6,372 | identical |
| C | 4K, 8 × 8, p 32, 30 fps | 0.0250% | 0.0918% | 0 of 143 | 4,622 | identical |
| D | 4K, 4 × 4, p 32, 30 fps | 0.0238% | 0.0299% | 0 of 36 | 4,434 | identical |
| E | 1080p, 8 × 8, p 64, 10 fps | 0.0000% | 0.0000% | 0 of 703 | 0 | identical |
| F | 1080p, 4 × 4, p 64, 10 fps | 0.0356% | 0.1451% | 0 of 167 | 7,673 | identical |
| G | 4K, 8 × 8, p 64, 10 fps | 0.0268% | 0.1157% | 0 of 167 | 5,793 | identical |
| H | 4K, 4 × 4, p 64, 10 fps | 0.0245% | 0.0405% | 0 of 42 | 5,331 | identical |

What this means:

- Even shrunk to 720p, every layout kept its data well inside what error correction repairs: the worst frame of all had 0.15% wrong bits, against roughly 0.8% that `p = 32` can repair (section 8.2). The 8 × 8 videos at 1080p came back with no wrong bits at all.
- So the size is the only thing that stopped decoding. Reading a shrunken copy (scaling it back to the ATV size before sampling the cells) would make these copies restorable. That is a format and product decision for the maintainer: section 5.1 currently requires the exact size.
- Not yet measured: copies downloaded at the full 1080p or 4K size.
- **Decided:** the maintainer chose to make ATV read resized copies (section 5.1). With that, all 8 YouTube copies decode directly to byte-identical files, with the same repair counts as in the table.

**Full-size copies, through the app (13 Nov 2026).** A few hours later YouTube offered every video at its full size (1080p for A, B, E, F; 4K for C, D, G, H). Each link was pasted into the app's From YouTube box (section 9.5a), downloaded with Download and Open, and restored with Restore Files:

| Video | Copy YouTube gave | Wrong bits (`atv measure`) | Restored file |
|---|---|---|---|
| A | AV1, 1920 × 1080, 4.1 MB | 0.0000% (602 of 602 frames) | identical |
| B | AV1, 1920 × 1080, 5.5 MB | 0.0000% (143 of 143) | identical |
| C | VP9, 3840 × 2160, 3.7 MB | 0.0000% (143 of 143) | identical |
| D | VP9, 3840 × 2160, 5.5 MB | 0.0000% (36 of 36) | identical |
| E | AV1, 1920 × 1080, 5.0 MB | 0.0000% (703 of 703) | identical (after updating yt-dlp, below) |
| F | AV1, 1920 × 1080, 7.2 MB | 0.0000% (167 of 167) | identical |
| G | VP9, 3840 × 2160, 5.8 MB | 0.0000% (167 of 167) | identical |
| H | VP9, 3840 × 2160, 6.2 MB | 0.0000% (42 of 42) | identical |

What this means:

- At full size, YouTube's re-encoding left **no wrong bits at all** in any layout, including 4 × 4 blocks with `p = 32`. The damage measured earlier came only from the 720p copies.
- So on YouTube, the denser 4 × 4 layout (four times the data of 8 × 8) works with room to spare, as long as the full-size copy is downloaded. Changing the Robust default from 8 × 8 is the maintainer's decision; one site, one test day, and 2 MB per video are a small sample.
- Each download and restore took about 10 seconds in the app.
- **E and yt-dlp's age.** With yt-dlp 2026.07.04, YouTube refused every download link for E (HTTP 403), about ten times in a row, in the app and by hand, while the other seven downloaded. After updating yt-dlp to 2026.10.26 (`python -m pip install --user -U yt-dlp`), E downloaded at once. yt-dlp has to keep up with YouTube's changes, so the app's advice for refusals ("update yt-dlp") is the right first step.

### 8.5 Telegram and WhatsApp, measured (milestone 6, 13 Nov 2026)

The maintainer sent the 8 test videos (A to H) and a YtDisk video of C's data (Y, for the comparison with YtDisk) to Telegram (Saved Messages) and WhatsApp (Message yourself), as videos, from the **desktop apps**, and saved the copies.

**What came back:** nothing was re-compressed. The Telegram copies are the uploaded files byte for byte. WhatsApp rewrote only the MP4 container (each file 161 bytes smaller), and its video stream is bit-identical to the upload. All 18 copies restore byte-identically (ATV for A to H, YtDisk for Y).

What this means:

- Sent from the desktop apps, both deliver Robust videos untouched, so any Robust layout works there, even the densest (4K, 4 x 4).
- It does not show what their compression does: the phone apps compress videos (WhatsApp's "Standard quality", Telegram's compressed send). That test is still to do, and it is the evidence the maintainer chose to wait for before deciding on repair across frames (section 13).

**WhatsApp from the phone, "Standard" quality (13 Nov 2026).** The test phone (a mid-range Android phone) cannot decode 4K H.264 (its decoders stop at 2560 x 1440 and 1920 x 1088), so WhatsApp refused the 4K videos as "not supported"; the 1080p ones were used. WhatsApp shrank them to **854 x 480** H.264 (about 4.6 to 7 Mbit/s) and kept every frame and the frame rate.

| Video | Wrong bits, average / worst frame | Result |
|---|---|---|
| A: ATV 1080p, 8 x 8, p 32 | 0.243% / 0.386% | failed: 1 of 602 frames past repair |
| B: ATV 1080p, 4 x 4, p 32 | 9.56% / 10.05% | failed: 139 of 143 frames past repair |
| Y1080: YtDisk, 1080p, block 4 (same density as B) | | failed: Reed-Solomon gave up |

Replaying A's real wrong bits under other layouts (same damage, counted per 255-byte codeword): ATV format 1 with p 32 fails by 2 bytes (18 wrong, 16 fixable) in one codeword; **with p 64 it restores with a wide margin (18 of 32)**; shifting each row of cells by one codeword also restores (15 of 16); a random scramble fails (18); spreading codewords over the whole video, as YtDisk does, restores exactly at the limit (16 of 16). So WhatsApp's damage is fairly even, and the stronger parity that ATV already has is the robust answer, not cross-frame repair. To confirm on the real site: E (1080p, 8 x 8, p 64) and Y8 (YtDisk, 1080p, block 8, same density as A) were put on the phone for the maintainer to send.

**The deciding test (13 Nov 2026).** A64 (A's data, 1080p, 8 x 8, **p 64**, 30 fps) and Y8 (YtDisk, 1080p, block 8, same density, p 32) sent the same way. WhatsApp shrank both to 854 x 480.

| Video | Wrong bits | Result |
|---|---|---|
| A64: ATV, p 64 | 0.244% average, 0.417% worst frame | **restored**, 0 of 703 frames past repair (54,969 bytes repaired) |
| Y8: YtDisk, p 32, codewords over the whole video | 0.244% | restored, but its worst codeword had 15 wrong bytes of 16 fixable (7 codewords at 14 or more): one byte from failing |
| E: ATV, p 64, **10 fps**, 0.93 Mbit/s | none | WhatsApp did not re-encode it at all (low bitrate): restored |

Decision: strong repair (`p = 64`) is the Robust default from now on, with "Repair strength: Standard / Strong" in the app (section 9.6). Spreading codewords across frames is not needed for these sites. A low frame rate is another way through WhatsApp untouched.

### 8.6 Local imitations of WhatsApp and YouTube (13 Nov 2026)

The maintainer asked to stop testing by hand. The real copies show exactly what the sites do (read with ffprobe and ffmpeg's per-macroblock QP output), so each site was imitated with ffmpeg and tuned until it reproduced the real wrong-bit rates (`atv measure`).

| Site | Real copies show | Imitation | Imitation vs real wrong bits |
|---|---|---|---|
| WhatsApp phone, Standard | H.264 High, 854 x 480, 91% of macroblocks at QP 51, keyframe every 300, no B-frames, 4.6-7.3 Mbit/s | bilinear shrink, x264 ultrafast QP 51, keyint 300, no B-frames | A 0.207 / 0.243%, B 8.15 / 9.56%, A64 0.208 / 0.244% (about 15% gentler) |
| YouTube 720p file | H.264 High, 1280 x 720, QP 44-45, keyframe every 180, B-frames | bicubic shrink, x264 slow QP 43, keyint 180, 1 B-frame | 4K copies 0.025 / 0.024%; 1080p copies 0 / 0.03% |
| YouTube full size | AV1 1080p (A 1.7 Mbit/s), VP9 4K (C 6.4, D 36 Mbit/s): quality-based, so complex videos get more bits; for ATV's AV1 test video VP9 4K 35 Mbit/s and H.264 1080p 48 Mbit/s | bitrate-matched: libvpx-vp9 4K and x264 1080p at YouTube's real bitrates (the free encoders cannot reach YouTube's efficiency at any quality setting, so quality-based imitations are gentler) | real bitrates: 0 / 0%; the harsh test runs at about half (VP9 20 Mbit/s, H.264 24 Mbit/s: 0.035%) |

A fixed-bitrate imitation of the full size (6 Mbit/s) broke 4K 4 x 4 videos that real YouTube returned with 0 wrong bits, which is why that imitation must be quality-based.

`crates/atv-core/tests/sites.rs` runs the Smart-delivery settings through the **harsher** variants (WhatsApp with a fast-bilinear shrink, about 1.8x the real damage; YouTube 720p at QP 45), so passing leaves a margin: the WhatsApp setting (1080p 8 x 8, p 64, 10 fps), and the YouTube setting (4K 4 x 4, p 64) in H.264 and AV1 through the harsh 720p file and both harsh full-size copies. With standard repair (p 32) the WhatsApp setting fails the harsh imitation, which confirms strong repair.

### 8.7 Denser layouts for YouTube, measured (13 Nov 2026)

Random data at 4K, uploaded as AV1 (screen tools), then through the YouTube imitations of section 8.6:

| Layout | Data per frame | Upload bits per data bit | VP9 4K 36 Mbit/s (real) | VP9 4K 20 Mbit/s (harsh) | H.264 1080p 24 Mbit/s (harsh) |
|---|---|---|---|---|---|
| 4 x 4 black/white (ATV) | 518,400 bits | 1.68 | 0% | 0% | 0.26% |
| 4 x 4, four grey levels (2 bits, Gray-coded) | 1,036,800 | 1.65 | 2.44% | 2.44% | 4.70% |
| 2 x 2 black/white | 2,073,600 | 4.79 | 0.49% | 0.49% | 13.5% |

**Standard repair for the YouTube route** (maintainer's decision): with AV1, 1 MB of data made 1,993 KB with standard repair against 2,471 KB with strong (19% smaller). Standard repair restored through YouTube's 4K (VP9 36 and 20 Mbit/s) and 1080p (H.264 48 and 24 Mbit/s) imitations, but only the gentlest 720p one (QP 43; it failed at QP 44, and YouTube's real 720p files used QP 44-45). So the YouTube destination uses standard repair, and after a YouTube download the app does not restore a standard-repair video from a copy smaller than 1080p: it says YouTube is still making the HD copy and to try again later. Strong repair still survives the 720p file.

Decided: no denser layout. AV1 spends about the same per bit of data however densely it is packed (and far more for 2 x 2 blocks), so density makes videos shorter but not smaller, and much weaker: grey levels fail even at YouTube's real bitrate. For random data the YouTube route is now about 1.68 x 1.34 = 2.25x the data; the rest of the way to the 1x floor is the repair overhead (1.34x, where a better code could save perhaps 10-15%) and AV1's own overhead.

### 8.8 LDPC research, first results (13 Nov 2026)

The maintainer asked to research a repair code that uses how sure each cell is. **Theoretical floor** (channel capacity from the real copies' grey levels, assuming interleaving): WhatsApp's phone copy of A (0.24% wrong bits) needs at least 1.3% overhead with soft decisions (2.5% hard); YouTube's 720p copies 0.1-0.4%; its full-size copy about 0.1%. Reed-Solomon in ATV uses 14.3% (standard) and 33.5% (strong): short 255-byte codewords on whole bytes need a large reserve for unlucky codewords.

**Prototype:** random regular LDPC codes, n = 8,100 bits, column weight 3, normalised min-sum with the cell's grey level as confidence, channel samples drawn from the real copies (ideal interleaving), 60 codewords per case:

| Damage | Wrong bits | Rate 0.95 (5%) | Rate 0.90 (11%) | Rate 0.85 (18%) |
|---|---|---|---|---|
| WhatsApp phone, real (A) | 0.236% | 0 of 60 failed | 0 | 0 |
| WhatsApp harsh imitation | 0.429% | 1 of 60 | 0 | 0 |
| YouTube 720p, real (D, 4K 4 x 4) | 0.024% | 0 | 0 | 0 |

If it holds at scale, an 11% LDPC code would replace strong repair on the WhatsApp route (about 17% smaller videos) and a 5% code would replace standard repair on the YouTube route (about 8% smaller, and surviving the 720p copy, so no waiting for HD). Not adopted yet: 60 codewords only show a failure rate below about 1 in 60, while a 1 GB video has about a million codewords, so the rate must be shown below about 1 in 10 million, with a structured (quasi-cyclic) code to avoid error floors, before a format version 3.

**Scale test (13 Nov 2026, maintainer's choice "prove it at scale first"):** a quasi-cyclic LDPC code (3 x L circulants, no 4-cycles, layered min-sum, Rust, 20 cores), each codeword drawing its blocks from one group of 15 frames of a real or imitated copy. Test videos were made the ATV way: grey-level input to ffmpeg came back with 3x the damage of ATV's 1-bit input (0.70% against 0.22% wrong bits through the WhatsApp imitation), so the first fresh copies were remade. A 3 MB ATV video with today's WhatsApp setting measured 0.23% (imitation) and 0.48% (harsh) with `atv measure`, 0 frames past repair: today's setting is safe.

| Damage, no lost frames | Wrong bits | RS 32 (14.3%), calculated | RS 64 (33.5%), calculated | LDPC 5.3% | LDPC 8.1% | LDPC 11.1% |
|---|---|---|---|---|---|---|
| WhatsApp phone, real (A) | 0.237% | 1.3e-5 | 2e-17 | 0 of 1 M | 0 of 1 M | 0 of 1 M |
| WhatsApp harsh, real ATV video | 0.472% | 1.5e-2 | 6e-10 | 33 of 1 M | 0 of 20 M | 0 of 20 M |
| YouTube 720p, real (D) | 0.024% | 4e-21 | ~0 | 0 of 1 M | 0 of 1 M | 0 of 1 M |
| YouTube 720p harsh (AV1 4K 4 x 4) | 0.355% | 1.1e-3 | 8e-13 | 0 of 5 M (32,400-bit) | 0 of 1 M | 0 of 1 M |
| YouTube 1080p harsh | 0.018% | 4e-18 | ~0 | 0 of 5 M (32,400-bit) | 0 of 20 M | - |

Lost frames (1/15 of every codeword each): with the WhatsApp harsh damage, LDPC 11.1% failed 83% of codewords with one lost frame per group, 17.6% failed none of 5 M with one and all with two; on the YouTube 1080p harsh copy 11.1% failed 1 of 5 M with one. Strong Reed-Solomon survives up to 3 lost frames per group. Meaning: WhatsApp route 12% smaller with LDPC 17.6% (survives 1 lost frame) or 19% with 8.1% (every frame must arrive); YouTube route 8% smaller with 5.3% at 32,400 bits (every frame must arrive, no waiting for HD) or 3% with 11.1% (survives the 720p copy and a lost frame). At 5.3% with 8,100-bit codewords a few failures decoded to a wrong codeword; the frame checksum catches those. The maintainer chose to build format version 3 with the safe settings (strong 17.6% for WhatsApp, standard 11.1% for YouTube): section 5.4b.

## 9. GUI (Tauri 2)

The screens, states and wording follow a GUI design brief and an approved design handoff used during development (not published).

**History of this decision.** Milestone 4 was first built in Slint 1.18.1 (commit `1841bbb`). After trying it, the maintainer chose to move the GUI to **Tauri 2**, as section 2 allowed. Only `atv-gui` was replaced; `atv-core` and `atv-cli` did not change for it. Because the design handoff is HTML and CSS, the Tauri GUI was built directly with the approved design, which merged the redo of milestone 4 with milestone 5.

### 9.1 Structure

- `crates/atv-gui/web/` is the page: `index.html` (every screen and state, shown and hidden by the script), `theme.css` (the design's `atv-theme.css`, unchanged apart from leaving out the sample wallpapers), `app.css` (layout and components, using only the theme's colours), `icons.js` (the design's icon list) and `app.js` (screen state, and calls to the Rust side). Plain HTML, CSS and JavaScript: no framework, no npm packages, no build step.
- `crates/atv-gui/src/` is the Rust side: `commands.rs` (everything the page can ask for, and the job runner), `jobs.rs` (progress arithmetic: stages, percent, speed, time left), `messages.rs` (errors in plain words), `files.rs` (sizes, kinds, free names), `store.rs` (settings and history), `platform.rs` (macOS and Windows differences), `text.rs` (number formatting).
- The page never touches files or ffmpeg itself. File dialogs are opened from Rust (`tauri-plugin-dialog`), so the page has no file-system or dialog permissions at all (`capabilities/default.json`).

### 9.2 Threading

Never run `atv-core` work on the main thread. Each job runs on its own thread; its `Event`s become `job-update` events for the page (the engine already limits them to about 30 a second, and previews to about 10 a second). A preview is sent as base64 grey bytes and drawn into a `<canvas>`. Commands that open a dialog or run ffmpeg are `async`, so Tauri runs them off the main thread: the dialog plugin's blocking calls must never run on it.

### 9.3 Glass

- **Windows 11:** the window has no system frame (`decorations: false`), is transparent, and uses a system material: Acrylic by default, Mica or none by the maintainer's setting (below). The design's own caption buttons (46 × 32) sit at the top right; the toolbar and the sidebar top are drag regions. When the Appearance setting forces light or dark, the window's theme (and Mica's `micaLight` / `micaDark`) follows.
- **macOS:** overlay title bar with the traffic lights inside the sidebar (`trafficLightPosition`), transparent window, `sidebar` vibrancy (`tauri.macos.conf.json`, which needs `macOSPrivateApi` and Tauri's `macos-private-api` feature). Not yet tried on a Mac.
- Inside the window, surfaces are translucent fills, borders and shadows from the design; the page blurs nothing itself.
- **The maintainer's adjustments (more glass, then a choice).** After trying the app, the maintainer found the design's Mica, tinted 86-88%, too solid; then bare Mica still too solid; then chose **Acrylic**, which blurs whatever is behind the window, and asked for the material to be a setting. Settings → Appearance → **Window glass** (Windows only, since macOS is untried): **Acrylic** (default; window tint 30-35%, content 35-45%), **Mica** (wallpaper colours only; tint 10-20%) or **Solid** (the design's solid colours). Windows builds before 22621, where Acrylic stutters, get Mica instead. The tints are in `app.css`, top; the design file itself is unchanged. Saved as `material` in the settings file.
- **Solid fallback:** the page switches every surface to the design's solid colours (`.solid` in `theme.css`) when Windows "Transparency effects" is off, when the system is older than Windows 11 (build 22000) and has no Mica, when macOS "Reduce transparency" is on, when the web view reports `prefers-reduced-transparency`, and on Linux.
- **Accent colour:** read from the system. Windows keeps eight shades of the accent in the registry (`AccentPalette`); the page uses the first darker shade on light and the second lighter shade on dark, as Windows' own controls do. macOS: `AppleAccentColor`. The page then derives `accent-ink`, `on-accent`, `accent-soft` and `focus` by the rules in the design values table (for example black text on a light accent).

### 9.4 Drag and drop of files

Tauri's web view reports operating-system drags directly (`onDragDropEvent`: enter with the paths, over, drop, leave). Dropping on Encode adds the items; dropping on Decode opens the first item as a video; dropping on History or Settings switches to Encode. While a job runs, or while ffmpeg is missing, drops are ignored.

(For the record: Slint 1.18.1's `DropArea` never received operating-system drops, so the Slint version used winit's window events instead.)

### 9.5 Other pieces

- Settings and history: JSON files in the per-user application data folder (`directories` crate). History entries hold direction, names, date, resolution, mode, encrypted yes/no, size, result. Nothing secret. The Slint and Tauri versions use the same files and format.
- One job at a time in v1. Starting a second while one runs is refused.
- "Show in Finder" / "Show in Explorer" opens the containing folder with the file selected.

### 9.5a Decoding from a YouTube link

Added at the maintainer's request after the YouTube test (13 Nov 2026): the Decode screen has a **From YouTube** box under the drop zone.

- **Paste a link.** Watch, `youtu.be`, Shorts, live and embed links are accepted, with or without extra parameters (core: `YouTubeLink::parse`). ATV rebuilds the link from the 11-character video id and passes it to yt-dlp after `--`, so nothing typed can be read as an option. Anything else gets "This isn't a YouTube video link".
- **Preview while it is on YouTube.** ATV asks yt-dlp for the details (`-J`, no download; core: `youtube_details`) and shows YouTube's thumbnail (`i.ytimg.com`), the title, channel, length and the best size YouTube offers. Below 1080p it warns that YouTube may still be making the HD copy. **Play** swaps the thumbnail for YouTube's own player (privacy-enhanced `youtube-nocookie.com` embed), inside the app. **Watch on YouTube** opens the page in the browser. The page's content security policy allows only these two addresses (`img-src https://i.ytimg.com`, `frame-src https://www.youtube-nocookie.com`).
- **Download and Open.** yt-dlp downloads the best **video-only** stream (`-f bv/b`: the largest size, up to 4K; no sound, no merging, no re-encoding) into the app's cache folder (`…\ATV\cache\downloads`), with a progress bar and Cancel. The folder is emptied before each download and when the app starts. When it finishes, the video opens as if dropped, named by its YouTube title, with a "From: YouTube, 1080p copy" line, then unlocking and restoring work as usual. Resized copies are read as section 5.1 says.
- **One job at a time.** The download uses the job slot, so it cannot run beside an encode or decode, and closing the window cancels it.
- **yt-dlp is not bundled**, like ffmpeg. ATV looks for it in its own tools folder (section 9.5b), on the `PATH`, then in the winget, Python user-scripts, Scoop, Chocolatey and Homebrew folders. If it is missing, the box says how to install it (`winget install yt-dlp.yt-dlp`, `brew install yt-dlp`).
- **JavaScript runtime.** YouTube now protects its download links with a puzzle that yt-dlp solves with a JavaScript program. yt-dlp only looks for Deno by itself, so ATV looks for Deno, Node.js or Bun and passes it with `--js-runtimes name:path`. Without one, YouTube refuses many downloads (HTTP 403); this was seen while building.
- **Refusals.** Even with a runtime, YouTube sometimes refuses a download link (HTTP 403) and accepts a fresh one moments later, so ATV tries up to three times in all. Other failures show plain advice (private video, unavailable, bot check, connection) and yt-dlp's own last line in small print.
- **Cancelling** stops yt-dlp and everything it started (on Windows the pip-installed `yt-dlp.exe` is a launcher that runs Python as a second process, so the whole process tree is stopped) and deletes the partial file.
- Core: `find_ytdlp`, `youtube_details`, `download_youtube` in `atv-core/src/youtube.rs`; errors `DownloaderNotFound` and `DownloadFailed`. The real download test is opt-in: `ATV_TEST_YOUTUBE=<link> cargo test -p atv-core youtube -- --ignored`.

### 9.5b Tool manager: ffmpeg, yt-dlp and a JavaScript runtime

Added at the maintainer's request (13 Nov 2026). The maintainer chose: ATV downloads the tools itself; it checks at start-up at most once a day and on request; ffmpeg comes from gyan.dev; ATV also manages Deno.

- **Settings → Tools** lists ffmpeg (required), yt-dlp (for YouTube links) and the JavaScript runtime yt-dlp uses. Each row shows the version in use, where it lives ("ATV's copy" or "On this PC"), its path, and a chip: Up to date, Update to *x*, Not installed, or Unknown version (a build without a release number). **Install** / **Update** downloads the newest official release with a progress bar and Cancel. "Check for Updates" asks again; the card says when it last checked. When an update is waiting, the sidebar's Settings item shows a dot.
- **When it checks.** At start-up, if the last check was more than a day ago, and whenever "Check for Updates" is pressed. The newest versions found are kept in the settings file (`latest_tools`), so the chips stay right after a restart without asking online again. Only version numbers and checksum files are fetched; the requests carry a generic `ATV/<version>` user agent and nothing about the person.
- **Official sources only.**
  - ffmpeg (Windows x64): the gyan.dev **release essentials** build, the Windows build ffmpeg.org links to and the one ATV was developed with. Version from `https://www.gyan.dev/ffmpeg/builds/release-version`; file `…/builds/packages/ffmpeg-<v>-essentials_build.zip` (about 115 MB) and its `.sha256`. On macOS and Linux ATV does not install ffmpeg (ffmpeg.org links no official build with a checksum there) and shows the Homebrew or apt command instead.
  - yt-dlp: the GitHub release (`yt-dlp.exe`, or `yt-dlp_macos`), checked against the release's `SHA2-256SUMS`.
  - Deno: the GitHub release zip for this system (`deno-<target>.zip`), checked against its `.sha256sum`. Only offered when no Deno, Node.js or Bun is found; if Node.js or Bun is present, that row just reports it.
  - The latest version is read from where GitHub's `releases/latest` link redirects, not from GitHub's API, which limits how often it may be asked.
- **Safety.** The checksum file comes from the same official release. The download streams to a temporary file and is hashed as it arrives; on a mismatch it is deleted. Programs are taken out of a zip by exact file name only (`enclosed_name`), written under a temporary name, must report their version before they replace anything, and the old file is moved aside so a failure puts it back. Downloads over 600 MB are refused.
- **Where.** `%LOCALAPPDATA%\ATV\data\tools` on Windows (the system's local data folder elsewhere). ATV's copy is used before the `PATH`; a hand-chosen ffmpeg in Settings still wins, and installing ffmpeg from ATV clears that choice. Nothing outside the tools folder is changed: a winget, pip or Homebrew copy stays as it is.
- **Also offered where needed:** the first-run "ATV needs ffmpeg" screen has **Install ffmpeg** (Windows); the YouTube box offers **Install yt-dlp** when it is missing, and **Check for Tool Updates** when YouTube refuses a download.
- Installing uses the job slot, so no encode, decode or download can run during it, and closing the window cancels it.
- Measured when built: ffmpeg 9.0.2 from ATV passed the full test suite (203 tests) and decoded the AV1 and VP9 YouTube copies byte-identically; the earlier tests used 8.1.
- Core: `tools.rs` (`Tool`, `latest_release`, `install_tool`, `is_newer`, `program_version`, `managed_program`); error `ToolInstallFailed`. Opt-in internet tests: `cargo test -p atv-core tools -- --ignored --nocapture`.

### 9.6 Settled while building the GUI

- **Encryption label.** The design's Protection card says "Encryption · AES-256-GCM". That is not what ATV uses; the app says "Encryption · XChaCha20-Poly1305" (section 6).
- **Thumbnail.** The design shows a picture of the video on the Decode screen, so `probe` also returns a small picture of the first data frame (`VideoInfo.preview`). It costs one extra frame. A video that holds very little data shows an almost black picture, because the rest of its frame is padding.
- **Choosing inputs.** The file dialog cannot pick files and folders together on Windows, so Encode has "Choose Files…" and "Choose Folder…" where the design has one "Choose…".
- **Robust wording.** The Mode hint says "Made to survive sites that re-compress video", not "Survives…", because section 8.2 says the app must not claim that until real sites have been tested.
- **Smart delivery** (maintainer's choice, 13 Nov 2026). The Encode screen asks "Where is this video going?" above the Video card: **Keep or send as a file** (Lossless, 1080p: 0.3-1.05x the files, for drives, USB and the desktop apps that delivered files untouched), **WhatsApp from a phone** (Robust 1080p, 8 x 8, strong repair, 10 fps: E passed WhatsApp untouched at that bitrate; the ladder test in section 8.5 will tune it), **YouTube** (Robust 4K, 4 x 4, **standard repair** since 13 Nov 2026, AV1 when small: 0 wrong bits in YouTube's 4K and 1080p copies, 19% smaller than strong repair; see section 8.7) and **Custom**. Changing Resolution, Mode, Block size or Repair strength by hand switches to Custom. The choice is remembered (`destination` in the settings file). The length estimate follows the frame rate.
- **Smaller upload (slower).** Under Repair strength, for Robust videos when ffmpeg has AV1: writes AV1 (section 7.1), about half the size. Automatic for the YouTube destination when the estimate says it takes at most about 5 minutes; the switch forces it either way; the hint gives the expected time. The size estimate halves when it is on.
- **Smallest video.** Under "Compress before encoding" (shown only when it is on), on the Encode screen and in Settings → Defaults (off by default): "Compresses harder: documents about 17% smaller. Slower to create; no help for zip, video or photos."
- **Repair strength.** Robust mode shows "Repair strength: Standard / Strong" under Block size, on the Encode screen and in Settings → Defaults. Strong (64 parity bytes) is the default; Standard (32) holds 14% more. The hint says which survives what (section 8.5). Video info shows "strong repair" or "standard repair".
- **Summary strip.** As in the design: `≈ 2 min`, `≈ 1.12 GB`, frames. With compression on, the size is an upper limit (`≤`), since the saving is not known ahead.
- **Output names.** A new video is named after the single input, or "ATV video" for several, in the output folder from Settings (default: the system's Videos folder), numbered if the name is taken. "Change…" picks another name; its extension follows the mode.
- **Closing during a job** cancels the job, waits for it to clean up, then closes.
- **History** rows for decodes carry the video's name; "Show in Explorer" opens the first restored item. A wrong or missing password is a retry, not a History row.
- **ffmpeg missing.** Encode and Decode show the design's first-run screen ("ATV needs ffmpeg", with the install command and Check Again); History and Settings show the banner. The install command is `winget install --id Gyan.FFmpeg -e` on Windows (the gyan.dev build this project was developed with) and `brew install ffmpeg` on macOS.
- **Keyboard.** Every control is a real button or field, so Tab reaches everything; segmented controls are radio groups (arrow keys move the choice); switches are `role="switch"`. Focus shows as the design's 3 px accent ring with a 1 px gap. In the confirm dialog the safe choice has focus, Escape closes it, and Tab stays inside it.
- **Reduce Motion** turns movement into short fades.
- **Secrets on screen.** The password is copied into a wiped `SecretString` when a job starts, but the page's own copy cannot be wiped; the fields are cleared when a job finishes and on "Encode Another" / "Decode Another".

### 9.7 Packaging (milestone 7, 14 Nov 2026)

- `tauri.conf.json` bundles with `cargo tauri build` (Tauri CLI 2). Windows (`tauri.windows.conf.json`): NSIS installer only, `installMode: currentUser` (no admin rights; installs to `%LOCALAPPDATA%\ATV`), the GPL text as its licence page. macOS (`tauri.macos.conf.json`): `.app` and `.dmg`, icon `icons/icon.icns` (made from `icon.png`); untested.
- **The app's file is `AnythingToVideo.exe`** (`mainBinaryName`), not `ATV.exe`: Windows ignores case in file names, so `ATV.exe` and the command-line `atv.exe` would be the same file, and building one overwrote the other (found while packaging). The Start menu entry, window title and Apps list still say "ATV".
- The per-user install folder is also where ATV keeps its tools and cache (`%LOCALAPPDATA%\ATV\data`, `cache`). That is safe: the uninstaller deletes only the files it installed and removes the folder only if it is empty (checked in Tauri's NSIS template and by uninstalling).
- Publisher "Hesham Nasser", copyright "Copyright (C) 2026 Hesham Nasser", licence GPL-3.0-only (maintainer's choices); these show in the file properties and in Windows' Apps list. `authors` and `license` are set for every crate.
- `scripts/build-windows.ps1` writes the installer and a portable zip (`AnythingToVideo.exe`, `atv.exe`, `LICENSE.txt`, `README.txt`) to `dist/`; `scripts/build-macos.sh` does the macOS build.
- **Credits** (maintainer's request): "Made by Hesham Nasser (@Hesham0-0Nasser)" in the About card and "by …" on the setup wizard's Welcome page, both opening https://github.com/Hesham0-0Nasser (`open_author_page`); also in the README.

## 10. Command-line tool

A thin wrapper for testing and scripting:

```
atv encode <inputs…> -o out.mkv [--res 1080p|4k] [--mode lossless|robust] [--cell 4|8]
           [--no-compress] [--no-verify] [--password-stdin] [--key-file k.atvkey] [--no-encrypt]
atv decode <video> -o <folder> [--password-stdin] [--key-file k.atvkey]
atv info   <video>
atv keygen <path.atvkey>
atv measure <original> <copy>
```

`atv measure` is for the real-site tests: it matches each frame of a copy (for example one downloaded from a site) to the original and reports the share of wrong bits, the worst frame, lost frames, and, for Robust videos, frames past repair. It works even when the copy cannot be decoded. Core: `measure_damage`. It holds the original in memory, so it is meant for small test videos.

Passwords are read from an interactive prompt or stdin, never from a command-line argument, because arguments are visible to other programs.

How `encode` decides what to lock with:

- `--no-encrypt`: no encryption.
- `--key-file` alone: the key file only.
- `--password-stdin`: the password is the first line of standard input (add `--key-file` to use both).
- None of these: the tool asks for a new password twice. `--password` forces that prompt, which is how to use a typed password together with `--key-file`.
- With no terminal and none of the flags, `encode` stops and explains, rather than writing an unencrypted video by surprise.

`decode` reads the header first and asks only for what the video needs.

`--smallest` turns on "Smallest video" (zstd 19, section 4.1).

Robust mode options: `--cell` defaults to `8` (the safer choice, pending the real-site tests). `--parity 32|64` defaults to `64` (strong repair; the app offers the same choice as "Repair strength"). `--fps N` was added for the real-site tests and is not in the GUI (1 to 60; fewer frames per second means less data per second). In the core these are `Mode::Robust { parity }` and `EncodeOptions.frame_rate`. A Robust video must be named `.mp4`.

## 11. Dependencies

Current versions on crates.io when this was written. Check for newer ones, and read the docs of the version you pin, since several of these crates changed their APIs recently.

| Purpose | Crate | Version seen |
|---|---|---|
| GUI | `tauri`, `tauri-build` | 2.12.5, 2.7.4 (a 3.0 alpha exists; the stable 2.x line is used) |
| File dialogs | `tauri-plugin-dialog` | 2.8.4 |
| Preview pictures to the page | `base64` | 0.22.1 |
| Cipher | `chacha20poly1305` | 0.11.0 |
| Password hashing | `argon2` | 0.6.0 |
| Key derivation | `hkdf`, `sha2` | 0.13.0, 0.11.1 |
| Error correction | `reed-solomon` | 0.2.1 |
| Compression | `zstd` | 0.14.3 |
| Archive | `tar` | 0.4.46 |
| Checksums | `crc32fast` | 1.5.5 |
| Secrets | `zeroize` | 1.9.1 |
| Secure random numbers | `getrandom` | 0.4.3 |
| Password normalisation (NFC) | `unicode-normalization` | 0.1.26 |
| Password prompt (CLI only) | `rpassword` | 7.5.5 |
| Ctrl-C handling (CLI only) | `ctrlc` | 3.5.2 |
| Errors | `thiserror` | 2.0.21 |
| CLI | `clap` | 4.6.7 |
| App folders | `directories` | 6.0.0 |
| Settings and history | `serde`, `serde_json` | 1.0.229, 1.0.154 |
| Local time for history dates | `chrono` | 0.4.45 |
| Downloading tools from official releases (core) | `ureq` (rustls) | 3.5.2 |
| Opening the ffmpeg and Deno zips (core) | `zip` (deflate only) | 9.5.1 |

Note on `reed-solomon`: it corrects errors at unknown positions, which is what Robust mode needs. `reed-solomon-erasure` and `reed-solomon-simd` only rebuild pieces already known to be missing, so they do not fit. The crate is old and not fast; Robust data rates are low, so it should be enough, but measure it and replace or rewrite it if it becomes the bottleneck.

## 12. Testing

Unit tests in `atv-core`:

- Bit packing and cell drawing round trip for every resolution and cell size.
- Header record: write, read, CRC failure, wrong magic, repeated copies with the first ones damaged.
- Frame block round trip, Lossless and Robust.
- Reed-Solomon: inject up to `p/2` byte errors per codeword and recover; inject more and get a clean failure.
- Encryption: round trip; wrong password; wrong key file; password plus key file; a flipped bit in any chunk; a truncated stream; reordered chunks.
- Tar safety: paths with `..`, absolute paths and outside-pointing links are rejected.

Integration tests (skipped with a clear message when ffmpeg is not installed):

- Lossless round trip at 1080p and 4K: a single file, several files, a folder tree, an empty file, a file smaller than one frame, a file spanning many frames.
- The same with compression on and off, and with each protection combination.
- Verify after encode passes on a good video and fails on one that was tampered with.
- Robust round trip, then again after re-encoding the video with `libx264 -crf 28`.
- Cancelling mid-encode leaves no partial output file.
- A non-ATV video gives `NotAtvVideo`.

## 13. Out of scope for v1

- Colour or grey-level pixels to carry more than one bit each.
- Spreading error correction across a whole video. (Groups of 15 frames are in format version 2, section 5.4a.)
- Splitting one input across several videos.
- Uploading to any site. Downloading is limited to YouTube links on the Decode screen (section 9.5a, the maintainer's request).
- Bundling ffmpeg or yt-dlp inside the app's installer. ATV downloads them from their official releases on request instead (section 9.5b).
- Languages other than English.
- Linux as a supported GUI platform (the core and CLI should still build and pass tests there).
