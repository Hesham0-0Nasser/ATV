//! `atv`: a thin command-line wrapper over `atv-core`, for testing and scripting.
//!
//! Passwords are read from a prompt or from standard input, never from an
//! argument, because arguments are visible to other programs.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use atv_core::{
    AtvError, CancelToken, Cell, DecodeOptions, EncodeOptions, Event, FfmpegTools, Mode,
    Protection, Resolution, SecretString, Stage, VideoInfo,
};
use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "atv",
    version,
    about = "Turn any file or folder into a black-and-white pixel video, and back."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Turn files and folders into a video (encrypted unless --no-encrypt)
    Encode(EncodeArgs),
    /// Turn a video back into the original files
    Decode(DecodeArgs),
    /// Show what a video says about itself
    Info {
        /// The ATV video to look at
        video: PathBuf,
    },
    /// Compare a copy of a video (for example one downloaded from a site)
    /// with the original, and count the damaged bits
    Measure {
        /// The video as ATV made it
        original: PathBuf,
        /// The copy to check
        copy: PathBuf,
    },
    /// Create a new key file for locking videos
    Keygen {
        /// Where to write the key file (for example my.atvkey)
        path: PathBuf,
    },
}

#[derive(Args)]
struct EncodeArgs {
    /// Files and folders to put in the video
    #[arg(required = true)]
    inputs: Vec<PathBuf>,
    /// The video file to write (.mkv for lossless, .mp4 for robust)
    #[arg(short, long)]
    output: PathBuf,
    /// Frame size of the video
    #[arg(long = "res", value_enum, default_value = "1080p")]
    resolution: ResolutionArg,
    /// lossless: one pixel per bit, for your own storage.
    /// robust: bigger blocks plus error correction, made to survive re-compression
    #[arg(long, value_enum, default_value = "lossless")]
    mode: ModeArg,
    /// Robust mode: block size in pixels. Larger is safer but holds less
    #[arg(long, value_enum, default_value = "8")]
    cell: CellArg,
    /// Robust mode: repair strength. 64 = strong (the default): survives WhatsApp's
    /// 480p copies and a lost frame in 15. 32 = standard: holds about 6% more; for YouTube
    #[arg(long, value_enum, default_value = "64")]
    parity: ParityArg,
    /// Robust mode: frames per second (1 to 60). Fewer survives harsher re-compression
    #[arg(long, value_name = "N")]
    fps: Option<u32>,
    /// Do not compress the data before encoding
    #[arg(long)]
    no_compress: bool,
    /// Compress harder (zstd 19 instead of 3): documents about 17% smaller, slower to create
    #[arg(long, conflicts_with = "no_compress")]
    smallest: bool,
    /// Robust mode: write AV1 instead of H.264, about half the size but 10-20x slower (for YouTube)
    #[arg(long)]
    av1: bool,
    /// Do not decode the finished video to check it
    #[arg(long)]
    no_verify: bool,
    /// Do not encrypt: anyone with the video can restore the files
    #[arg(long, conflicts_with_all = ["password", "password_stdin", "key_file"])]
    no_encrypt: bool,
    /// Ask for a password (the default when no key file is given)
    #[arg(long, conflicts_with = "password_stdin")]
    password: bool,
    /// Read the password from the first line of standard input
    #[arg(long)]
    password_stdin: bool,
    /// Lock the video with this key file (see `atv keygen`)
    #[arg(long, value_name = "FILE")]
    key_file: Option<PathBuf>,
}

#[derive(Args)]
struct DecodeArgs {
    /// The ATV video to read
    video: PathBuf,
    /// Folder to restore into (existing files are never overwritten)
    #[arg(short, long)]
    output: PathBuf,
    /// Read the password from the first line of standard input
    #[arg(long)]
    password_stdin: bool,
    /// The key file the video was locked with
    #[arg(long, value_name = "FILE")]
    key_file: Option<PathBuf>,
}

#[derive(Clone, Copy, ValueEnum)]
enum ResolutionArg {
    #[value(name = "1080p")]
    P1080,
    #[value(name = "4k")]
    P2160,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ModeArg {
    Lossless,
    Robust,
}

#[derive(Clone, Copy, ValueEnum)]
enum CellArg {
    #[value(name = "4")]
    C4,
    #[value(name = "8")]
    C8,
}

#[derive(Clone, Copy, ValueEnum)]
enum ParityArg {
    #[value(name = "32")]
    P32,
    #[value(name = "64")]
    P64,
}

impl EncodeArgs {
    fn mode(&self) -> Mode {
        match self.mode {
            ModeArg::Lossless => Mode::Lossless,
            ModeArg::Robust => Mode::Robust {
                cell: match self.cell {
                    CellArg::C4 => Cell::C4,
                    CellArg::C8 => Cell::C8,
                },
                parity: match self.parity {
                    ParityArg::P32 => 32,
                    ParityArg::P64 => 64,
                },
            },
        }
    }
}

impl From<ResolutionArg> for Resolution {
    fn from(arg: ResolutionArg) -> Self {
        match arg {
            ResolutionArg::P1080 => Resolution::P1080,
            ResolutionArg::P2160 => Resolution::P2160,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let cancel = CancelToken::new();
    let on_ctrl_c = cancel.clone();
    // Ctrl-C asks the job to stop, so it can remove its partial output first.
    if let Err(error) = ctrlc::set_handler(move || on_ctrl_c.cancel()) {
        eprintln!("warning: Ctrl-C will not clean up ({error})");
    }
    match run(cli.command, &cancel) {
        Ok(()) => ExitCode::SUCCESS,
        Err(AtvError::Cancelled) => {
            eprintln!("Cancelled. Nothing was left behind.");
            ExitCode::from(130)
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command, cancel: &CancelToken) -> Result<(), AtvError> {
    match command {
        Command::Encode(args) => encode(args, atv_core::find_ffmpeg(None)?, cancel),
        Command::Decode(args) => decode(args, atv_core::find_ffmpeg(None)?, cancel),
        Command::Info { video } => {
            let info = atv_core::probe(&video, &atv_core::find_ffmpeg(None)?)?;
            print_info(&video, &info);
            Ok(())
        }
        Command::Measure { original, copy } => {
            let tools = atv_core::find_ffmpeg(None)?;
            let report = atv_core::measure_damage(&original, &copy, &tools)?;
            print_damage(&report);
            Ok(())
        }
        Command::Keygen { path } => {
            atv_core::generate_key_file(&path)?;
            println!("Created key file {}", path.display());
            println!(
                "Keep it safe and private. Without it, a video locked with it cannot be opened."
            );
            Ok(())
        }
    }
}

fn invalid(message: &str) -> AtvError {
    AtvError::InvalidInput {
        message: message.to_string(),
    }
}

/// The first line of standard input, without its line ending.
fn password_from_stdin() -> Result<SecretString, AtvError> {
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    while line.ends_with(['\n', '\r']) {
        line.pop();
    }
    if line.is_empty() {
        return Err(invalid("no password was given on standard input"));
    }
    Ok(SecretString::new(line))
}

/// Ask for a password at the terminal, without showing it.
fn ask_password(prompt: &str) -> Result<SecretString, AtvError> {
    if !std::io::stdin().is_terminal() {
        return Err(invalid(
            "a password is needed but there is no terminal to ask on; use --password-stdin",
        ));
    }
    Ok(SecretString::new(rpassword::prompt_password(prompt)?))
}

/// Ask for a new password twice, so a typing mistake cannot lock the files away.
/// Without a key file, an easy password is refused at once (spec 6.4).
fn ask_new_password(with_key_file: bool) -> Result<SecretString, AtvError> {
    if !std::io::stdin().is_terminal() {
        return Err(invalid(
            "videos are encrypted by default: give --password-stdin or --key-file, \
             or pass --no-encrypt to leave the video open",
        ));
    }
    let first = rpassword::prompt_password("New password: ")?;
    if first.is_empty() {
        return Err(invalid("the password cannot be empty"));
    }
    if !with_key_file {
        let strength = atv_core::password_strength(&first);
        if !strength.enough {
            drop(SecretString::new(first));
            if let Some(warning) = strength.warning {
                eprintln!("{warning}");
            }
            return Err(AtvError::WeakPassword);
        }
    }
    let second = rpassword::prompt_password("Type it again: ")?;
    if first != second {
        return Err(invalid("the two passwords do not match"));
    }
    drop(SecretString::new(second));
    Ok(SecretString::new(first))
}

fn encode(args: EncodeArgs, tools: FfmpegTools, cancel: &CancelToken) -> Result<(), AtvError> {
    let mode = args.mode();
    let protection = if args.no_encrypt {
        None
    } else {
        let password = if args.password_stdin {
            Some(password_from_stdin()?)
        } else if args.password || args.key_file.is_none() {
            Some(ask_new_password(args.key_file.is_some())?)
        } else {
            None
        };
        Some(Protection {
            password,
            key_file: args.key_file,
        })
    };
    let options = EncodeOptions {
        mode,
        inputs: args.inputs,
        output: args.output,
        resolution: args.resolution.into(),
        compress: !args.no_compress,
        smallest: args.smallest,
        protection,
        verify: !args.no_verify,
        keep_unverified: false,
        frame_rate: args.fps,
        video_codec: if args.av1 {
            atv_core::VideoCodec::Av1
        } else {
            atv_core::VideoCodec::H264
        },
        tools,
    };
    let mut progress = ProgressLine::new();
    let result = atv_core::encode(&options, &mut |event| progress.show(&event), cancel);
    progress.clear();
    let report = result?;

    println!("Created {}", report.output.display());
    println!(
        "  {} {}, {} frames, {} long",
        report.resolution.label(),
        mode_name(&report.mode, report.format_version),
        group_digits(report.frames),
        clock(report.seconds),
    );
    let ratio = if report.input_bytes > 0 {
        format!(
            " ({:.1}% of the input)",
            report.video_bytes as f64 * 100.0 / report.input_bytes as f64
        )
    } else {
        String::new()
    };
    println!(
        "  {} in {} -> {} of video{ratio}",
        human_bytes(report.input_bytes),
        plural(report.file_count, "file"),
        human_bytes(report.video_bytes),
    );
    let locked_with = match &options.protection {
        None => "not encrypted",
        Some(Protection {
            password: Some(_),
            key_file: Some(_),
        }) => "encrypted with the password and the key file",
        Some(Protection {
            key_file: Some(_), ..
        }) => "encrypted with the key file",
        Some(_) => "encrypted with the password",
    };
    println!(
        "  {locked_with}, {}, {}",
        if report.compressed {
            "compressed"
        } else {
            "not compressed"
        },
        if report.verified {
            "verified"
        } else {
            "not verified"
        },
    );
    if report.verified && matches!(report.mode, Mode::Robust { .. }) {
        println!(
            "  error correction repaired {} while verifying",
            plural(report.verify_repaired_bytes, "byte")
        );
    }
    println!("  SHA-256 {}", hex(&report.sha256));
    if report.skipped_links > 0 {
        println!(
            "  note: {} inside the folders were not stored",
            plural(report.skipped_links, "link")
        );
    }
    Ok(())
}

fn decode(args: DecodeArgs, tools: FfmpegTools, cancel: &CancelToken) -> Result<(), AtvError> {
    // Look at the header first, so only the secrets the video needs are asked for.
    let info = atv_core::probe(&args.video, &tools)?;
    let protection = if info.encrypted {
        if info.needs_key_file && args.key_file.is_none() {
            return Err(AtvError::KeyRequired {
                password: info.needs_password,
                key_file: true,
            });
        }
        let password = if !info.needs_password {
            None
        } else if args.password_stdin {
            Some(password_from_stdin()?)
        } else {
            Some(ask_password("Password: ")?)
        };
        Some(Protection {
            password,
            key_file: args.key_file,
        })
    } else {
        None
    };
    let options = DecodeOptions {
        video: args.video,
        output_dir: args.output,
        protection,
        tools,
    };
    let mut progress = ProgressLine::new();
    let result = atv_core::decode(&options, &mut |event| progress.show(&event), cancel);
    progress.clear();
    let report = result?;

    println!(
        "Restored {} ({}) into {}",
        plural(report.files.len() as u64, "file"),
        human_bytes(report.total_bytes),
        report.output_dir.display(),
    );
    for item in &report.items {
        let name = item.file_name().unwrap_or(item.as_os_str());
        println!("  {}", Path::new(name).display());
    }
    println!("Integrity confirmed (SHA-256 {})", hex(&report.sha256));
    if report.repaired_bytes > 0 {
        println!(
            "  error correction repaired {}",
            plural(report.repaired_bytes, "damaged byte")
        );
    }
    if report.skipped_entries > 0 {
        println!(
            "  note: {} in the video (links or special files) were not restored",
            plural(report.skipped_entries, "entry")
        );
    }
    Ok(())
}

fn print_info(video: &Path, info: &VideoInfo) {
    let yes_no = |value: bool| if value { "yes" } else { "no" };
    println!("File:        {}", video.display());
    println!(
        "Resolution:  {} ({} x {})",
        info.resolution.label(),
        info.resolution.width(),
        info.resolution.height()
    );
    if let Some((width, height)) = info.resized_from {
        println!("Frame size:  {width} x {height} (resized by a site; read scaled back)");
    }
    println!(
        "Mode:        {}",
        mode_name(&info.mode, info.format_version)
    );
    let needs = match (info.needs_password, info.needs_key_file) {
        (true, true) => " (needs the password and the key file)",
        (true, false) => " (needs the password)",
        (false, true) => " (needs the key file)",
        (false, false) => "",
    };
    println!("Encrypted:   {}{needs}", yes_no(info.encrypted));
    println!("Compressed:  {}", yes_no(info.compressed));
    if let Some(seconds) = info.seconds {
        let frames = info
            .frames
            .map(|frames| format!(" (about {} frames)", group_digits(frames)))
            .unwrap_or_default();
        println!("Length:      {}{frames}", clock(seconds));
    }
    println!("Size:        {}", human_bytes(info.file_bytes));
    println!("Codec:       {}", info.codec);
}

fn print_damage(report: &atv_core::DamageReport) {
    let percent = |share: f64| format!("{:.4}%", share * 100.0);
    let (width, height) = report.copy_size;
    if (width, height) == (report.resolution.width(), report.resolution.height()) {
        println!(
            "Resolution:      {} (same as the original)",
            report.resolution.label()
        );
    } else {
        println!(
            "Resolution:      {} original; the copy is {width} x {height}, read scaled back",
            report.resolution.label()
        );
    }
    println!(
        "Mode:            {}",
        mode_name(&report.mode, report.format_version)
    );
    println!("Copy codec:      {}", report.copy_codec);
    println!(
        "Data frames:     {} in the original, {} in the copy",
        group_digits(report.original_frames),
        group_digits(report.copy_frames)
    );
    println!(
        "Matched:         {} (unmatched {}, lost {})",
        group_digits(report.compared),
        group_digits(report.unmatched),
        group_digits(report.missing)
    );
    println!("Wrong bits:      {} on average", percent(report.mean_ber));
    println!(
        "Worst frame:     {} wrong (data frame {})",
        percent(report.worst_ber),
        group_digits(report.worst_frame)
    );
    if matches!(report.mode, Mode::Robust { .. }) {
        println!(
            "Past repair:     {} of the matched frames",
            group_digits(report.beyond_repair)
        );
    }
}

fn mode_name(mode: &Mode, version: u8) -> String {
    match mode {
        Mode::Lossless => "Lossless".to_string(),
        Mode::Robust { cell, parity } => {
            let side = cell.pixels();
            let strength = if *parity >= 64 { "strong" } else { "standard" };
            let repair = if version >= 3 {
                format!("{strength} LDPC repair, format 3")
            } else {
                format!("{strength} Reed-Solomon repair, {parity} parity bytes, format {version}")
            };
            format!("Robust ({side} x {side} blocks, {repair})")
        }
    }
}

/// One line on the terminal that is redrawn as the job moves along.
/// Silent when the output is not a terminal, so logs stay clean.
struct ProgressLine {
    enabled: bool,
    started: Instant,
    last_drawn: Option<Instant>,
}

impl ProgressLine {
    const WIDTH: usize = 78;
    const INTERVAL: Duration = Duration::from_millis(100);

    fn new() -> Self {
        ProgressLine {
            enabled: std::io::stderr().is_terminal(),
            started: Instant::now(),
            last_drawn: None,
        }
    }

    fn show(&mut self, event: &Event) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        if self
            .last_drawn
            .is_some_and(|last| now.duration_since(last) < Self::INTERVAL)
        {
            return;
        }
        self.last_drawn = Some(now);
        let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
        let speed = format!(
            "{}/s",
            human_bytes((event.bytes_done as f64 / elapsed) as u64)
        );
        let frames = match event.frames_total {
            Some(total) => format!(
                "frame {} of {}",
                group_digits(event.frames_done),
                group_digits(total.max(event.frames_done))
            ),
            None => format!("frame {}", group_digits(event.frames_done)),
        };
        let fraction = |done: u64, total: u64| {
            if total == 0 {
                String::new()
            } else {
                format!("{:3.0}%  ", (done as f64 * 100.0 / total as f64).min(100.0))
            }
        };
        let text = match event.stage {
            Stage::Preparing => "Preparing...".to_string(),
            Stage::WritingFrames => format!(
                "Writing frames  {}{frames}  {speed}",
                fraction(event.bytes_done, event.bytes_total)
            ),
            Stage::Verifying => format!(
                "Verifying  {}{frames}",
                fraction(event.frames_done, event.frames_total.unwrap_or(0))
            ),
            Stage::ReadingFrames => {
                let repaired = if event.repaired_bytes > 0 {
                    format!("  {} repaired", group_digits(event.repaired_bytes))
                } else {
                    String::new()
                };
                format!(
                    "Reading frames  {}{frames}  {speed}{repaired}",
                    fraction(event.frames_done, event.frames_total.unwrap_or(0))
                )
            }
            Stage::CheckingIntegrity => "Checking integrity...".to_string(),
        };
        eprint!("\r{text:<width$}", width = Self::WIDTH);
        let _ = std::io::stderr().flush();
    }

    fn clear(&mut self) {
        if self.enabled && self.last_drawn.is_some() {
            eprint!("\r{:<width$}\r", "", width = Self::WIDTH);
            let _ = std::io::stderr().flush();
        }
    }
}

/// `1234567` becomes `1,234,567`.
fn group_digits(number: u64) -> String {
    let digits = number.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// `1 file`, `2 files`, `1 entry`, `2 entries`.
fn plural(count: u64, noun: &str) -> String {
    let word = if count == 1 {
        noun.to_string()
    } else if let Some(stem) = noun.strip_suffix('y') {
        format!("{stem}ies")
    } else {
        format!("{noun}s")
    };
    format!("{} {word}", group_digits(count))
}

/// A size with a unit a person would use: `812 B`, `7.8 MB`, `1.04 GB`.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    match unit {
        0 => format!("{bytes} B"),
        _ if value >= 100.0 => format!("{value:.0} {}", UNITS[unit]),
        _ if value >= 10.0 => format!("{value:.1} {}", UNITS[unit]),
        _ => format!("{value:.2} {}", UNITS[unit]),
    }
}

/// Seconds as `m:ss` or `h:mm:ss`. Anything under a second shows tenths.
fn clock(seconds: f64) -> String {
    if seconds < 1.0 {
        return format!("{seconds:.1} s");
    }
    let total = seconds.round() as u64;
    let (hours, minutes, secs) = (total / 3600, total / 60 % 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes}:{secs:02}")
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_grouped_in_threes() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1000), "1,000");
        assert_eq!(group_digits(1_234_567), "1,234,567");
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(812), "812 B");
        assert_eq!(human_bytes(1500), "1.50 KB");
        assert_eq!(human_bytes(7_800_000), "7.80 MB");
        assert_eq!(human_bytes(31_100_000), "31.1 MB");
        assert_eq!(human_bytes(2_147_483_648), "2.15 GB");
    }

    #[test]
    fn durations_read_as_clock_time() {
        assert_eq!(clock(0.1), "0.1 s");
        assert_eq!(clock(12.4), "0:12");
        assert_eq!(clock(128.0), "2:08");
        assert_eq!(clock(3725.0), "1:02:05");
    }

    #[test]
    fn counts_get_the_right_plural() {
        assert_eq!(plural(1, "file"), "1 file");
        assert_eq!(plural(2, "file"), "2 files");
        assert_eq!(plural(1, "entry"), "1 entry");
        assert_eq!(plural(3, "entry"), "3 entries");
    }

    #[test]
    fn the_command_line_is_well_formed() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
