//! Swarm coordination view: the data model + loader behind the [2] Swarm AI /
//! [3] Results tabs. Extracted from main.rs (god-file split).
//!
//! Pure local-file reads of the `/tmp/flux-swarm*.json|jsonl` snapshots the
//! Claude Code sessions write — tolerant of missing/partial files, no chain
//! state, no `super::` deps. tabs_ui.rs renders these structs (via `super::*`),
//! so the types and their fields are `pub(crate)`.

#[derive(Default, Clone)]
pub(crate) struct SwarmAgent { pub(crate) id: String, pub(crate) status: String, pub(crate) qug: f64 }
#[derive(Default, Clone)]
pub(crate) struct SwarmClaim { pub(crate) agent: String, pub(crate) path: String, pub(crate) note: String }
#[derive(Default, Clone)]
pub(crate) struct SwarmActivity { pub(crate) agent: String, pub(crate) kind: String, pub(crate) detail: String, pub(crate) at: u64 }
#[derive(Default, Clone)]
pub(crate) struct SwarmResult { pub(crate) agent: String, pub(crate) task_id: String, pub(crate) qug: f64, pub(crate) crates: String, pub(crate) success: bool, pub(crate) at: u64 }
#[derive(Default, Clone)]
pub(crate) struct SwarmTask { pub(crate) task_id: String, pub(crate) agent: String, pub(crate) crates: String, pub(crate) priority: i64, pub(crate) est_qug: f64 }
#[derive(Default, Clone)]
pub(crate) struct SwarmMsg { pub(crate) from: String, pub(crate) text: String, pub(crate) at: u64 }

/// A snapshot of the swarm coordination files written by the Claude Code sessions
/// (/tmp/flux-swarm*.json|jsonl). Drives the [2] Swarm AI + [3] Results tabs.
#[derive(Default, Clone)]
pub(crate) struct SwarmView {
    pub(crate) agents: Vec<SwarmAgent>,
    pub(crate) claims: Vec<SwarmClaim>,
    pub(crate) tasks: Vec<SwarmTask>,        // v0.14: swarm task board (priority + QUG bounty)
    pub(crate) feed: Vec<SwarmMsg>,          // v0.14: recent broadcast coordination, newest-first
    pub(crate) activity: Vec<SwarmActivity>, // newest-first
    pub(crate) results: Vec<SwarmResult>,    // newest-first
    pub(crate) completed_count: u64,
    pub(crate) qug_paid: f64,
    pub(crate) err: Option<String>,
}

fn swarm_dir() -> String { std::env::var("SIGIL_SWARM_DIR").unwrap_or_else(|_| "/tmp".into()) }

/// Read + parse the swarm coordination files into a SwarmView. Cheap local file
/// reads; tolerant of missing/partial files (off-box → shows a hint).
pub(crate) fn load_swarm_view() -> SwarmView {
    let dir = swarm_dir();
    let mut v = SwarmView::default();
    let mut any = false;
    if let Ok(s) = std::fs::read_to_string(format!("{dir}/flux-swarm.json")) {
        if let Ok(j) = serde_json::from_str::<serde_json::Value>(&s) {
            any = true;
            v.completed_count = j.get("completed_count").and_then(|x| x.as_u64()).unwrap_or(0);
            v.qug_paid = j.get("qug_paid").and_then(|x| x.as_f64()).unwrap_or(0.0);
            if let Some(ags) = j.get("agents").and_then(|x| x.as_object()) {
                for (id, a) in ags {
                    v.agents.push(SwarmAgent {
                        id: id.clone(),
                        status: a.get("status").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                        qug: a.get("total_earned_qug").and_then(|x| x.as_f64()).unwrap_or(0.0),
                    });
                }
                v.agents.sort_by(|a, b| b.qug.partial_cmp(&a.qug).unwrap_or(std::cmp::Ordering::Equal));
            }
            // v0.14: swarm task board — claims[] carry priority + QUG bounty.
            if let Some(cl) = j.get("claims").and_then(|x| x.as_array()) {
                for c in cl {
                    let agent = c.get("agent").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    if agent.starts_with("test_") { continue; }
                    v.tasks.push(SwarmTask {
                        task_id: c.get("task_id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                        agent,
                        crates: c.get("crates").and_then(|x| x.as_array())
                            .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(","))
                            .unwrap_or_default(),
                        priority: c.get("priority").and_then(|x| x.as_i64()).unwrap_or(9),
                        est_qug: c.get("estimated_qug").and_then(|x| x.as_f64()).unwrap_or(0.0),
                    });
                }
                // Highest priority first (lower number = higher), then bigger bounty.
                v.tasks.sort_by(|a, b| a.priority.cmp(&b.priority)
                    .then(b.est_qug.partial_cmp(&a.est_qug).unwrap_or(std::cmp::Ordering::Equal)));
            }
        }
    }
    // v0.14: broadcast coordination feed (the human-readable "board" chatter).
    if let Ok(s) = std::fs::read_to_string(format!("{dir}/flux-swarm-messages.jsonl")) {
        any = true;
        for line in s.lines().rev() {
            if v.feed.len() >= 6 { break; }
            let Ok(j) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            if j.get("to").and_then(|x| x.as_str()) != Some("*") { continue; }
            let from = j.get("from").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if from.starts_with("test_") || from.is_empty() { continue; }
            // ts_ms may be a number or a stringified number; normalize to secs.
            let at = j.get("ts_ms").and_then(|x| x.as_u64())
                .or_else(|| j.get("ts_ms").and_then(|x| x.as_str()).and_then(|s| s.parse::<u64>().ok()))
                .map(|ms| ms / 1000).unwrap_or(0);
            let raw = j.get("payload").and_then(|x| x.as_str()).unwrap_or("");
            let text = raw.lines().next().unwrap_or(raw).to_string();
            v.feed.push(SwarmMsg { from, text, at });
        }
    }
    if let Ok(s) = std::fs::read_to_string(format!("{dir}/flux-swarm-files.json")) {
        if let Ok(j) = serde_json::from_str::<serde_json::Value>(&s) {
            any = true;
            if let Some(cl) = j.get("claims").and_then(|x| x.as_object()) {
                for (_p, c) in cl {
                    v.claims.push(SwarmClaim {
                        agent: c.get("agent").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                        path: c.get("path").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                        note: c.get("note").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    });
                }
            }
        }
    }
    if let Ok(s) = std::fs::read_to_string(format!("{dir}/flux-swarm-activity.jsonl")) {
        any = true;
        for line in s.lines().rev().take(60) {
            if let Ok(j) = serde_json::from_str::<serde_json::Value>(line) {
                v.activity.push(SwarmActivity {
                    agent: j.get("agent").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    kind: j.get("kind").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    detail: j.get("detail").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    at: j.get("at").and_then(|x| x.as_u64()).unwrap_or(0),
                });
            }
        }
    }
    if let Ok(s) = std::fs::read_to_string(format!("{dir}/flux-swarm-completed.jsonl")) {
        any = true;
        for line in s.lines().rev().take(80) {
            if let Ok(j) = serde_json::from_str::<serde_json::Value>(line) {
                v.results.push(SwarmResult {
                    agent: j.get("agent_id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    task_id: j.get("task_id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    qug: j.get("qug_earned").and_then(|x| x.as_f64()).unwrap_or(0.0),
                    crates: j.get("crates").and_then(|x| x.as_array())
                        .map(|a| a.iter().filter_map(|c| c.as_str()).collect::<Vec<_>>().join(","))
                        .unwrap_or_default(),
                    success: j.get("success").and_then(|x| x.as_bool()).unwrap_or(false),
                    at: j.get("completed_at").and_then(|x| x.as_u64()).unwrap_or(0),
                });
            }
        }
    }
    if !any {
        v.err = Some(format!("no swarm data under {dir} — set SIGIL_SWARM_DIR to the dev box's swarm dir"));
    }
    v
}
