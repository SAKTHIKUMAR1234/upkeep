use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::Mutex;

/// Sign-ins one client may make within `WINDOW_MS`: failed ones plus the ones still running.
pub const MAX_FAILURES: usize = 10;
pub const WINDOW_MS: i64 = 15 * 60_000;
/// Sign-ins one client may have running at the same time.
pub const MAX_IN_FLIGHT: usize = 2;
/// The wait for a client that has `MAX_IN_FLIGHT` sign-ins running.
const BUSY_WAIT_MS: i64 = 1_000;

/// What the throttle counts as one client: an IPv4 address, or the /64 network of an IPv6
/// address, because one IPv6 host can use every address of its /64.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Client(IpAddr);

impl Client {
    pub fn of(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => Self(IpAddr::V4(v4)),
                None => Self(IpAddr::V6(Ipv6Addr::from_bits(
                    v6.to_bits() & !u128::from(u64::MAX),
                ))),
            },
            v4 => Self(v4),
        }
    }
}

/// `203.0.113.5` or `2001:db8:1:2::/64`, the form fail2ban bans.
impl fmt::Display for Client {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            IpAddr::V4(v4) => write!(formatter, "{v4}"),
            IpAddr::V6(v6) => write!(formatter, "{v6}/64"),
        }
    }
}

#[derive(Debug, Default)]
struct Attempts {
    failures: VecDeque<i64>,
    in_flight: usize,
}

/// Sign-ins per client, in memory. It adds to the per-account lockout, so one client cannot
/// try many accounts.
#[derive(Debug, Default)]
pub struct IpThrottle {
    clients: Mutex<HashMap<Client, Attempts>>,
}

/// A running sign-in of a client. Dropped, it frees its place; [`Attempt::fail`] keeps it as a
/// failure.
#[derive(Debug)]
pub struct Attempt<'a> {
    throttle: &'a IpThrottle,
    client: Client,
}

impl IpThrottle {
    /// Counts a running sign-in for the client in one step with the check, or returns the
    /// milliseconds the client must wait. Failures older than the window are forgotten.
    pub fn begin(&self, client: Client, now: i64) -> Result<Attempt<'_>, i64> {
        let mut clients = self.clients.lock().expect("throttle lock");
        clients.retain(|_, attempts| {
            attempts.failures.retain(|time| time + WINDOW_MS > now);
            !attempts.failures.is_empty() || attempts.in_flight > 0
        });
        let attempts = clients.entry(client).or_default();
        if attempts.failures.len() + attempts.in_flight >= MAX_FAILURES {
            let oldest = attempts.failures.front().copied().unwrap_or(now);
            return Err((oldest + WINDOW_MS - now).max(BUSY_WAIT_MS));
        }
        if attempts.in_flight >= MAX_IN_FLIGHT {
            return Err(BUSY_WAIT_MS);
        }
        attempts.in_flight += 1;
        Ok(Attempt {
            throttle: self,
            client,
        })
    }
}

impl Attempt<'_> {
    pub fn fail(self, now: i64) {
        let mut clients = self.throttle.clients.lock().expect("throttle lock");
        clients
            .entry(self.client)
            .or_default()
            .failures
            .push_back(now);
    }
}

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        let mut clients = self.throttle.clients.lock().expect("throttle lock");
        if let Some(attempts) = clients.get_mut(&self.client) {
            attempts.in_flight -= 1;
            if attempts.in_flight == 0 && attempts.failures.is_empty() {
                clients.remove(&self.client);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(ip: &str) -> Client {
        Client::of(ip.parse().unwrap())
    }

    #[test]
    fn a_client_waits_after_ten_failures_until_the_oldest_leaves_the_window() {
        let throttle = IpThrottle::default();
        let (ip, other) = (client("203.0.113.5"), client("203.0.113.6"));
        for minute in 0..10 {
            throttle
                .begin(ip, minute * 60_000)
                .unwrap()
                .fail(minute * 60_000);
        }
        assert_eq!(throttle.begin(ip, 10 * 60_000).unwrap_err(), 5 * 60_000);
        assert!(throttle.begin(other, 10 * 60_000).is_ok());
        assert!(throttle.begin(ip, WINDOW_MS).is_ok());

        drop(throttle.begin(ip, 40 * 60_000));
        assert_eq!(throttle.clients.lock().unwrap().len(), 0);
    }

    #[test]
    fn running_sign_ins_count_before_they_fail() {
        let throttle = IpThrottle::default();
        let ip = client("203.0.113.5");
        let first = throttle.begin(ip, 0).unwrap();
        let second = throttle.begin(ip, 0).unwrap();
        assert_eq!(throttle.begin(ip, 0).unwrap_err(), BUSY_WAIT_MS);
        drop(first);
        second.fail(0);
        for _ in 0..9 {
            throttle.begin(ip, 0).unwrap().fail(0);
        }
        assert_eq!(throttle.begin(ip, 0).unwrap_err(), WINDOW_MS);

        let throttle = IpThrottle::default();
        for _ in 0..9 {
            throttle.begin(ip, 0).unwrap().fail(0);
        }
        let last = throttle.begin(ip, 0).unwrap();
        assert!(
            throttle.begin(ip, 0).is_err(),
            "the running one fills the limit"
        );
        drop(last);
        assert!(throttle.begin(ip, 0).is_ok(), "a success frees its place");
    }

    #[test]
    fn ipv6_counts_per_64_network_and_mapped_ipv4_as_ipv4() {
        assert_eq!(
            client("2001:db8:1:2:aaaa::1"),
            client("2001:db8:1:2:ffff:ffff:ffff:ffff")
        );
        assert_ne!(client("2001:db8:1:2::1"), client("2001:db8:1:3::1"));
        assert_eq!(client("::ffff:203.0.113.5"), client("203.0.113.5"));
        assert_eq!(
            client("2001:db8:1:2:aaaa::1").to_string(),
            "2001:db8:1:2::/64"
        );
        assert_eq!(client("203.0.113.5").to_string(), "203.0.113.5");

        let throttle = IpThrottle::default();
        for host in 0..10 {
            let rotated = client(&format!("2001:db8:1:2::{host:x}"));
            throttle.begin(rotated, 0).unwrap().fail(0);
        }
        assert!(throttle.begin(client("2001:db8:1:2::ffff"), 0).is_err());
    }
}
