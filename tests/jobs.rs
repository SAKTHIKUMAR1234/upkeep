mod common;

use std::time::{Duration, Instant};

use common::{Harness, statuses};
use upkeep::client::ClientError;

const SLOW: &str = "[actions.wait]\ntimeout = \"1m\"\n[actions.after]\n[actions.cleanup]\n[actions.hang]\ntimeout = \"1s\"\n";
const SLOW_SCRIPT: &str = "case \"$1\" in\n  wait) sleep 3 ;;\n  hang) sleep 30 ;;\nesac\n";

fn rejected_with(result: Result<i64, ClientError>, expected: u16) -> String {
    match result {
        Err(ClientError::Status { status, message }) if status == expected => message,
        other => panic!("expected HTTP {expected}, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn os_update_plan_reboots_and_resumes_at_the_next_step() {
    let mut harness = Harness::new();
    harness.start().await;

    let message = rejected_with(harness.run("os-update", false).await, 428);
    assert!(message.contains("needs confirmation"));

    let id = harness.run("os-update", true).await.unwrap();
    harness.wait_for_status(id, "rebooting").await;
    assert_eq!(harness.reboots(), 1);

    harness.kill();
    harness.start().await;
    let detail = harness.wait_for_status(id, "succeeded").await;

    assert_eq!(
        statuses(&detail)[..8],
        [
            "main:demo-app:maintenance_on=ok",
            "main:demo-app:backup=ok",
            "main:demo-os:snapshot=ok",
            "main:demo-os:upload=ok",
            "main:demo-os:update=reboot",
            "main:demo-app:verify=ok",
            "main:demo-os:verify=ok",
            "main:demo-app:maintenance_off=ok",
        ]
    );
    assert!(
        statuses(&detail)[8..]
            .iter()
            .all(|step| step.ends_with("=pending"))
    );
    assert_eq!(
        harness.actions("demo-app"),
        ["maintenance_on", "backup", "verify", "maintenance_off"]
    );
    assert_eq!(
        harness.actions("demo-os"),
        ["snapshot", "upload", "update", "verify"]
    );
    let backup = detail["steps"][1]["result"]["backup"].as_str().unwrap();
    assert!(backup.ends_with(&format!("backup-job{id}.txt")));

    let log =
        std::fs::read_to_string(harness.log_dir().join(format!("jobs/{id}/job.log"))).unwrap();
    assert!(log.contains("host did not reboot: continuing with the next step"));
    assert!(log.contains("[demo-app:backup] progress 50% copying data"));
}

#[tokio::test(flavor = "multi_thread")]
async fn adapters_receive_context_with_earlier_results() {
    let mut harness = Harness::new();
    harness.start().await;

    let id = harness.run("os-update", true).await.unwrap();
    harness.wait_for_status(id, "rebooting").await;

    let context_file = harness.log_dir().join(format!(
        "jobs/{id}/steps/main-3-demo-os-upload/context.json"
    ));
    let context: serde_json::Value =
        serde_json::from_slice(&std::fs::read(context_file).unwrap()).unwrap();
    assert_eq!(context["job_id"], id);
    assert_eq!(context["step"]["action"], "upload");
    assert_eq!(
        context["previous"][2]["result"]["snapshot"],
        format!("demo-snap-job{id}")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn second_update_needs_no_reboot() {
    let mut harness = Harness::new();
    std::fs::write(harness.adapter_state("demo-os").join("updated"), "").unwrap();
    harness.start().await;

    let id = harness.run("os-update", true).await.unwrap();
    harness.wait_for_status(id, "succeeded").await;
    assert_eq!(harness.reboots(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_step_runs_on_failure() {
    let mut harness = Harness::new();
    std::fs::write(harness.adapter_state("demo-app").join("fail-backup"), "").unwrap();
    harness.start().await;

    let id = harness.run("os-update", true).await.unwrap();
    let detail = harness.wait_for_status(id, "failed").await;

    assert_eq!(
        statuses(&detail),
        [
            "main:demo-app:maintenance_on=ok",
            "main:demo-app:backup=failed",
            "main:demo-os:snapshot=skipped",
            "main:demo-os:upload=skipped",
            "main:demo-os:update=skipped",
            "main:demo-app:verify=skipped",
            "main:demo-os:verify=skipped",
            "main:demo-app:maintenance_off=skipped",
            "recovery:demo-os:restore=ok",
            "recovery:demo-app:verify=ok",
            "recovery:demo-os:verify=ok",
            "recovery:demo-app:maintenance_off=ok",
        ]
    );
    assert_eq!(detail["job"]["error"], "step demo-app:backup failed");
    assert_eq!(detail["steps"][1]["exit_code"], 1);

    let log =
        std::fs::read_to_string(harness.log_dir().join(format!("jobs/{id}/job.log"))).unwrap();
    assert!(log.contains("[demo-app:backup stderr] backup failed: disk full (simulated)"));
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_stops_after_the_current_step_and_runs_on_failure() {
    let mut harness = Harness::new();
    harness.add_adapter("slow", SLOW, SLOW_SCRIPT);
    harness.add_plan(
        "slow",
        "steps = [\"slow:wait\", \"slow:after\"]\non_failure = [\"slow:cleanup\"]\n",
    );
    harness.start().await;

    let id = harness.run("slow", false).await.unwrap();
    harness.wait_for_step(id, "wait", "running").await;
    harness
        .client()
        .post(&format!("/jobs/{id}/cancel"), &serde_json::json!({}))
        .await
        .unwrap();

    let detail = harness.wait_for_status(id, "cancelled").await;
    assert_eq!(
        statuses(&detail),
        [
            "main:slow:wait=ok",
            "main:slow:after=skipped",
            "recovery:slow:cleanup=ok"
        ]
    );
    assert_eq!(harness.actions("slow"), ["wait", "cleanup"]);

    let error = harness
        .client()
        .post(&format!("/jobs/{id}/cancel"), &serde_json::json!({}))
        .await;
    assert!(matches!(
        error,
        Err(ClientError::Status { status: 409, .. })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn step_past_its_timeout_is_stopped_and_fails() {
    let mut harness = Harness::new();
    harness.add_adapter("slow", SLOW, SLOW_SCRIPT);
    harness.add_plan(
        "hang",
        "steps = [\"slow:hang\", \"slow:after\"]\non_failure = [\"slow:cleanup\"]\n",
    );
    harness.start().await;

    let started = Instant::now();
    let id = harness.run("hang", false).await.unwrap();
    let detail = harness.wait_for_status(id, "failed").await;

    assert!(started.elapsed() < Duration::from_secs(15));
    assert_eq!(detail["steps"][0]["error"], "timed out after 1s");
    assert_eq!(
        statuses(&detail),
        [
            "main:slow:hang=failed",
            "main:slow:after=skipped",
            "recovery:slow:cleanup=ok"
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn runner_killed_mid_step_reattaches_without_running_it_twice() {
    let mut harness = Harness::new();
    harness.add_adapter("slow", SLOW, SLOW_SCRIPT);
    harness.add_plan("two", "steps = [\"slow:wait\", \"slow:after\"]\n");
    harness.start().await;

    let id = harness.run("two", false).await.unwrap();
    harness.wait_for_step(id, "wait", "running").await;
    harness.kill();
    harness.start().await;

    let detail = harness.wait_for_status(id, "succeeded").await;
    assert_eq!(
        statuses(&detail),
        ["main:slow:wait=ok", "main:slow:after=ok"]
    );
    assert_eq!(harness.actions("slow"), ["wait", "after"]);

    let log =
        std::fs::read_to_string(harness.log_dir().join(format!("jobs/{id}/job.log"))).unwrap();
    assert!(log.contains("step main slow:wait re-attached"));
}

#[tokio::test(flavor = "multi_thread")]
async fn runner_down_while_step_finishes_reads_the_result() {
    let mut harness = Harness::new();
    harness.add_adapter("slow", SLOW, SLOW_SCRIPT);
    harness.start().await;

    let id = harness.run("slow:wait", false).await.unwrap();
    harness.wait_for_step(id, "wait", "running").await;
    harness.kill();
    let exit_file = harness
        .log_dir()
        .join(format!("jobs/{id}/steps/main-0-slow-wait/exit.json"));
    harness
        .wait_until("the step finishes alone", || async {
            exit_file.exists().then_some(())
        })
        .await;
    harness.start().await;

    harness.wait_for_status(id, "succeeded").await;
    assert_eq!(harness.actions("slow"), ["wait"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn only_one_job_runs_at_a_time() {
    let mut harness = Harness::new();
    harness.add_adapter("slow", SLOW, SLOW_SCRIPT);
    harness.start().await;

    let id = harness.run("slow:wait", false).await.unwrap();
    let message = rejected_with(harness.run("slow:after", false).await, 409);
    assert!(message.contains("already active"));

    harness.wait_for_status(id, "succeeded").await;
    assert!(harness.run("slow:after", false).await.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn unmet_needs_and_unknown_targets_are_rejected() {
    let mut harness = Harness::new();
    harness.start().await;

    let message = rejected_with(harness.run("demo-os:update", false).await, 422);
    assert!(message.contains("needs demo-os:snapshot"));
    rejected_with(harness.run("missing-plan", false).await, 422);
    rejected_with(harness.run("demo-os:nothing", false).await, 422);
}

#[tokio::test(flavor = "multi_thread")]
async fn log_follow_streams_until_the_job_ends() {
    let mut harness = Harness::new();
    harness.add_adapter("slow", SLOW, SLOW_SCRIPT);
    harness.start().await;

    let id = harness.run("slow:wait", false).await.unwrap();
    let output = harness
        .command("logs")
        .arg(id.to_string())
        .arg("--follow")
        .output()
        .unwrap();

    let log = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success());
    assert!(log.contains("step main slow:wait started"));
    assert!(log.trim_end().ends_with("job finished: succeeded"));
}

#[tokio::test(flavor = "multi_thread")]
async fn job_secrets_reach_the_adapter_but_never_the_log_or_database() {
    let mut harness = Harness::new();
    harness.add_adapter(
        "vault",
        "[actions.leak]\n",
        "cat > \"$UPKEEP_STATE_DIR/context.json\"\n\
         file=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))[\"secrets_file\"])' \"$UPKEEP_STATE_DIR/context.json\")\n\
         password=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))[\"ssh_password\"])' \"$file\")\n\
         echo \"stdout has $password\"\n\
         echo \"stderr has $password\" >&2\n\
         printf '{\"type\":\"result\",\"echo\":\"%s\"}\\n' \"$password\"\n",
    );
    harness.start().await;

    let request = serde_json::json!({
        "target": "vault:leak",
        "params": {"destination": {"name": "laptop"}},
        "secrets": {"ssh_password": "pa55-word-xyz"},
    });
    let started = harness.client().post("/run", &request).await.unwrap();
    let id = started["id"].as_i64().unwrap();
    let detail = harness.wait_for_status(id, "succeeded").await;

    assert_eq!(detail["job"]["params"]["destination"]["name"], "laptop");
    assert_eq!(detail["steps"][0]["result"]["echo"], "[redacted]");

    let context: serde_json::Value = serde_json::from_slice(
        &std::fs::read(harness.adapter_state("vault").join("context.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(context["params"]["destination"]["name"], "laptop");
    let secrets_file = std::path::PathBuf::from(context["secrets_file"].as_str().unwrap());
    assert!(
        !secrets_file.exists(),
        "secrets file is deleted when the job ends"
    );

    let log_dir = harness.log_dir().join(format!("jobs/{id}"));
    let log = std::fs::read_to_string(log_dir.join("job.log")).unwrap();
    assert!(log.contains("stdout has [redacted]") && log.contains("stderr has [redacted]"));
    for path in walk(&log_dir).into_iter().chain(walk(&harness.state_dir())) {
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains("pa55-word-xyz"),
            "secret found in {}",
            path.display()
        );
    }
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else if path.is_file() {
            files.push(path);
        }
    }
    files
}
