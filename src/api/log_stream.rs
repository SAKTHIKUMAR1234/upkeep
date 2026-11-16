use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::Stream;
use tokio::io::AsyncReadExt;

use crate::engine::Runner;

const FOLLOW_INTERVAL: Duration = Duration::from_millis(300);

struct Tail {
    runner: Arc<Runner>,
    job_id: i64,
    path: PathBuf,
    file: Option<tokio::fs::File>,
    follow: bool,
    job_done: bool,
}

/// Streams the job log. With `follow`, it keeps reading until the job has finished.
pub fn job_log(
    runner: Arc<Runner>,
    job_id: i64,
    follow: bool,
) -> impl Stream<Item = std::io::Result<Bytes>> {
    let tail = Tail {
        path: runner.paths().job_log(job_id),
        runner,
        job_id,
        file: None,
        follow,
        job_done: false,
    };
    futures_util::stream::unfold(tail, |mut tail| async move {
        match tail.next_chunk().await {
            Ok(Some(chunk)) => Some((Ok(chunk), tail)),
            Ok(None) => None,
            Err(error) => Some((Err(error), tail.stopped())),
        }
    })
}

impl Tail {
    async fn next_chunk(&mut self) -> std::io::Result<Option<Bytes>> {
        let mut buffer = vec![0u8; 16 * 1024];
        loop {
            let read = self.read(&mut buffer).await?;
            if read > 0 {
                buffer.truncate(read);
                return Ok(Some(Bytes::from(buffer)));
            }
            if !self.follow || self.job_done {
                return Ok(None);
            }
            // Read once more after the job ends, to catch its last lines.
            self.job_done = self.is_job_done();
            if !self.job_done {
                tokio::time::sleep(FOLLOW_INTERVAL).await;
            }
        }
    }

    async fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.file.is_none() {
            match tokio::fs::File::open(&self.path).await {
                Ok(file) => self.file = Some(file),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
                Err(error) => return Err(error),
            }
        }
        match self.file.as_mut() {
            Some(file) => file.read(buffer).await,
            None => Ok(0),
        }
    }

    fn is_job_done(&self) -> bool {
        self.runner
            .store()
            .job(self.job_id)
            .map_or(true, |job| !job.status.is_active())
    }

    fn stopped(mut self) -> Self {
        self.follow = false;
        self.job_done = true;
        self.file = None;
        self
    }
}
