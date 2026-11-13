use std::path::PathBuf;

/// Errors produced by `axel-config`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A filesystem operation failed; `path` is the file or directory involved.
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The YAML frontmatter could not be deserialized.
    #[error("invalid YAML frontmatter: {0}")]
    Yaml(#[from] serde_norway::Error),
    /// The text started with a `---` line but no closing `---` line followed.
    #[error("frontmatter opened with `---` but never closed")]
    UnterminatedFrontmatter,
    /// A parse error, annotated with the file it came from.
    #[error("{path}: {source}")]
    Config {
        path: PathBuf,
        #[source]
        source: Box<Error>,
    },
    /// The configuration parsed but is semantically invalid.
    #[error("invalid configuration: {0}")]
    Invalid(String),
    /// A `git` invocation failed or `git` is not installed.
    #[error("git: {0}")]
    Git(String),
    /// Symbolic links are required but not available on this platform.
    #[error("symbolic links are not supported on this platform")]
    SymlinkUnsupported,
}

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }

    pub(crate) fn at(self, path: impl Into<PathBuf>) -> Self {
        Error::Config {
            path: path.into(),
            source: Box::new(self),
        }
    }
}
