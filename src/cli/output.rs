use serde_json::Value;

use crate::time::format_unix;

pub fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "-".into(),
        other => other.to_string(),
    }
}

pub fn job_line(job: &Value) -> String {
    let created = job["created_at"]
        .as_i64()
        .map(format_unix)
        .unwrap_or_default();
    format!(
        "{:>5}  {:<10}  {:<24}  {}  by {}",
        text(&job["id"]),
        text(&job["status"]),
        text(&job["target"]),
        created,
        text(&job["requested_by"])
    )
}

pub fn print_health(report: &Value) {
    let overall = if report["ok"].as_bool() == Some(true) {
        "ok"
    } else {
        "FAILED"
    };
    println!("health: {overall}");
    for item in report["items"].as_array().into_iter().flatten() {
        let state = if item["ok"].as_bool() == Some(true) {
            "ok"
        } else {
            "FAILED"
        };
        let detail = item["detail"]
            .as_str()
            .map(|detail| format!("  ({detail})"))
            .unwrap_or_default();
        println!(
            "  {:<6}  {:<12}  {:<8}  {}{detail}",
            state,
            text(&item["adapter"]),
            text(&item["kind"]),
            text(&item["name"])
        );
    }
    print_errors(report);
}

pub fn print_adapters(listing: &Value) {
    for adapter in listing["adapters"].as_array().into_iter().flatten() {
        let actions: Vec<&String> = adapter["actions"]
            .as_object()
            .map(|map| map.keys().collect())
            .unwrap_or_default();
        let writes = if adapter["writes_data"].as_bool() == Some(true) {
            "  [writes data]"
        } else {
            ""
        };
        println!("{}{writes}", text(&adapter["name"]));
        println!(
            "  actions: {}",
            actions
                .iter()
                .map(|name| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    print_errors(listing);
}

fn print_errors(listing: &Value) {
    for error in listing["errors"].as_array().into_iter().flatten() {
        println!(
            "INVALID {}: {}",
            text(&error["path"]),
            text(&error["error"])
        );
    }
}

pub fn print_job(detail: &Value) {
    let job = &detail["job"];
    println!("{}", job_line(job));
    if let Some(error) = job["error"].as_str() {
        println!("  reason: {error}");
    }
    for step in detail["steps"].as_array().into_iter().flatten() {
        let error = step["error"]
            .as_str()
            .map(|error| format!("  ({error})"))
            .unwrap_or_default();
        println!(
            "  {:<8} {:>2}  {:<9}  {}:{}{error}",
            text(&step["phase"]),
            text(&step["index"]),
            text(&step["status"]),
            text(&step["adapter"]),
            text(&step["action"])
        );
    }
}

/// Bytes in binary units, for example `47.1 KiB`.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes_in_binary_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(48254), "47.1 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }
}
