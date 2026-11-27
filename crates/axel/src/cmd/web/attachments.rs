//! Attachments from terminal path pastes and browser image drops.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use base64::Engine;
use serde_json::{Value, json};

/// Enough for a handful of screenshots, without an unbounded prompt payload.
pub const COUNT_MAX: usize = 8;
/// Screenshots and camera images fit; larger files need an explicit resize.
const FILE_BYTES_MAX: u64 = 10 * 1024 * 1024;
/// Eight long local paths fit without parsing arbitrary pasted documents.
const PATH_TEXT_BYTES_MAX: usize = 8192;

pub fn attach(body: &Value) -> Result<Value> {
    let paths = if let Some(text) = body["paths"].as_str() {
        ensure!(text.len() <= PATH_TEXT_BYTES_MAX, "too many file paths");
        let paths = axel_tui::attach::parse_dropped_paths(text).context("drop existing files to attach them")?;
        validate(paths)?
    } else {
        vec![upload(body)?]
    };
    Ok(json!({ "attachments": paths.iter().map(|p| json!({
        "path": p, "name": p.file_name().map(|n| n.to_string_lossy()),
    })).collect::<Vec<_>>() }))
}

pub fn from_message(body: &Value) -> Result<Vec<PathBuf>> {
    let Some(value) = body.get("attachments") else { return Ok(Vec::new()) };
    let array = value.as_array().context("attachments must be file paths")?;
    ensure!(array.len() <= COUNT_MAX, "at most {COUNT_MAX} attachments");
    let paths = array.iter().map(|v| {
        let text = v.as_str().context("an attachment must be a file path")?;
        ensure!(text.len() <= PATH_TEXT_BYTES_MAX, "file path is too long");
        Ok(PathBuf::from(text))
    }).collect::<Result<Vec<_>>>()?;
    validate(paths)
}

fn validate(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    ensure!(paths.len() <= COUNT_MAX, "at most {COUNT_MAX} attachments");
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        ensure!(path.is_absolute(), "attachment paths must be absolute");
        let path = path.canonicalize().context("attachment no longer exists")?;
        let metadata = path.metadata().context("reading attachment")?;
        ensure!(metadata.is_file(), "attach a file, not a directory");
        ensure!(metadata.len() <= FILE_BYTES_MAX, "attachments must be 10 MiB or smaller");
        if !out.contains(&path) { out.push(path); }
    }
    assert!(out.len() <= COUNT_MAX);
    Ok(out)
}

fn upload(body: &Value) -> Result<PathBuf> {
    upload_at(body, &crate::paths::home().join("attachments"))
}

fn upload_at(body: &Value, directory: &Path) -> Result<PathBuf> {
    let data = body["data"].as_str().context("which image?")?;
    ensure!(data.len() <= FILE_BYTES_MAX as usize * 4 / 3 + 4, "image must be 10 MiB or smaller");
    let bytes = base64::engine::general_purpose::STANDARD.decode(data).context("invalid image encoding")?;
    ensure!(bytes.len() as u64 <= FILE_BYTES_MAX, "image must be 10 MiB or smaller");
    let extension = image_extension(&bytes).context("use a PNG, JPEG, GIF or WebP image")?;
    std::fs::create_dir_all(directory).context("creating attachment directory")?;
    // A generated name prevents a browser-supplied filename escaping the directory.
    let mut file = tempfile::Builder::new().prefix("image-").suffix(extension).tempfile_in(directory)?;
    file.write_all(&bytes).context("saving dropped image")?;
    let path = file.into_temp_path().keep().context("keeping dropped image")?;
    ensure!(path.metadata()?.len() == bytes.len() as u64, "image was not saved completely");
    Ok(path)
}

fn image_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") { return Some(".png") }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) { return Some(".jpg") }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") { return Some(".gif") }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") { return Some(".webp") }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_drops_are_canonicalized_and_bad_attachments_are_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image with spaces.png");
        std::fs::write(&path, b"image").unwrap();
        let result = attach(&json!({ "paths": format!("'{}'", path.display()) })).unwrap();
        assert_eq!(result["attachments"][0]["path"], json!(path.canonicalize().unwrap()));
        assert_eq!(from_message(&json!({ "attachments": [path, path] })).unwrap().len(), 1);
        assert!(from_message(&json!({ "attachments": [directory.path()] })).is_err());
        assert!(from_message(&json!({ "attachments": ["/a/missing/file"] })).is_err());
        assert!(from_message(&json!({ "attachments": vec!["/x"; COUNT_MAX + 1] })).is_err());
        assert!(from_message(&json!({ "attachments": "not an array" })).is_err());
        assert!(attach(&json!({ "paths": "ordinary text" })).is_err());
    }

    #[test]
    fn image_uploads_use_generated_names_and_reject_bad_or_oversized_data() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = b"\x89PNG\r\n\x1a\n";
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        let body = json!({ "name": "../../outside.png", "data": encoded });
        let path = upload_at(&body, directory.path()).unwrap();
        assert_eq!(path.parent(), Some(directory.path()));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(upload_at(&json!({ "data": "!" }), directory.path()).is_err());
        assert!(upload_at(&json!({ "data": "aGVsbG8=" }), directory.path()).is_err());
        let large = directory.path().join("too-large.png");
        std::fs::File::create(&large).unwrap().set_len(FILE_BYTES_MAX + 1).unwrap();
        assert!(from_message(&json!({ "attachments": [large] })).is_err());
        assert_eq!(image_extension(b"GIF89a"), Some(".gif"));
        assert_eq!(image_extension(b"RIFF1234WEBP"), Some(".webp"));
        assert_eq!(image_extension(&[0xff, 0xd8, 0xff]), Some(".jpg"));
    }
}
