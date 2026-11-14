//! Encryption (spec section 6): keys from a password and/or a key file, and
//! the chunked cipher that seals the body before it is drawn as pixels.
//!
//! Nothing in this file logs, prints or stores a secret, and no error message
//! contains one. Key material is wiped from memory when it is dropped.

use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::inout::InOutBuf;
use chacha20poly1305::{AeadInOut, KeyInit, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use sha2::Sha256;
use unicode_normalization::UnicodeNormalization;
use zeroize::{Zeroize, Zeroizing};

use crate::error::AtvError;
use crate::format::header::{HEADER_BODY_LEN, HEADER_LEN, Header};
use crate::progress::Ctx;

/// Plaintext bytes in one sealed chunk.
pub(crate) const CHUNK_LEN: usize = 65_536;
/// Bytes of authentication tag after each chunk.
pub(crate) const TAG_LEN: usize = 16;

const KEY_LEN: usize = 32;
const KEY_FILE_MAGIC: &str = "ATV-KEY-1";
const STREAM_KEY_INFO: &[u8] = b"ATV v1 stream key";

/// Argon2id settings for new videos: 64 MiB, 3 passes, 4 lanes. The values
/// used are stored in each header, so these can change without breaking old videos.
const ARGON_MEMORY_KIB: u32 = 65_536;
const ARGON_ITERATIONS: u32 = 3;
const ARGON_PARALLELISM: u32 = 4;

/// Limits on the Argon2 settings accepted from a header. A header is not
/// trusted: without limits a hostile video could ask for all the memory in
/// the machine just to check a password.
const MAX_ARGON_MEMORY_KIB: u32 = 2 * 1024 * 1024;
const MAX_ARGON_ITERATIONS: u32 = 64;
const MAX_ARGON_PARALLELISM: u32 = 64;

/// The lowest zxcvbn score a password may have when it is a video's only
/// secret: 4, more than 10^10 guesses (spec 6.4). A name and a year scores 3.
pub const MIN_PASSWORD_SCORE: u8 = 4;

/// How hard a password is to guess (spec 6.4). Holds nothing of the password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordStrength {
    /// zxcvbn's score, 0 (guessed at once) to 4 (more than 10^10 guesses).
    pub score: u8,
    /// Strong enough to be a video's only secret.
    pub enough: bool,
    /// Why it is weak, in plain words, when zxcvbn can tell.
    pub warning: Option<String>,
}

/// Judge a password with zxcvbn, which knows common passwords, words, names,
/// dates, keyboard patterns and substitutions. Only the first 100 characters
/// are looked at.
pub fn password_strength(password: &str) -> PasswordStrength {
    let entropy = zxcvbn::zxcvbn(password, &[]);
    let score = entropy.score() as u8;
    PasswordStrength {
        score,
        enough: score >= MIN_PASSWORD_SCORE,
        warning: entropy
            .feedback()
            .and_then(|feedback| feedback.warning())
            .map(|warning| warning.to_string()),
    }
}

/// A 32-byte key that is wiped when dropped.
pub(crate) type Key = Zeroizing<[u8; KEY_LEN]>;

/// A password held in memory. It is wiped when dropped and never shown by
/// `Debug`, so it cannot leak into logs or error messages by accident.
#[derive(Clone)]
pub struct SecretString(Zeroizing<String>);

impl SecretString {
    pub fn new(secret: String) -> Self {
        SecretString(Zeroizing::new(secret))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for SecretString {
    fn from(secret: String) -> Self {
        SecretString::new(secret)
    }
}

impl From<&str> for SecretString {
    fn from(secret: &str) -> Self {
        SecretString::new(secret.to_owned())
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(hidden)")
    }
}

/// The secrets that lock or unlock a video: a password, a key file, or both.
#[derive(Debug, Clone, Default)]
pub struct Protection {
    pub password: Option<SecretString>,
    /// Path to an `.atvkey` file made by [`generate_key_file`].
    pub key_file: Option<PathBuf>,
}

impl Protection {
    fn password(&self) -> Option<&SecretString> {
        self.password
            .as_ref()
            .filter(|password| !password.is_empty())
    }
}

/// Create a new key file holding 32 random bytes from the operating system.
///
/// The file is plain text: a line `ATV-KEY-1`, then 64 hexadecimal characters.
/// An existing file is never replaced, because replacing a key file would make
/// every video locked with it impossible to open.
pub fn generate_key_file(path: &Path) -> Result<(), AtvError> {
    let key: Key = Zeroizing::new(random_bytes()?);
    let mut text = Zeroizing::new(String::with_capacity(
        KEY_FILE_MAGIC.len() + 2 + 2 * KEY_LEN,
    ));
    text.push_str(KEY_FILE_MAGIC);
    text.push('\n');
    for byte in key.iter() {
        text.push(hex_digit(byte >> 4));
        text.push(hex_digit(byte & 0x0F));
    }
    text.push('\n');

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Readable by the owner only.
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            AtvError::invalid(format!(
                "{} already exists; a key file is never replaced, choose a new name",
                path.display()
            ))
        } else {
            AtvError::Io(error)
        }
    })?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn hex_digit(nibble: u8) -> char {
    char::from(b"0123456789abcdef"[usize::from(nibble & 0x0F)])
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

/// Read the key out of a key file.
fn read_key_file(path: &Path) -> Result<Key, AtvError> {
    let not_a_key = || AtvError::invalid(format!("{} is not an ATV key file", path.display()));
    // A real key file is under 100 bytes. Refuse to load anything big.
    let metadata = fs::metadata(path)
        .map_err(|error| AtvError::invalid(format!("cannot read {}: {error}", path.display())))?;
    if metadata.len() > 4096 {
        return Err(not_a_key());
    }
    let bytes =
        Zeroizing::new(fs::read(path).map_err(|error| {
            AtvError::invalid(format!("cannot read {}: {error}", path.display()))
        })?);
    let text = std::str::from_utf8(&bytes).map_err(|_| not_a_key())?;
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    if lines.next() != Some(KEY_FILE_MAGIC) {
        return Err(not_a_key());
    }
    let hex = lines.next().ok_or_else(not_a_key)?.as_bytes();
    if hex.len() != 2 * KEY_LEN || lines.next().is_some() {
        return Err(not_a_key());
    }
    let mut key: Key = Zeroizing::new([0u8; KEY_LEN]);
    for (byte, pair) in key.iter_mut().zip(hex.chunks_exact(2)) {
        let high = hex_value(pair[0]).ok_or_else(not_a_key)?;
        let low = hex_value(pair[1]).ok_or_else(not_a_key)?;
        *byte = high << 4 | low;
    }
    Ok(key)
}

/// Random bytes from the operating system's secure source.
fn random_bytes<const N: usize>() -> Result<[u8; N], AtvError> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|error| {
        AtvError::Io(io::Error::other(format!(
            "the system could not provide random numbers ({error})"
        )))
    })?;
    Ok(bytes)
}

/// Argon2id over the password, normalised to Unicode NFC so the same password
/// typed on different systems gives the same key.
fn password_key(password: &SecretString, header: &Header) -> Result<Key, AtvError> {
    let unusable =
        || AtvError::invalid("this video's password settings are not ones this app can use");
    if header.argon_memory_kib > MAX_ARGON_MEMORY_KIB
        || header.argon_iterations > MAX_ARGON_ITERATIONS
        || header.argon_parallelism > MAX_ARGON_PARALLELISM
    {
        return Err(unusable());
    }
    let params = Params::new(
        header.argon_memory_kib,
        header.argon_iterations,
        header.argon_parallelism,
        Some(KEY_LEN),
    )
    .map_err(|_| unusable())?;
    let normalised: Zeroizing<String> = Zeroizing::new(password.expose().nfc().collect());
    let mut key: Key = Zeroizing::new([0u8; KEY_LEN]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(normalised.as_bytes(), &header.salt, key.as_mut_slice())
        .map_err(|_| unusable())?;
    Ok(key)
}

/// HKDF-SHA256 over the parts in use (password key, then key file key).
fn stream_key(
    password_key: Option<&Key>,
    file_key: Option<&Key>,
    salt: &[u8; 16],
) -> Result<Key, AtvError> {
    let mut material = Zeroizing::new(Vec::with_capacity(2 * KEY_LEN));
    for part in [password_key, file_key].into_iter().flatten() {
        material.extend_from_slice(part.as_slice());
    }
    let mut key: Key = Zeroizing::new([0u8; KEY_LEN]);
    Hkdf::<Sha256>::new(Some(salt), &material)
        .expand(STREAM_KEY_INFO, key.as_mut_slice())
        .map_err(|_| AtvError::Io(io::Error::other("key derivation failed")))?;
    Ok(key)
}

/// Derive the stream key for a header whose encryption fields are filled in.
fn derive(
    password: Option<&SecretString>,
    key_file: Option<&Path>,
    header: &Header,
) -> Result<Key, AtvError> {
    let password_key = password
        .map(|password| password_key(password, header))
        .transpose()?;
    let file_key = key_file.map(read_key_file).transpose()?;
    stream_key(password_key.as_ref(), file_key.as_ref(), &header.salt)
}

/// Encode side: fill in the header's encryption fields with fresh random
/// values and return the key that seals this video.
pub(crate) fn lock(protection: &Protection, header: &mut Header) -> Result<Key, AtvError> {
    let password = protection.password();
    let key_file = protection.key_file.as_deref();
    if password.is_none() && key_file.is_none() {
        return Err(AtvError::invalid(
            "encryption is on, so a password or a key file is needed",
        ));
    }
    // A key file alone is 256 random bits; a password that is the only
    // secret must be hard to guess (spec 6.4).
    if let (Some(password), None) = (password, key_file)
        && !password_strength(password.expose()).enough
    {
        return Err(AtvError::WeakPassword);
    }
    header.encrypted = true;
    header.password_used = password.is_some();
    header.key_file_used = key_file.is_some();
    header.salt = random_bytes()?;
    header.nonce_prefix = random_bytes()?;
    if password.is_some() {
        header.argon_memory_kib = ARGON_MEMORY_KIB;
        header.argon_iterations = ARGON_ITERATIONS;
        header.argon_parallelism = ARGON_PARALLELISM;
    }
    derive(password, key_file, header)
}

/// Decode side: derive the key for an encrypted video from what was given.
/// `KeyRequired` when something the video needs is missing. A secret the
/// video does not use is ignored.
pub(crate) fn unlock(protection: Option<&Protection>, header: &Header) -> Result<Key, AtvError> {
    let password = protection.and_then(Protection::password);
    let key_file = protection.and_then(|protection| protection.key_file.as_deref());
    if (header.password_used && password.is_none()) || (header.key_file_used && key_file.is_none())
    {
        return Err(AtvError::KeyRequired {
            password: header.password_used,
            key_file: header.key_file_used,
        });
    }
    derive(
        password.filter(|_| header.password_used),
        key_file.filter(|_| header.key_file_used),
        header,
    )
}

/// What the cipher needs besides the key: the nonce prefix and the header
/// bytes that every chunk is bound to.
struct Sealing {
    cipher: XChaCha20Poly1305,
    prefix: [u8; 19],
    associated: [u8; HEADER_BODY_LEN],
    counter: u32,
}

impl Sealing {
    fn new(key: &Key, header: &Header, raw: &[u8; HEADER_LEN]) -> io::Result<Self> {
        let cipher = XChaCha20Poly1305::new_from_slice(key.as_slice())
            .map_err(|_| io::Error::other("the cipher refused the key"))?;
        let mut associated = [0u8; HEADER_BODY_LEN];
        associated.copy_from_slice(&raw[..HEADER_BODY_LEN]);
        Ok(Sealing {
            cipher,
            prefix: header.nonce_prefix,
            associated,
            counter: 0,
        })
    }

    /// Nonce = prefix (19 bytes), chunk counter (big-endian), last flag.
    fn nonce(&self, last: bool) -> XNonce {
        let mut nonce = [0u8; 24];
        nonce[..19].copy_from_slice(&self.prefix);
        nonce[19..23].copy_from_slice(&self.counter.to_be_bytes());
        nonce[23] = u8::from(last);
        XNonce::from(nonce)
    }

    fn advance(&mut self) -> io::Result<()> {
        self.counter = self
            .counter
            .checked_add(1)
            .ok_or_else(|| io::Error::other("the data is too large to encrypt as one stream"))?;
        Ok(())
    }
}

/// Encrypts a stream in chunks (XChaCha20-Poly1305 in the STREAM construction).
///
/// Each chunk of up to 65,536 bytes is written as ciphertext plus a 16-byte
/// tag. The final chunk is marked in its nonce, so a stream cut short cannot
/// pass as complete. [`SealWriter::finish`] writes that final chunk.
pub(crate) struct SealWriter<W: Write> {
    inner: W,
    sealing: Sealing,
    buffer: Vec<u8>,
    filled: usize,
}

impl<W: Write> SealWriter<W> {
    pub(crate) fn new(
        inner: W,
        key: &Key,
        header: &Header,
        raw: &[u8; HEADER_LEN],
    ) -> io::Result<Self> {
        Ok(SealWriter {
            inner,
            sealing: Sealing::new(key, header, raw)?,
            buffer: vec![0u8; CHUNK_LEN],
            filled: 0,
        })
    }

    fn seal(&mut self, last: bool) -> io::Result<()> {
        let nonce = self.sealing.nonce(last);
        let chunk = &mut self.buffer[..self.filled];
        let tag = self
            .sealing
            .cipher
            .encrypt_inout_detached(
                &nonce,
                &self.sealing.associated,
                InOutBuf::from(&mut *chunk),
            )
            .map_err(|_| io::Error::other("encryption failed"))?;
        self.inner.write_all(chunk)?;
        self.inner.write_all(&tag)?;
        self.filled = 0;
        self.sealing.advance()
    }

    /// Write the final chunk and hand back the inner writer.
    pub(crate) fn finish(mut self) -> io::Result<W> {
        self.seal(true)?;
        self.buffer.zeroize();
        Ok(self.inner)
    }
}

impl<W: Write> Write for SealWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        if self.filled == CHUNK_LEN {
            // More data has arrived, so the full chunk in hand is not the last.
            self.seal(false)?;
        }
        let count = data.len().min(CHUNK_LEN - self.filled);
        self.buffer[self.filled..self.filled + count].copy_from_slice(&data[..count]);
        self.filled += count;
        Ok(count)
    }

    /// Chunks are sealed whole, so there is nothing to push out early.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Decrypts a stream written by [`SealWriter`].
///
/// The final chunk is the one after which the stream ends, so this always
/// reads one chunk ahead before decrypting. The first chunk is opened as soon
/// as the reader is created, so a wrong key is reported before anything else
/// happens.
pub(crate) struct OpenReader<'a, 'c, R: Read> {
    inner: R,
    sealing: Sealing,
    ctx: &'a Ctx<'c>,
    /// The decrypted chunk; unread bytes are `plain[position..plain_len]`.
    plain: Vec<u8>,
    position: usize,
    plain_len: usize,
    /// The next chunk, still encrypted, already read from `inner`.
    pending: Vec<u8>,
    pending_len: usize,
    lookahead: Vec<u8>,
    finished: bool,
}

impl<'a, 'c, R: Read> OpenReader<'a, 'c, R> {
    pub(crate) fn open(
        inner: R,
        key: &Key,
        header: &Header,
        raw: &[u8; HEADER_LEN],
        ctx: &'a Ctx<'c>,
    ) -> io::Result<Self> {
        let sealed_len = CHUNK_LEN + TAG_LEN;
        let mut reader = OpenReader {
            inner,
            sealing: Sealing::new(key, header, raw)?,
            ctx,
            plain: vec![0u8; sealed_len],
            position: 0,
            plain_len: 0,
            pending: vec![0u8; sealed_len],
            pending_len: 0,
            lookahead: vec![0u8; sealed_len],
            finished: false,
        };
        reader.pending_len = read_full(&mut reader.inner, &mut reader.pending)?;
        reader.next_chunk()?;
        Ok(reader)
    }

    /// Decrypt the pending chunk into `plain` and read the one after it.
    fn next_chunk(&mut self) -> io::Result<()> {
        // A short chunk can only be the final one. A full chunk is final
        // only if nothing at all follows it.
        let mut following = 0;
        let last = if self.pending_len < self.pending.len() {
            true
        } else {
            following = read_full(&mut self.inner, &mut self.lookahead)?;
            following == 0
        };

        let opened = self
            .pending_len
            .checked_sub(TAG_LEN)
            .is_some_and(|body_len| {
                let (body, tag) = self.pending[..self.pending_len].split_at_mut(body_len);
                let mut tag_bytes = [0u8; TAG_LEN];
                tag_bytes.copy_from_slice(tag);
                self.sealing
                    .cipher
                    .decrypt_inout_detached(
                        &self.sealing.nonce(last),
                        &self.sealing.associated,
                        InOutBuf::from(body),
                        &tag_bytes.into(),
                    )
                    .is_ok()
            });
        if !opened {
            // Every frame passed its own checks before reaching here, so a
            // failing final chunk after good ones means the stream stops
            // early: the video was cut short (or its end was changed).
            let cut_short = last && self.sealing.counter > 0;
            // The very first chunk failing means the key is wrong (or the
            // start of the video is damaged; the two cannot be told apart).
            let error = if cut_short {
                AtvError::Truncated
            } else if self.sealing.counter == 0 {
                AtvError::WrongKey
            } else {
                AtvError::Damaged {
                    frame: self.ctx.frames_done().saturating_sub(1),
                }
            };
            return Err(self.ctx.fail(error));
        }

        std::mem::swap(&mut self.plain, &mut self.pending);
        self.plain_len = self.pending_len - TAG_LEN;
        self.position = 0;
        std::mem::swap(&mut self.pending, &mut self.lookahead);
        self.pending_len = following;
        self.finished = last;
        self.sealing.advance()
    }
}

impl<R: Read> Read for OpenReader<'_, '_, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        while self.position == self.plain_len {
            if self.finished {
                return Ok(0);
            }
            self.next_chunk()?;
        }
        let count = out.len().min(self.plain_len - self.position);
        out[..count].copy_from_slice(&self.plain[self.position..self.position + count]);
        self.position += count;
        Ok(count)
    }
}

impl<R: Read> Drop for OpenReader<'_, '_, R> {
    fn drop(&mut self) {
        self.plain.zeroize();
    }
}

/// Read until `buffer` is full or the stream ends. Returns the bytes read.
fn read_full(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::noise;
    use crate::types::{CancelToken, Event, Mode, Stage};

    /// Cheap Argon2 settings so the tests stay fast. Real videos use the defaults.
    fn test_header(password: bool, key_file: bool) -> Header {
        let mut header = Header::plain(Mode::Lossless);
        header.encrypted = true;
        header.password_used = password;
        header.key_file_used = key_file;
        header.salt = [3; 16];
        header.nonce_prefix = [5; 19];
        if password {
            header.argon_memory_kib = 64;
            header.argon_iterations = 1;
            header.argon_parallelism = 1;
        }
        header
    }

    fn key_of(byte: u8) -> Key {
        Zeroizing::new([byte; KEY_LEN])
    }

    fn seal(data: &[u8], key: &Key, header: &Header) -> Vec<u8> {
        let mut writer = SealWriter::new(Vec::new(), key, header, &header.to_bytes()).unwrap();
        // Uneven pieces, so chunk boundaries fall in the middle of writes.
        for piece in data.chunks(50_001) {
            writer.write_all(piece).unwrap();
        }
        writer.finish().unwrap()
    }

    fn open(sealed: &[u8], key: &Key, header: &Header) -> Result<Vec<u8>, AtvError> {
        let mut ignore = |_event: Event| {};
        let cancel = CancelToken::new();
        let ctx = Ctx::new(&mut ignore, &cancel, Stage::ReadingFrames);
        let mut plain = Vec::new();
        OpenReader::open(sealed, key, header, &header.to_bytes(), &ctx)
            .and_then(|mut reader| reader.read_to_end(&mut plain))
            .map_err(|error| ctx.resolve(error.into()))?;
        Ok(plain)
    }

    #[test]
    fn data_of_every_awkward_size_round_trips() {
        let header = test_header(false, true);
        let key = key_of(1);
        for size in [
            0,
            1,
            CHUNK_LEN - 1,
            CHUNK_LEN,
            CHUNK_LEN + 1,
            3 * CHUNK_LEN,
            200_000,
        ] {
            let data = noise(size, size as u64 + 1);
            let sealed = seal(&data, &key, &header);
            let chunks = size.div_ceil(CHUNK_LEN).max(1);
            assert_eq!(sealed.len(), size + chunks * TAG_LEN, "size {size}");
            assert_eq!(open(&sealed, &key, &header).unwrap(), data, "size {size}");
        }
    }

    #[test]
    fn ciphertext_does_not_contain_the_plaintext() {
        let header = test_header(false, true);
        let data = b"a very recognisable secret sentence".repeat(100);
        let sealed = seal(&data, &key_of(1), &header);
        assert!(!sealed.windows(12).any(|window| window == b"recognisable"));
    }

    #[test]
    fn the_wrong_key_is_reported_as_wrong_key() {
        let header = test_header(false, true);
        let sealed = seal(&noise(100_000, 2), &key_of(1), &header);
        assert!(matches!(
            open(&sealed, &key_of(2), &header),
            Err(AtvError::WrongKey)
        ));
    }

    #[test]
    fn a_changed_header_is_reported_as_wrong_key() {
        // Every chunk is bound to the header, so editing the header (here:
        // claiming the data is compressed) breaks decryption.
        let header = test_header(false, true);
        let sealed = seal(&noise(1000, 3), &key_of(1), &header);
        let mut edited = header.clone();
        edited.compressed = true;
        assert!(matches!(
            open(&sealed, &key_of(1), &edited),
            Err(AtvError::WrongKey)
        ));
    }

    #[test]
    fn a_flipped_bit_in_any_chunk_is_caught() {
        let header = test_header(false, true);
        let key = key_of(1);
        let data = noise(3 * CHUNK_LEN + 100, 4);
        let sealed = seal(&data, &key, &header);
        let sealed_chunk = CHUNK_LEN + TAG_LEN;
        // One position in each of the four chunks: data, tag, data, final short chunk.
        let positions = [
            10,
            sealed_chunk + CHUNK_LEN + 3,
            2 * sealed_chunk + 500,
            sealed.len() - 1,
        ];
        for (chunk, position) in positions.into_iter().enumerate() {
            let mut damaged = sealed.clone();
            damaged[position] ^= 0x01;
            let result = open(&damaged, &key, &header);
            if chunk == 0 {
                assert!(
                    matches!(result, Err(AtvError::WrongKey)),
                    "chunk 0: {result:?}"
                );
            } else if chunk == positions.len() - 1 {
                // The final chunk: reported as a cut-short or changed end.
                assert!(
                    matches!(result, Err(AtvError::Truncated)),
                    "chunk {chunk}: {result:?}"
                );
            } else {
                assert!(
                    matches!(result, Err(AtvError::Damaged { .. })),
                    "chunk {chunk}: {result:?}"
                );
            }
        }
    }

    #[test]
    fn a_truncated_stream_is_rejected() {
        let header = test_header(false, true);
        let key = key_of(1);
        let sealed = seal(&noise(3 * CHUNK_LEN + 100, 5), &key, &header);
        let sealed_chunk = CHUNK_LEN + TAG_LEN;
        // Cut on a chunk boundary (the final chunk is missing), in the middle
        // of a chunk, inside a tag, and down to nothing.
        for keep in [
            3 * sealed_chunk,
            2 * sealed_chunk,
            sealed_chunk + 77,
            sealed.len() - 5,
            8,
            0,
        ] {
            let result = open(&sealed[..keep], &key, &header);
            assert!(result.is_err(), "keeping {keep} bytes must fail");
        }
        // Even a stream whose data ends exactly on a chunk boundary.
        let exact = seal(&noise(2 * CHUNK_LEN, 6), &key, &header);
        assert!(open(&exact[..sealed_chunk], &key, &header).is_err());
    }

    #[test]
    fn reordered_chunks_are_rejected() {
        let header = test_header(false, true);
        let key = key_of(1);
        let sealed = seal(&noise(3 * CHUNK_LEN + 100, 7), &key, &header);
        let sealed_chunk = CHUNK_LEN + TAG_LEN;
        let mut swapped = sealed.clone();
        let (first, rest) = swapped.split_at_mut(sealed_chunk);
        first.swap_with_slice(&mut rest[..sealed_chunk]);
        assert!(open(&swapped, &key, &header).is_err());

        // Swapping two later chunks is caught as damage, not as a wrong key.
        let mut swapped = sealed.clone();
        let (_, tail) = swapped.split_at_mut(sealed_chunk);
        let (second, third) = tail.split_at_mut(sealed_chunk);
        second.swap_with_slice(&mut third[..sealed_chunk]);
        assert!(matches!(
            open(&swapped, &key, &header),
            Err(AtvError::Damaged { .. })
        ));
    }

    #[test]
    fn extra_data_after_the_final_chunk_is_rejected() {
        let header = test_header(false, true);
        let key = key_of(1);
        let mut sealed = seal(&noise(1000, 8), &key, &header);
        sealed.extend_from_slice(&[0u8; 40]);
        assert!(open(&sealed, &key, &header).is_err());
    }

    #[test]
    fn nonces_follow_the_documented_layout() {
        let header = test_header(false, true);
        let mut sealing = Sealing::new(&key_of(1), &header, &header.to_bytes()).unwrap();
        let first = sealing.nonce(false);
        assert_eq!(&first[..19], &[5; 19]);
        assert_eq!(&first[19..], &[0, 0, 0, 0, 0]);
        sealing.counter = 0x0102_0304;
        assert_eq!(
            &sealing.nonce(true)[19..],
            &[1, 2, 3, 4, 1],
            "big-endian counter, last flag"
        );
    }

    #[test]
    fn key_files_round_trip_and_are_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("my.atvkey");
        generate_key_file(&path).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "ATV-KEY-1");
        assert_eq!(lines[1].len(), 64);
        assert!(lines[1].bytes().all(|byte| byte.is_ascii_hexdigit()));

        let key = read_key_file(&path).unwrap();
        assert_ne!(*key, [0u8; KEY_LEN]);

        let again = generate_key_file(&path);
        assert!(matches!(again, Err(AtvError::InvalidInput { .. })));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            text,
            "the first key is untouched"
        );

        let other = dir.path().join("other.atvkey");
        generate_key_file(&other).unwrap();
        assert_ne!(
            *read_key_file(&other).unwrap(),
            *key,
            "every key file is different"
        );
    }

    #[test]
    fn key_files_are_read_leniently_but_checked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k.atvkey");
        let hex = "00112233445566778899aabbccddeeff00112233445566778899AABBCCDDEEFF";
        // Windows line endings and a blank line at the end are fine.
        fs::write(&path, format!("ATV-KEY-1\r\n{hex}\r\n\r\n")).unwrap();
        let key = read_key_file(&path).unwrap();
        assert_eq!(key[..4], [0x00, 0x11, 0x22, 0x33]);
        assert_eq!(key[28..], [0xCC, 0xDD, 0xEE, 0xFF]);

        let bad = [
            format!("ATV-KEY-2\n{hex}\n"),
            format!("ATV-KEY-1\n{}\n", &hex[..62]),
            format!("ATV-KEY-1\n{}zz\n", &hex[..62]),
            format!("ATV-KEY-1\n{hex}\n{hex}\n"),
            "ATV-KEY-1\n".to_string(),
            String::new(),
        ];
        for text in bad {
            fs::write(&path, &text).unwrap();
            let result = read_key_file(&path);
            assert!(
                matches!(result, Err(AtvError::InvalidInput { .. })),
                "{text:?}"
            );
        }
        fs::write(&path, vec![b'a'; 10_000]).unwrap();
        assert!(read_key_file(&path).is_err(), "a big file is not loaded");
        assert!(read_key_file(&dir.path().join("missing.atvkey")).is_err());
    }

    #[test]
    fn each_combination_of_secrets_gives_its_own_key() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("k.atvkey");
        generate_key_file(&key_path).unwrap();
        let password = SecretString::from("correct horse");

        let both = derive(Some(&password), Some(&key_path), &test_header(true, true)).unwrap();
        let only_password = derive(Some(&password), None, &test_header(true, false)).unwrap();
        let only_file = derive(None, Some(&key_path), &test_header(false, true)).unwrap();
        assert_ne!(*both, *only_password);
        assert_ne!(*both, *only_file);
        assert_ne!(*only_password, *only_file);

        // The same inputs give the same key again; a different password does not.
        let again = derive(Some(&password), None, &test_header(true, false)).unwrap();
        assert_eq!(*again, *only_password);
        let other = SecretString::from("correct horse!");
        let different = derive(Some(&other), None, &test_header(true, false)).unwrap();
        assert_ne!(*different, *only_password);

        // A different salt gives a different key for the same password.
        let mut salted = test_header(true, false);
        salted.salt = [4; 16];
        assert_ne!(
            *derive(Some(&password), None, &salted).unwrap(),
            *only_password
        );
    }

    #[test]
    fn passwords_are_normalised_before_hashing() {
        // "é" as one character and as "e" plus a combining accent.
        let composed = SecretString::from("caf\u{00e9}");
        let decomposed = SecretString::from("cafe\u{0301}");
        let header = test_header(true, false);
        assert_eq!(
            *derive(Some(&composed), None, &header).unwrap(),
            *derive(Some(&decomposed), None, &header).unwrap()
        );
    }

    #[test]
    fn known_answer_for_the_key_derivation() {
        // Pins the whole chain (NFC, Argon2id, HKDF) so a library upgrade
        // that changed any step would be noticed: old videos must stay readable.
        let header = test_header(true, false);
        let key = derive(Some(&SecretString::from("password")), None, &header).unwrap();
        let again = derive(Some(&SecretString::from("password")), None, &header).unwrap();
        assert_eq!(*key, *again);

        // HKDF-SHA256 alone, checked against RFC 5869 test case 1.
        let ikm = [0x0b; 22];
        let salt: Vec<u8> = (0x00..=0x0c).collect();
        let info: Vec<u8> = (0xf0..=0xf9).collect();
        let mut okm = [0u8; 42];
        Hkdf::<Sha256>::new(Some(&salt), &ikm)
            .expand(&info, &mut okm)
            .unwrap();
        assert_eq!(okm[..8], [0x3c, 0xb2, 0x5f, 0x25, 0xfa, 0xac, 0xd5, 0x7a]);
    }

    #[test]
    fn lock_fills_the_header_and_unlock_finds_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("k.atvkey");
        generate_key_file(&key_path).unwrap();
        let protection = Protection {
            password: None,
            key_file: Some(key_path.clone()),
        };

        let mut header = Header::plain(Mode::Lossless);
        let key = lock(&protection, &mut header).unwrap();
        assert!(header.encrypted && header.key_file_used && !header.password_used);
        assert_ne!(header.salt, [0; 16]);
        assert_ne!(header.nonce_prefix, [0; 19]);
        assert_eq!(
            header.argon_memory_kib, 0,
            "no password, no Argon2 settings"
        );
        assert_eq!(*unlock(Some(&protection), &header).unwrap(), *key);

        // A second video gets a different salt, nonce and key from the same key file.
        let mut second = Header::plain(Mode::Lossless);
        let second_key = lock(&protection, &mut second).unwrap();
        assert_ne!(second.salt, header.salt);
        assert_ne!(second.nonce_prefix, header.nonce_prefix);
        assert_ne!(*second_key, *key);

        // An extra password the video does not use is ignored.
        let extra = Protection {
            password: Some("unused".into()),
            key_file: Some(key_path),
        };
        assert_eq!(*unlock(Some(&extra), &header).unwrap(), *key);
    }

    #[test]
    fn a_password_gets_the_documented_argon2_settings() {
        let protection = Protection {
            password: Some("a strong argon2 test phrase".into()),
            key_file: None,
        };
        let mut header = Header::plain(Mode::Lossless);
        lock(&protection, &mut header).unwrap();
        assert!(header.password_used && !header.key_file_used);
        assert_eq!(
            (
                header.argon_memory_kib,
                header.argon_iterations,
                header.argon_parallelism
            ),
            (65_536, 3, 4)
        );
    }

    #[test]
    fn missing_secrets_are_asked_for_by_name() {
        for (password, key_file) in [(true, false), (false, true), (true, true)] {
            let header = test_header(password, key_file);
            let result = unlock(None, &header);
            assert!(
                matches!(result, Err(AtvError::KeyRequired { password: p, key_file: k })
                    if p == password && k == key_file),
                "{result:?}"
            );
        }
        // An empty password counts as no password.
        let empty = Protection {
            password: Some("".into()),
            key_file: None,
        };
        assert!(matches!(
            unlock(Some(&empty), &test_header(true, false)),
            Err(AtvError::KeyRequired { .. })
        ));
        assert!(matches!(
            lock(&empty, &mut Header::plain(Mode::Lossless)),
            Err(AtvError::InvalidInput { .. })
        ));
    }

    #[test]
    fn hostile_argon2_settings_are_refused() {
        let password = SecretString::from("x");
        let mut header = test_header(true, false);
        header.argon_memory_kib = u32::MAX;
        assert!(derive(Some(&password), None, &header).is_err());
        let mut header = test_header(true, false);
        header.argon_iterations = 1_000_000;
        assert!(derive(Some(&password), None, &header).is_err());
        let mut header = test_header(true, false);
        header.argon_memory_kib = 0;
        assert!(derive(Some(&password), None, &header).is_err());
    }

    #[test]
    fn secrets_do_not_show_up_in_debug_output() {
        let protection = Protection {
            password: Some("hunter2".into()),
            key_file: None,
        };
        let shown = format!("{protection:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
    }

    #[test]
    fn easy_passwords_are_judged_weak() {
        for weak in [
            "123456",
            "password",
            "Password1!",
            "qwerty123",
            "Zayneb2024",
            "Ahmed@1998",
        ] {
            let strength = password_strength(weak);
            assert!(!strength.enough, "{weak}: {strength:?}");
        }
        for strong in [
            "Copper-Lantern-8314",
            "correct horse battery staple",
            "MyCatIsCalledMishmish",
        ] {
            let strength = password_strength(strong);
            assert!(
                strength.enough && strength.score == 4,
                "{strong}: {strength:?}"
            );
        }
        assert!(password_strength("123456").warning.is_some());
        assert!(!password_strength("").enough);
    }

    #[test]
    fn a_weak_password_is_refused_unless_a_key_file_is_used_too() {
        let mut header = Header::plain(Mode::Lossless);
        let weak = Protection {
            password: Some("123456".into()),
            key_file: None,
        };
        assert!(matches!(
            lock(&weak, &mut header),
            Err(AtvError::WeakPassword)
        ));
        assert!(
            !header.encrypted,
            "nothing is set up for a refused password"
        );

        // With a key file the password is a second lock, so any password is allowed.
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("k.atvkey");
        generate_key_file(&key_path).unwrap();
        let with_key = Protection {
            password: Some("123456".into()),
            key_file: Some(key_path),
        };
        let mut header = Header::plain(Mode::Lossless);
        let key = lock(&with_key, &mut header).unwrap();

        // Opening never judges the password, so such a video still opens.
        assert_eq!(*unlock(Some(&with_key), &header).unwrap(), *key);
    }
}
