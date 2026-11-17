use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use serde::Serialize;
use tokio::io::AsyncWriteExt;

/// The most output one read returns.
pub const MAX_CHUNK: u64 = 1024 * 1024;

/// The output file of a run, `<data>/runs/<id>.log`, readable by its owner only.
pub struct RunLog {
    file: tokio::fs::File,
    size: u64,
}

impl RunLog {
    pub fn create(path: &Path) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            file: file.into(),
            size,
        })
    }

    /// Appends text and returns the byte range it took in the file.
    pub async fn append(&mut self, text: &str) -> std::io::Result<(u64, u64)> {
        self.file.write_all(text.as_bytes()).await?;
        // Readers of the file must see every line that an event announces.
        self.file.flush().await?;
        let start = self.size;
        self.size += text.len() as u64;
        Ok((start, self.size))
    }
}

/// A piece of a run's output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Chunk {
    pub start: u64,
    pub end: u64,
    /// The size of the whole file when it was read.
    pub size: u64,
    pub text: String,
}

/// Reads up to `limit` bytes from `offset`. With `tail`, it reads the last `tail` bytes instead,
/// from the start of a line. A chunk that is not the end of the file stops after its last full
/// line, so lines never split across chunks.
pub fn read_chunk(
    path: &Path,
    offset: u64,
    limit: u64,
    tail: Option<u64>,
) -> std::io::Result<Chunk> {
    let mut file = match std::fs::File::open(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Chunk {
                start: 0,
                end: 0,
                size: 0,
                text: String::new(),
            });
        }
        other => other?,
    };
    let size = file.metadata()?.len();
    let limit = limit.min(MAX_CHUNK);
    let mut start = match tail {
        Some(tail) => size.saturating_sub(tail.min(MAX_CHUNK)),
        None => offset.min(size),
    };
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(start))?;
    file.by_ref().take(limit).read_to_end(&mut bytes)?;

    if tail.is_some() && start > 0 {
        let line_start = bytes
            .iter()
            .position(|&byte| byte == b'\n')
            .map_or(0, |at| at + 1);
        bytes.drain(..line_start);
        start += line_start as u64;
    }
    let reached_end = start + bytes.len() as u64 >= size;
    if !reached_end && let Some(last_newline) = bytes.iter().rposition(|&byte| byte == b'\n') {
        bytes.truncate(last_newline + 1);
    }
    Ok(Chunk {
        start,
        end: start + bytes.len() as u64,
        size,
        text: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn chunks_cover_the_file_in_whole_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("1.log");
        let mut log = RunLog::create(&path).unwrap();
        assert_eq!(log.append("first line\n").await.unwrap(), (0, 11));
        assert_eq!(log.append("second\nthird\n").await.unwrap(), (11, 24));

        let chunk = read_chunk(&path, 0, 15, None).unwrap();
        assert_eq!(
            (chunk.text.as_str(), chunk.end, chunk.size),
            ("first line\n", 11, 24)
        );
        let rest = read_chunk(&path, chunk.end, 100, None).unwrap();
        assert_eq!(rest.text, "second\nthird\n");
        assert_eq!(rest.end, rest.size);

        let tail = read_chunk(&path, 0, 100, Some(9)).unwrap();
        assert_eq!((tail.start, tail.text.as_str()), (18, "third\n"));
        let past_end = read_chunk(&path, 500, 10, None).unwrap();
        assert_eq!((past_end.start, past_end.text.as_str()), (24, ""));

        let missing = read_chunk(&dir.path().join("2.log"), 0, 10, None).unwrap();
        assert_eq!(missing.size, 0);
        let mode = std::fs::metadata(&path).unwrap().permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o600
        );
    }
}
