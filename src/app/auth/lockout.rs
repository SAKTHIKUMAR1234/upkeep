use crate::app::store::Db;

/// Failures in a row before the account must wait.
pub const FREE_ATTEMPTS: u32 = 5;
const FIRST_WAIT_MS: i64 = 30_000;
const MAX_WAIT_MS: i64 = 15 * 60_000;

/// The wait after `failures` failures in a row: none for the first four, then 30 s doubling up to
/// 15 min.
pub fn wait_after(failures: u32) -> Option<i64> {
    let doublings = failures.checked_sub(FREE_ATTEMPTS)?.min(16);
    Some((FIRST_WAIT_MS << doublings).min(MAX_WAIT_MS))
}

/// Counts a sign-in attempt for the email before the password check, in one step with the
/// check of its wait. Returns the attempts in a row, or the milliseconds the email must still
/// wait. A right password clears the count.
pub fn begin_attempt(db: &Db, email: &str, now: i64) -> rusqlite::Result<Result<u32, i64>> {
    let counted = db.change_login_failures(email, |failures, locked_until| {
        if locked_until > now {
            return Err(locked_until - now);
        }
        let failures = failures + 1;
        Ok((failures, wait_after(failures).map_or(0, |wait| now + wait)))
    })?;
    Ok(counted.map(|(failures, _)| failures))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::store::tests::database;

    #[test]
    fn waits_start_at_the_fifth_failure_and_double_up_to_fifteen_minutes() {
        for failures in 0..5 {
            assert_eq!(wait_after(failures), None, "{failures}");
        }
        assert_eq!(wait_after(5), Some(30_000));
        assert_eq!(wait_after(6), Some(60_000));
        assert_eq!(wait_after(9), Some(480_000));
        assert_eq!(wait_after(10), Some(900_000));
        assert_eq!(wait_after(1000), Some(900_000));
    }

    #[test]
    fn attempts_lock_the_email_until_the_wait_ends() {
        let (_dir, db) = database();
        for attempt in 1..=4 {
            assert_eq!(begin_attempt(&db, "a@x.com", 1_000).unwrap(), Ok(attempt));
        }
        assert_eq!(begin_attempt(&db, "A@x.com", 1_000).unwrap(), Ok(5));
        assert_eq!(begin_attempt(&db, "a@x.com", 11_000).unwrap(), Err(20_000));
        assert_eq!(begin_attempt(&db, "a@x.com", 31_000).unwrap(), Ok(6));
        assert_eq!(begin_attempt(&db, "a@x.com", 31_000).unwrap(), Err(60_000));

        db.clear_login_failures("a@x.com").unwrap();
        assert_eq!(begin_attempt(&db, "a@x.com", 100_000).unwrap(), Ok(1));
    }

    #[test]
    fn parallel_attempts_on_one_email_stop_at_the_fifth() {
        let (_dir, db) = database();
        let db = std::sync::Arc::new(db);
        let threads: Vec<_> = (0..30)
            .map(|_| {
                let db = db.clone();
                std::thread::spawn(move || begin_attempt(&db, "victim@x.com", 1_000).unwrap())
            })
            .collect();
        let allowed = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .filter(Result::is_ok)
            .count();
        assert_eq!(allowed, FREE_ATTEMPTS as usize);
    }
}
