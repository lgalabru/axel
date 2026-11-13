//! Turning a prompt's text and file attachments into ACP content blocks.

use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1 as acp;
use base64::Engine;

/// Inline images larger than this are sent as links instead.
pub const MAX_INLINE_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// What the agent said it accepts in `session/prompt` (from `initialize`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PromptCaps {
    pub image: bool,
    pub embedded_context: bool,
}

impl From<&acp::PromptCapabilities> for PromptCaps {
    fn from(c: &acp::PromptCapabilities) -> Self {
        PromptCaps {
            image: c.image,
            embedded_context: c.embedded_context,
        }
    }
}

/// Build the blocks for one prompt: the text first (if non-empty), then one
/// block per attachment (inline `image` when the agent accepts images and the
/// file is a small enough image, else a `resource_link`), then a text block
/// listing the attached paths so agents without fs access still know them.
/// `warn` receives one line per attachment that could not be inlined/stat'ed.
pub fn build_prompt_blocks(
    text: &str,
    attachments: &[PathBuf],
    caps: PromptCaps,
    warn: &mut dyn FnMut(String),
) -> Vec<acp::ContentBlock> {
    let mut blocks = Vec::new();
    if !text.is_empty() {
        blocks.push(acp::ContentBlock::Text(acp::TextContent::new(text)));
    }
    if attachments.is_empty() {
        return blocks;
    }
    let mut listed = Vec::new();
    for path in attachments {
        let abs = absolutize(path);
        listed.push(abs.display().to_string());
        let mime = mime_for(&abs);
        let size = std::fs::metadata(&abs).map(|m| m.len()).ok();
        if size.is_none() {
            warn(format!("attachment not found: {}", abs.display()));
        }
        if caps.image
            && is_image(&abs)
            && let Some(len) = size
        {
            if len > MAX_INLINE_IMAGE_BYTES {
                warn(format!(
                    "{} is {len} bytes (> {MAX_INLINE_IMAGE_BYTES}); sending as a link",
                    abs.display()
                ));
            } else {
                match std::fs::read(&abs) {
                    Ok(bytes) => {
                        let data = base64::prelude::BASE64_STANDARD.encode(bytes);
                        blocks.push(acp::ContentBlock::Image(
                            acp::ImageContent::new(data, mime.unwrap_or("image/*"))
                                .uri(file_uri(&abs)),
                        ));
                        continue;
                    }
                    Err(e) => warn(format!(
                        "cannot read {}: {e}; sending as a link",
                        abs.display()
                    )),
                }
            }
        }
        let name = abs
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| abs.display().to_string());
        let mut link = acp::ResourceLink::new(name, file_uri(&abs));
        if let Some(m) = mime {
            link = link.mime_type(m.to_string());
        }
        if let Some(len) = size {
            link = link.size(len as i64);
        }
        blocks.push(acp::ContentBlock::ResourceLink(link));
    }
    let mut listing = String::from("Attached files:");
    for p in &listed {
        listing.push_str("\n- ");
        listing.push_str(p);
    }
    blocks.push(acp::ContentBlock::Text(acp::TextContent::new(listing)));
    blocks
}

/// Absolute path strings for the `attachments` field of a UserMessage item.
pub fn attachment_paths(attachments: &[PathBuf]) -> Vec<String> {
    attachments
        .iter()
        .map(|p| absolutize(p).display().to_string())
        .collect()
}

fn absolutize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    }
}

fn file_uri(abs: &Path) -> String {
    format!("file://{}", abs.display())
}

fn ext(p: &Path) -> String {
    p.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// Whether the file is an image by extension (png/jpg/jpeg/gif/webp).
pub fn is_image(p: &Path) -> bool {
    matches!(ext(p).as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp")
}

/// A MIME type guessed from the extension, if we know one.
pub fn mime_for(p: &Path) -> Option<&'static str> {
    Some(match ext(p).as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "txt" | "log" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "json" => "application/json",
        "yaml" | "yml" => "application/yaml",
        "toml" => "application/toml",
        "xml" => "application/xml",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "csv" => "text/csv",
        "pdf" => "application/pdf",
        "rs" => "text/x-rust",
        "py" => "text/x-python",
        "js" | "mjs" => "text/javascript",
        "ts" => "text/typescript",
        "sh" => "text/x-shellscript",
        "swift" => "text/x-swift",
        "go" => "text/x-go",
        "zip" => "application/zip",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(blocks: &[acp::ContentBlock]) -> Vec<&'static str> {
        blocks
            .iter()
            .map(|b| match b {
                acp::ContentBlock::Text(_) => "text",
                acp::ContentBlock::Image(_) => "image",
                acp::ContentBlock::ResourceLink(_) => "link",
                _ => "other",
            })
            .collect()
    }

    #[test]
    fn text_only() {
        let mut warns = Vec::new();
        let b = build_prompt_blocks("hi", &[], PromptCaps::default(), &mut |w| warns.push(w));
        assert_eq!(kinds(&b), ["text"]);
        assert!(build_prompt_blocks("", &[], PromptCaps::default(), &mut |_| {}).is_empty());
        assert!(warns.is_empty());
    }

    #[test]
    fn images_inline_only_when_supported() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("shot.png");
        std::fs::write(&png, b"\x89PNG....").unwrap();
        let notes = dir.path().join("notes.md");
        std::fs::write(&notes, "# hi").unwrap();
        let files = vec![png.clone(), notes.clone()];

        let b = build_prompt_blocks(
            "look",
            &files,
            PromptCaps {
                image: true,
                embedded_context: false,
            },
            &mut |_| {},
        );
        assert_eq!(kinds(&b), ["text", "image", "link", "text"]);
        let acp::ContentBlock::Image(img) = &b[1] else {
            unreachable!()
        };
        assert_eq!(img.mime_type, "image/png");
        assert_eq!(
            img.data,
            base64::prelude::BASE64_STANDARD.encode(b"\x89PNG....")
        );
        assert_eq!(
            img.uri.as_deref(),
            Some(format!("file://{}", png.display()).as_str())
        );
        let acp::ContentBlock::ResourceLink(link) = &b[2] else {
            unreachable!()
        };
        assert_eq!(link.name, "notes.md");
        assert_eq!(link.uri, format!("file://{}", notes.display()));
        assert_eq!(link.mime_type.as_deref(), Some("text/markdown"));
        assert_eq!(link.size, Some(4));
        let acp::ContentBlock::Text(t) = &b[3] else {
            unreachable!()
        };
        assert_eq!(
            t.text,
            format!(
                "Attached files:\n- {}\n- {}",
                png.display(),
                notes.display()
            )
        );

        let b = build_prompt_blocks("look", &files, PromptCaps::default(), &mut |_| {});
        assert_eq!(kinds(&b), ["text", "link", "link", "text"]);
        let acp::ContentBlock::ResourceLink(link) = &b[1] else {
            unreachable!()
        };
        assert_eq!(link.mime_type.as_deref(), Some("image/png"));
    }

    #[test]
    fn oversized_images_become_links_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.jpg");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(MAX_INLINE_IMAGE_BYTES + 1).unwrap();
        let mut warns = Vec::new();
        let b = build_prompt_blocks(
            "",
            std::slice::from_ref(&big),
            PromptCaps {
                image: true,
                embedded_context: true,
            },
            &mut |w| warns.push(w),
        );
        assert_eq!(kinds(&b), ["link", "text"]);
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("sending as a link"), "{warns:?}");
    }

    #[test]
    fn missing_files_still_get_linked_and_warned() {
        let mut warns = Vec::new();
        let p = PathBuf::from("/definitely/missing.pdf");
        let b = build_prompt_blocks(
            "x",
            std::slice::from_ref(&p),
            PromptCaps {
                image: true,
                embedded_context: false,
            },
            &mut |w| warns.push(w),
        );
        assert_eq!(kinds(&b), ["text", "link", "text"]);
        let acp::ContentBlock::ResourceLink(link) = &b[1] else {
            unreachable!()
        };
        assert_eq!(link.mime_type.as_deref(), Some("application/pdf"));
        assert_eq!(link.size, None);
        assert_eq!(warns, vec!["attachment not found: /definitely/missing.pdf"]);
    }

    #[test]
    fn mime_guesses() {
        assert_eq!(mime_for(Path::new("a.JSON")), Some("application/json"));
        assert_eq!(mime_for(Path::new("a.txt")), Some("text/plain"));
        assert_eq!(mime_for(Path::new("a.unknownext")), None);
        assert!(is_image(Path::new("x.WebP")));
        assert!(!is_image(Path::new("x.svg")));
        assert_eq!(
            attachment_paths(&[PathBuf::from("/a/b.rs")]),
            vec!["/a/b.rs"]
        );
    }
}
