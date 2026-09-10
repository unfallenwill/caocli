//! Attaching an image to a message: reading it, naming its media type, and
//! encoding it as the data URL the API takes.
//!
//! The bytes travel inside the message itself, base64-encoded, so the session
//! log is the whole of the state: a resumed session sends the same bytes it sent
//! the first time -- the prefix cache goes on matching -- and an image that was
//! attached once does not depend on the file still being there.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::types::Message;

/// Largest image that may be attached, in bytes of the file. A screenshot is
/// hundreds of kilobytes; a file this size is either not an image or not one
/// anybody meant to send, and base64 makes it a third larger again on the wire.
pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

/// What an attached image is, as far as the message says: its format and how many
/// bytes of image there are. The path it was read from is not part of what was
/// sent, so a resumed session cannot show it either, and neither can this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// The format as a person names it: `png`, `jpeg`, `webp`, `gif`.
    pub format: String,
    pub bytes: usize,
}

/// The user message for `text` with the images at `paths` attached. No path, no
/// parts: a message with no image is the plain string it has always been.
pub fn user_message(text: &str, paths: &[PathBuf]) -> Result<Message> {
    let mut urls = Vec::with_capacity(paths.len());
    for path in paths {
        let (bytes, media_type) = read_image(path)?;
        urls.push(format!("data:{media_type};base64,{}", base64(&bytes)));
    }
    Ok(Message::user_with_images(text, urls))
}

/// What a message's image part says about itself, for the transcript.
///
/// `None` for a URL this program would not have written -- a remote one, say:
/// the transcript shows what it can and never guesses.
pub fn note(url: &str) -> Option<Note> {
    let (media_type, payload) = url.strip_prefix("data:")?.split_once(";base64,")?;
    if !media_type.starts_with("image/") {
        return None;
    }
    Some(Note {
        format: short_name(media_type).to_owned(),
        bytes: decoded_len(payload),
    })
}

/// Read `path` and name what it holds. The media type is sniffed from the bytes
/// rather than taken from the file's name: the name says what its author meant,
/// the bytes say what the backend will be sent.
fn read_image(path: &Path) -> Result<(Vec<u8>, &'static str)> {
    let size = std::fs::metadata(path)
        .with_context(|| format!("failed to read image {}", path.display()))?
        .len();
    if size > MAX_IMAGE_BYTES {
        bail!(
            "{} is {size} bytes; an attached image may be at most {MAX_IMAGE_BYTES} bytes",
            path.display()
        );
    }
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read image {}", path.display()))?;
    let Some(media_type) = media_type(&bytes) else {
        bail!("{} is not a PNG, JPEG, WebP or GIF image", path.display());
    };
    Ok((bytes, media_type))
}

/// The media type of an image, from its leading bytes.
fn media_type(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        // "RIFF" <4-byte size> "WEBP"
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some("image/webp"),
        _ => None,
    }
}

/// What a media type is called in one word: the part after the slash, which is
/// what a person reads the format as.
fn short_name(media_type: &str) -> &str {
    media_type.rsplit('/').next().unwrap_or(media_type)
}

/// How many bytes a base64 payload decodes to, without decoding it: every four
/// characters carry three bytes, less one for each `=`.
fn decoded_len(payload: &str) -> usize {
    let padding = payload.bytes().rev().take_while(|&b| b == b'=').count();
    payload.len() / 4 * 3 - padding
}

/// The standard base64 alphabet, padded: what a data URL carries.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Base64-encode `data`. Hand-written rather than pulled in: it is twenty lines
/// with the RFC's own test vectors pinning them, and this program has one caller.
fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let group = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let bits = u32::from(group[0]) << 16 | u32::from(group[1]) << 8 | u32::from(group[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            // A group of one byte carries two characters and a group of two
            // carries three; the rest of the quartet is padding.
            if i > chunk.len() {
                out.push('=');
            } else {
                out.push(ALPHABET[(bits >> shift & 63) as usize] as char);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PNG's leading bytes, so a file in a test is what its name says.
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];

    fn tmpdir(tag: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "caocli-image-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        // RFC 4648 section 10.
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes()), encoded, "{plain:?}");
        }
    }

    #[test]
    fn base64_covers_every_byte_without_a_character_of_its_own() {
        let bytes: Vec<u8> = (0..=255u8).collect();
        let encoded = base64(&bytes);
        assert_eq!(encoded.len(), 344); // 256 bytes -> 86 groups, the last padded
        assert!(encoded.ends_with('='));
        assert!(
            encoded
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
        );
        // The alphabet is the standard one if the first and the last byte of the
        // range encode to the first and the last character of it.
        assert_eq!(&encoded[..4], "AAEC");
        assert_eq!(encoded.chars().filter(|c| *c == '=').count(), 2);
    }

    #[test]
    fn media_type_is_sniffed_from_the_bytes() {
        assert_eq!(media_type(PNG), Some("image/png"));
        assert_eq!(media_type(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(media_type(b"GIF89a......"), Some("image/gif"));
        assert_eq!(
            media_type(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            Some("image/webp")
        );
        // A text file, an empty file, and a RIFF file that is not a WebP.
        assert_eq!(media_type(b"#!/bin/sh\n"), None);
        assert_eq!(media_type(b""), None);
        assert_eq!(media_type(b"RIFF\x00\x00\x00\x00WAVEfmt "), None);
    }

    #[test]
    fn a_message_carries_the_image_as_a_data_url() {
        let dir = tmpdir("attach");
        let path = dir.join("shot.png");
        std::fs::write(&path, PNG).unwrap();
        let message = user_message("what is this?", std::slice::from_ref(&path)).unwrap();
        assert_eq!(message.role, crate::types::Role::User);
        let content = message.content.as_ref().unwrap();
        assert_eq!(content.text(), "what is this?");
        let images = content.images();
        assert_eq!(images.len(), 1);
        assert_eq!(
            images[0],
            format!("data:image/png;base64,{}", base64(PNG)),
            "the message carries the file's own bytes"
        );
        // What the transcript reads back out of it.
        assert_eq!(
            note(images[0]),
            Some(Note {
                format: "png".into(),
                bytes: PNG.len()
            })
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_message_without_images_keeps_the_string_form() {
        let message = user_message("just text", &[]).unwrap();
        assert_eq!(message, Message::user("just text"));
        let json = serde_json::to_string(&message).unwrap();
        assert_eq!(json, r#"{"role":"user","content":"just text"}"#);
    }

    #[test]
    fn a_message_with_an_image_without_text_is_only_parts() {
        let dir = tmpdir("no-text");
        let path = dir.join("shot.jpg");
        std::fs::write(&path, [0xFF, 0xD8, 0xFF, 0xDB]).unwrap();
        let message = user_message("", &[path]).unwrap();
        let json = serde_json::to_value(&message).unwrap();
        let parts = json["content"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["type"], "image_url");
        assert!(
            parts[0]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/jpeg;base64,")
        );
        assert_eq!(message.content.as_ref().unwrap().text(), "");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn text_comes_before_the_images_it_is_about() {
        let dir = tmpdir("order");
        let path = dir.join("a.png");
        std::fs::write(&path, PNG).unwrap();
        let message = user_message("look", &[path]).unwrap();
        let json = serde_json::to_value(&message).unwrap();
        let parts = json["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "look");
        assert_eq!(parts[1]["type"], "image_url");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_file_is_an_error_naming_it() {
        let dir = tmpdir("missing");
        let err = user_message("hi", &[dir.join("nope.png")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("failed to read image"), "{err}");
        assert!(err.contains("nope.png"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_that_is_not_an_image_is_refused() {
        let dir = tmpdir("notimage");
        let path = dir.join("notes.txt");
        std::fs::write(&path, "hello\n").unwrap();
        let err = user_message("hi", &[path]).unwrap_err().to_string();
        assert!(err.contains("not a PNG, JPEG, WebP or GIF"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_file_is_refused() {
        let dir = tmpdir("empty");
        let path = dir.join("empty.png");
        std::fs::write(&path, b"").unwrap();
        let err = user_message("hi", &[path]).unwrap_err().to_string();
        assert!(err.contains("not a PNG, JPEG, WebP or GIF"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_oversized_file_is_refused_before_it_is_read() {
        let dir = tmpdir("too-big");
        let path = dir.join("huge.png");
        // Sparse: the size is what is checked, and writing 10 MiB is a waste.
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_IMAGE_BYTES + 1).unwrap();
        drop(file);
        let err = user_message("hi", &[path]).unwrap_err().to_string();
        assert!(err.contains("may be at most"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn note_reads_a_data_url_and_declines_anything_else() {
        assert_eq!(
            note("data:image/png;base64,Zm9vYmFy"),
            Some(Note {
                format: "png".into(),
                bytes: 6
            })
        );
        // Padding is not part of the bytes.
        assert_eq!(
            note("data:image/jpeg;base64,Zm9vYg=="),
            Some(Note {
                format: "jpeg".into(),
                bytes: 4
            })
        );
        for bad in [
            "https://example.com/cat.png",
            "data:image/png,Zm9v",
            "",
            "data:;base64,",
            "data:text/plain;base64,Zm9v",
        ] {
            assert_eq!(note(bad), None, "{bad}");
        }
    }
}
