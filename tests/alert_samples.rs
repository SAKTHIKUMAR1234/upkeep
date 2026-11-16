use std::path::PathBuf;

use upkeep::monitor::checks::{Level, Observation};
use upkeep::monitor::message::digest::{HostReport, Section};
use upkeep::monitor::message::{Event, Letterhead};
use upkeep::monitor::settings::TelegramSettings;
use upkeep::monitor::state::{CheckState, Notice};
use upkeep::monitor::telegram::Telegram;
use upkeep::store::{Job, JobStatus, Phase, Step, StepStatus, UpdateCheck};
use upkeep::time::now_unix;

const MINUTE: i64 = 60;
const DAY: i64 = 86_400;

fn letterhead(host: &str) -> Letterhead {
    Letterhead {
        host: host.into(),
        offset: "+05:30".parse().unwrap(),
        repeat_every: 6 * 3600,
    }
}

fn state(level: Level, since: i64, rounds: u32) -> CheckState {
    CheckState {
        level,
        since,
        rounds,
        ..CheckState::new(since)
    }
}

fn ok(observation: Observation) -> (Observation, CheckState) {
    (observation, state(Level::Ok, 0, 1))
}

fn job(status: JobStatus, created_at: i64, finished_at: Option<i64>, error: Option<&str>) -> Job {
    Job {
        id: 1,
        target: "os-update".into(),
        status,
        phase: Phase::Main,
        end_status: None,
        cancel_requested: false,
        requested_by: "Sakthi".into(),
        error: error.map(Into::into),
        reboot_boot_id: None,
        created_at,
        finished_at,
        params: None,
    }
}

fn steps(statuses: &[StepStatus]) -> Vec<Step> {
    let names = [
        "erp:maintenance_on",
        "erp:backup",
        "os:snapshot",
        "os:update",
        "os:verify",
        "erp:maintenance_off",
    ];
    names
        .iter()
        .zip(statuses)
        .enumerate()
        .map(|(index, (name, status))| {
            let (adapter, action) = name.split_once(':').unwrap();
            Step {
                job_id: 1,
                phase: Phase::Main,
                index: index as i64,
                adapter: adapter.into(),
                action: action.into(),
                adapter_dir: String::new(),
                run: String::new(),
                timeout_secs: 600,
                status: *status,
                exit_code: None,
                result: None,
                error: (*status == StepStatus::Failed)
                    .then(|| "exit code 100: dpkg was interrupted".to_owned()),
                started_at: None,
                finished_at: None,
            }
        })
        .collect()
}

fn samples(now: i64) -> Vec<String> {
    let yrp = letterhead("YRP server (.22)");
    let replica = letterhead("Replica server (.11)");
    let mut messages = Vec::new();

    let web = Observation::new("service:essdee/Production website", Level::Critical, "down")
        .with_subject("essdee_v16_production-web");
    let critical = Notice::Alert {
        level: Level::Critical,
        repeat: false,
    };
    let down = state(Level::Critical, now - 2 * MINUTE, 2);
    messages.extend(yrp.round(
        &[Event {
            notice: &critical,
            observation: &web,
            state: &down,
        }],
        now,
    ));

    let back = Observation::new("service:essdee/Production website", Level::Ok, "up")
        .with_subject("essdee_v16_production-web");
    let fixed = Notice::Recovery {
        was: Level::Critical,
        since: now - 12 * MINUTE,
        until: now,
    };
    let up = state(Level::Ok, now, 2);
    messages.extend(yrp.round(
        &[Event {
            notice: &fixed,
            observation: &back,
            state: &up,
        }],
        now,
    ));

    let peer = Observation::new("peer:yrp", Level::Critical, "down: no answer in 10s")
        .with_subject("YRP server (.22)");
    messages.extend(replica.round(
        &[Event {
            notice: &critical,
            observation: &peer,
            state: &down,
        }],
        now,
    ));

    let disk = Observation::new("disk:/", Level::Warn, "84% used (786 GB of 936 GB)")
        .with_subject("786 GB of 936 GB")
        .with_amount(84.0);
    let reboot = Observation::new("reboot-required", Level::Warn, "a reboot is required");
    let warn = Notice::Alert {
        level: Level::Warn,
        repeat: false,
    };
    let warned = state(Level::Warn, now - 2 * MINUTE, 2);
    messages.extend(replica.round(
        &[
            Event {
                notice: &warn,
                observation: &disk,
                state: &warned,
            },
            Event {
                notice: &warn,
                observation: &reboot,
                state: &warned,
            },
        ],
        now,
    ));

    let yrp_host = HostReport {
        name: "YRP server (.22)".into(),
        checks: vec![
            ok(Observation::new("disk:/", Level::Ok, "69% used").with_amount(69.0)),
            ok(Observation::new("memory", Level::Ok, "79% used").with_amount(79.0)),
            ok(Observation::new("snapshot-age", Level::Ok, "x").with_amount(DAY as f64)),
            (
                Observation::new("reboot-required", Level::Warn, "a reboot is required"),
                state(Level::Warn, now - 2 * DAY, 2880),
            ),
        ],
        updates: Some(UpdateCheck {
            packages: 70,
            risky: 4,
            checked_at: now - 3600,
        }),
    };
    let replica_host = HostReport {
        name: "Replica server (.11)".into(),
        checks: vec![
            ok(Observation::new("disk:/", Level::Ok, "15% used").with_amount(15.0)),
            ok(Observation::new("memory", Level::Ok, "41% used").with_amount(41.0)),
        ],
        updates: Some(UpdateCheck {
            packages: 53,
            risky: 0,
            checked_at: now - 3600,
        }),
    };
    messages.push(yrp.digest(&[Section::Host(yrp_host), Section::Host(replica_host)], now));

    let pending = [StepStatus::Pending; 6];
    let running = job(JobStatus::Running, now, None, None);
    messages.push(replica.job_started(&running, &steps(&pending)));

    let done = job(JobStatus::Succeeded, now - 18 * MINUTE, Some(now), None);
    messages.push(replica.job_finished(&done, &steps(&[StepStatus::Ok; 6]), now));

    let failed_steps = [
        StepStatus::Ok,
        StepStatus::Ok,
        StepStatus::Ok,
        StepStatus::Failed,
        StepStatus::Skipped,
        StepStatus::Skipped,
    ];
    let failed = job(
        JobStatus::Failed,
        now - 9 * MINUTE,
        Some(now),
        Some("step os:update failed"),
    );
    messages.push(replica.job_finished(&failed, &steps(&failed_steps), now));
    messages
}

/// Sends one sample of each message to a real chat, for review:
/// `UPKEEP_SAMPLE_TOKEN_FILE=... UPKEEP_SAMPLE_CHAT_ID=... cargo test --test alert_samples -- --ignored`
#[tokio::test]
#[ignore = "sends to a real Telegram chat"]
async fn send_one_sample_of_each_message() {
    let (Ok(token_file), Ok(chat_id)) = (
        std::env::var("UPKEEP_SAMPLE_TOKEN_FILE"),
        std::env::var("UPKEEP_SAMPLE_CHAT_ID"),
    ) else {
        panic!("set UPKEEP_SAMPLE_TOKEN_FILE and UPKEEP_SAMPLE_CHAT_ID");
    };
    let telegram = Telegram::new(&TelegramSettings {
        token_file: PathBuf::from(token_file),
        chat_id,
        message_thread_id: None,
        api_url: "https://api.telegram.org".into(),
    })
    .unwrap();
    for text in samples(now_unix()) {
        telegram.send(&format!("[sample] {text}")).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

#[test]
fn every_sample_is_one_message() {
    let messages = samples(1_794_803_700);
    assert_eq!(messages.len(), 8);
    for message in &messages {
        assert!(message.starts_with("<b>"), "{message}");
    }
}
