use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::time::{Duration, Instant};

/// The most output one read returns.
pub const MAX_CHUNK: u64 = 1024 * 1024;
/// The shortest time between two output events of a run.
pub const BATCH_INTERVAL: Duration = Duration::from_millis(250);

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
        if text.is_empty() {
            return Ok((self.size, self.size));
        }
        self.file.write_all(text.as_bytes()).await?;
        // Readers of the file must see every line that an event announces.
        self.file.flush().await?;
        let start = self.size;
        self.size += text.len() as u64;
        Ok((start, self.size))
    }
}

/// Output that waits for the next event, so a busy terminal sends a few events a second.
#[derive(Debug, Default)]
pub struct Batch {
    text: String,
    live: Vec<String>,
    live_changed: bool,
    sent_at: Option<Instant>,
}

impl Batch {
    pub fn push_line(&mut self, line: &str) {
        self.text.push_str(line);
        self.text.push('\n');
    }

    /// The rows the terminal may still redraw.
    pub fn set_live(&mut self, live: Vec<String>) {
        if live != self.live {
            self.live = live;
            self.live_changed = true;
        }
    }

    pub fn is_pending(&self) -> bool {
        !self.text.is_empty() || self.live_changed
    }

    /// When the waiting output may go out.
    pub fn due_at(&self) -> Option<Instant> {
        self.sent_at.map(|sent_at| sent_at + BATCH_INTERVAL)
    }

    pub fn is_due(&self, now: Instant) -> bool {
        self.is_pending() && self.due_at().is_none_or(|due_at| now >= due_at)
    }

    /// The waiting text and the live rows, or `None` when nothing changed.
    pub fn take(&mut self, now: Instant) -> Option<(String, Vec<String>)> {
        if !self.is_pending() {
            return None;
        }
        self.sent_at = Some(now);
        self.live_changed = false;
        Some((std::mem::take(&mut self.text), self.live.clone()))
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

    #[test]
    fn a_batch_joins_output_until_its_interval_passes() {
        let start = Instant::now();
        let mut batch = Batch::default();
        assert!(!batch.is_due(start));
        assert_eq!(batch.take(start), None);

        batch.push_line("first");
        assert!(batch.is_due(start), "the first output goes out at once");
        assert_eq!(batch.take(start), Some(("first\n".into(), vec![])));

        let soon = start + BATCH_INTERVAL / 2;
        for percent in 0..100 {
            batch.set_live(vec![format!("{percent}%")]);
        }
        batch.push_line("second");
        batch.push_line("third");
        assert!(!batch.is_due(soon));
        assert_eq!(batch.due_at(), Some(start + BATCH_INTERVAL));

        let later = start + BATCH_INTERVAL;
        assert!(batch.is_due(later));
        assert_eq!(
            batch.take(later),
            Some(("second\nthird\n".into(), vec!["99%".into()]))
        );
        batch.set_live(vec!["99%".into()]);
        assert!(!batch.is_pending(), "unchanged live rows send nothing");
        batch.set_live(vec![]);
        assert_eq!(batch.take(later), Some((String::new(), vec![])));
    }
}
