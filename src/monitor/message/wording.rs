use super::{count, escape, span};
use crate::monitor::checks::{Level, Observation};

/// The plain-language text of one check, as Telegram HTML.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wording {
    /// What is wrong, such as "Disk 84% full".
    pub title: String,
    /// The facts in one line.
    pub detail: String,
    /// What it means for users, and what to do.
    pub impact: String,
    /// The title of the recovery message.
    pub fixed: String,
    /// Something was down, so the recovery says "was down for".
    pub outage: bool,
}

impl Wording {
    fn new(title: String, detail: String, impact: &str, fixed: String) -> Self {
        Self {
            title,
            detail,
            impact: impact.to_owned(),
            fixed,
            outage: false,
        }
    }

    fn outage(mut self) -> Self {
        self.outage = true;
        self
    }
}

/// The text of every check kind. `host` is the server that runs the check.
pub fn describe(observation: &Observation, host: &str) -> Wording {
    let name = escape(observation.name());
    let subject = escape(observation.subject.as_deref().unwrap_or_default());
    let value = escape(&observation.value);
    let percent = observation
        .amount
        .map_or_else(String::new, |amount| format!("{amount:.0}%"));
    let number = observation.amount.unwrap_or_default() as usize;
    match observation.kind() {
        "service" => {
            let (adapter, service) = name.split_once('/').unwrap_or(("", &name));
            let reason = if observation.value == "down" {
                String::new()
            } else {
                format!(" ({value})")
            };
            Wording::new(
                format!("{service} is down"),
                format!("Service <code>{subject}</code> of {adapter} is not running{reason}."),
                &format!("Users of {adapter} may be affected until it runs again."),
                format!("{service} is back"),
            )
            .outage()
        }
        "ping" => {
            let ping = name.split_once('/').map_or(name.as_str(), |(_, ping)| ping);
            Wording::new(
                format!("Site {ping} not answering"),
                format!("<code>{subject}</code>: {value}."),
                "Users may not be able to open it.",
                format!("Site {ping} answers again"),
            )
            .outage()
        }
        "adapter" => Wording::new(
            "Adapter setup has an error".into(),
            format!("<code>{name}</code>: {value}."),
            "Upkeep cannot check or update this adapter until the file is fixed.",
            "Adapter setup is fixed".into(),
        ),
        "disk" => {
            let detail = match &observation.subject {
                Some(_) => format!("<code>{name}</code> has {subject} used."),
                None => format!("<code>{name}</code>: {value}."),
            };
            let impact = if observation.level == Level::Critical {
                "Sites and databases stop when the disk is full. Free space now."
            } else {
                "No user impact now. Plan a cleanup."
            };
            Wording::new(
                format!("Disk {percent} full"),
                detail,
                impact,
                format!("Disk {name} has space again"),
            )
        }
        "inodes" => Wording::new(
            format!("File slots {percent} used"),
            format!("<code>{name}</code> has used {percent} of its file slots (inodes)."),
            "No new files can be made when they run out.",
            format!("File slots on {name} are OK again"),
        ),
        "memory" => Wording::new(
            format!("Memory {percent} used"),
            "Little free memory is left.".into(),
            "The server can slow down or stop programs to free memory.",
            "Memory use is normal again".into(),
        ),
        "swap" => Wording::new(
            format!("Swap {percent} used"),
            "The server is moving memory to disk.".into(),
            "The server can be slow.",
            "Swap use is normal again".into(),
        ),
        "load" => Wording::new(
            "Server is very busy".into(),
            format!("5-minute load: {value}."),
            "Pages and jobs can be slow.",
            "Server load is normal again".into(),
        ),
        "failed-units" => {
            let detail = match &observation.subject {
                Some(units) => {
                    let units: Vec<String> = units
                        .split(", ")
                        .map(|unit| format!("<code>{}</code>", escape(unit)))
                        .collect();
                    format!("Failed: {}.", units.join(", "))
                }
                None => format!("{value}."),
            };
            Wording::new(
                format!("{} failed", count(number, "system service")),
                detail,
                "The work of these services is not done until they run again.",
                "No failed system services".into(),
            )
        }
        "reboot-required" => Wording::new(
            "Restart needed".into(),
            "Kernel updates are installed but not active yet.".into(),
            "No user impact now. Plan a restart.",
            "Restart done".into(),
        ),
        "security-updates" if observation.amount.is_none() => Wording::new(
            "Cannot check security updates".into(),
            format!("{value}."),
            "Upkeep does not know if the server is patched.",
            "Security updates can be checked again".into(),
        ),
        "security-updates" => Wording::new(
            format!("{} waiting", count(number, "security update")),
            "Security fixes are ready to install.".into(),
            "The server is not fully patched. Plan an update.",
            "Security updates are installed".into(),
        ),
        "snapshot-age" => {
            let title = match observation.amount {
                Some(age) => format!("Last snapshot {} ago", span(age as i64)),
                None => "No snapshot yet".into(),
            };
            Wording::new(
                title,
                format!("No new snapshot in time: {value}."),
                "A restore now would lose the changes since the last snapshot.",
                "Snapshots are up to date".into(),
            )
        }
        "peer" => Wording::new(
            format!("Upkeep on {subject} not answering"),
            format!("{} cannot reach Upkeep on {subject}.", escape(host)),
            "Possible reasons: server off, network down, or Upkeep stopped.",
            format!("Upkeep on {subject} answers again"),
        )
        .outage(),
        _ => Wording::new(
            format!("{} is {}", escape(&observation.key), observation.level),
            format!("{value}."),
            "Check the server.",
            format!("{} is OK again", escape(&observation.key)),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(observation: Observation) -> (String, String, String) {
        let wording = describe(&observation, "erp");
        (wording.title, wording.detail, wording.impact)
    }

    #[test]
    fn a_service_names_its_unit_and_adapter() {
        let service = Observation::new("service:frappe/web", Level::Critical, "down")
            .with_subject("frappe-web.service");
        let wording = describe(&service, "erp");
        assert_eq!(wording.title, "web is down");
        assert_eq!(
            wording.detail,
            "Service <code>frappe-web.service</code> of frappe is not running."
        );
        assert!(wording.outage);
        let container = Observation::new("service:db/mariadb", Level::Critical, "exit status 1")
            .with_subject("mariadb");
        assert!(
            describe(&container, "erp")
                .detail
                .ends_with("(exit status 1).")
        );
    }

    #[test]
    fn disk_shows_percent_and_sizes() {
        let disk = Observation::new("disk:/", Level::Warn, "84% used (786 GB of 936 GB)")
            .with_subject("786 GB of 936 GB")
            .with_amount(84.2);
        assert_eq!(
            titles(disk),
            (
                "Disk 84% full".into(),
                "<code>/</code> has 786 GB of 936 GB used.".into(),
                "No user impact now. Plan a cleanup.".into()
            )
        );
    }

    #[test]
    fn every_kind_has_its_own_words() {
        let cases = [
            ("ping:site/home", "Site home not answering"),
            ("inodes:/", "File slots 91% used"),
            ("memory", "Memory 91% used"),
            ("swap", "Swap 91% used"),
            ("load", "Server is very busy"),
            ("failed-units", "91 system services failed"),
            ("reboot-required", "Restart needed"),
            ("security-updates", "91 security updates waiting"),
            ("peer:b", "Upkeep on b not answering"),
            ("adapter:/etc/x.toml", "Adapter setup has an error"),
        ];
        for (key, title) in cases {
            let observation = Observation::new(key, Level::Warn, "x")
                .with_subject("b")
                .with_amount(91.0);
            assert_eq!(describe(&observation, "erp").title, title, "{key}");
        }
        let snapshot = Observation::new("snapshot-age", Level::Warn, "x").with_amount(90_000.0);
        assert_eq!(
            describe(&snapshot, "erp").title,
            "Last snapshot 1 day 1 hour ago"
        );
    }

    #[test]
    fn peer_down_says_who_cannot_reach_whom() {
        let peer = Observation::new("peer:erp", Level::Critical, "down: refused")
            .with_subject("YRP server (.22)");
        let wording = describe(&peer, "Replica <.11>");
        assert_eq!(
            wording.detail,
            "Replica &lt;.11&gt; cannot reach Upkeep on YRP server (.22)."
        );
        assert!(wording.impact.starts_with("Possible reasons:"));
    }

    #[test]
    fn dynamic_values_are_escaped() {
        let units = Observation::new("failed-units", Level::Critical, "x")
            .with_subject("a<b>.service, c&d.service")
            .with_amount(2.0);
        assert_eq!(
            describe(&units, "erp").detail,
            "Failed: <code>a&lt;b&gt;.service</code>, <code>c&amp;d.service</code>."
        );
        let ping = Observation::new(
            "ping:x/<i>",
            Level::Critical,
            "expected HTTP 200, got <none>",
        )
        .with_subject("http://a/?b=1&c=2");
        let wording = describe(&ping, "erp");
        assert_eq!(wording.title, "Site &lt;i&gt; not answering");
        assert_eq!(
            wording.detail,
            "<code>http://a/?b=1&amp;c=2</code>: expected HTTP 200, got &lt;none&gt;."
        );
    }
}
