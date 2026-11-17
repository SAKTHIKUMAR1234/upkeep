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

/// Milliseconds the email must still wait before the next attempt, if any.
pub fn remaining_wait(db: &Db, email: &str, now: i64) -> rusqlite::Result<Option<i64>> {
    let (_, locked_until) = db.login_failures(email)?;
    Ok((locked_until > now).then(|| locked_until - now))
}

/// Counts a failure and returns the number of failures in a row.
pub fn record_failure(db: &Db, email: &str, now: i64) -> rusqlite::Result<u32> {
    let (failures, _) = db.login_failures(email)?;
    let failures = failures + 1;
    let locked_until = wait_after(failures).map_or(0, |wait| now + wait);
    db.set_login_failures(email, failures, locked_until)?;
    Ok(failures)
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
    fn failures_lock_the_email_until_the_wait_ends() {
        let (_dir, db) = database();
        for attempt in 1..=4 {
            assert_eq!(record_failure(&db, "a@x.com", 1_000).unwrap(), attempt);
            assert_eq!(remaining_wait(&db, "a@x.com", 1_000).unwrap(), None);
        }
        assert_eq!(record_failure(&db, "A@x.com", 1_000).unwrap(), 5);
        assert_eq!(
            remaining_wait(&db, "a@x.com", 11_000).unwrap(),
            Some(20_000)
        );
        assert_eq!(remaining_wait(&db, "a@x.com", 31_000).unwrap(), None);
        assert_eq!(record_failure(&db, "a@x.com", 31_000).unwrap(), 6);
        assert_eq!(
            remaining_wait(&db, "a@x.com", 31_000).unwrap(),
            Some(60_000)
        );

        db.clear_login_failures("a@x.com").unwrap();
        assert_eq!(record_failure(&db, "a@x.com", 100_000).unwrap(), 1);
    }
}
