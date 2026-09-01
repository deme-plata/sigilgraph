//! Detecting this process's memory ceiling (cgroup limit, else system RAM).
//! Extracted from `main.rs`.
//!
//! The result drives the RocksDB cache budgets, so a misparse here is an OOM
//! risk: reading an "unlimited" cgroup-v1 limit as its ~9 EB sentinel would
//! make the node size caches for exabytes of RAM it does not have. The parse
//! rules are pure and unit-tested; only `detect_memory_ceiling_bytes` touches
//! the filesystem.

/// cgroup v2 `memory.high`/`memory.max`: `"max"` (or 0 / garbage) → `None`;
/// a positive byte count → `Some`.
pub(crate) fn parse_cgroup_v2_limit(txt: &str) -> Option<usize> {
    let t = txt.trim();
    if t == "max" {
        return None;
    }
    match t.parse::<usize>() {
        Ok(v) if v > 0 => Some(v),
        _ => None,
    }
}

/// cgroup v1 `memory.limit_in_bytes`: a positive count *below* the near-`u64::MAX`
/// sentinel v1 reports when there is no limit → `Some`; anything else → `None`.
pub(crate) fn parse_cgroup_v1_limit(txt: &str) -> Option<usize> {
    match txt.trim().parse::<usize>() {
        Ok(v) if v > 0 && v < (1 << 62) => Some(v),
        _ => None,
    }
}

/// The `MemTotal:` line of `/proc/meminfo`, converted to bytes (the file
/// reports kB). `None` if the line is absent or malformed.
pub(crate) fn parse_meminfo_memtotal_bytes(txt: &str) -> Option<usize> {
    for line in txt.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            if let Some(kb) = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<usize>().ok())
            {
                return Some(kb.saturating_mul(1024));
            }
        }
    }
    None
}

/// Bytes of RAM this process should assume it can use: the cgroup limit if one
/// is set (v2 preferred, then v1), else total system RAM, else a conservative
/// 2 GiB rather than a generous guess.
pub(crate) fn detect_memory_ceiling_bytes() -> usize {
    // cgroup v2: the effective high/max for this process's cgroup.
    for f in ["/sys/fs/cgroup/memory.high", "/sys/fs/cgroup/memory.max"] {
        if let Ok(txt) = std::fs::read_to_string(f) {
            if let Some(v) = parse_cgroup_v2_limit(&txt) {
                return v;
            }
        }
    }
    // cgroup v1.
    if let Ok(txt) = std::fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes") {
        if let Some(v) = parse_cgroup_v1_limit(&txt) {
            return v;
        }
    }
    // No cgroup limit: fall back to total system RAM.
    if let Ok(txt) = std::fs::read_to_string("/proc/meminfo") {
        if let Some(v) = parse_meminfo_memtotal_bytes(&txt) {
            return v;
        }
    }
    // Last resort: assume a modest box rather than a generous one.
    2 * 1024 * 1024 * 1024
}

#[cfg(test)]
mod mem_ceiling_tests {
    use super::{parse_cgroup_v1_limit, parse_cgroup_v2_limit, parse_meminfo_memtotal_bytes};

    #[test]
    fn cgroup_v2_max_and_garbage_are_none() {
        assert_eq!(parse_cgroup_v2_limit("max\n"), None);
        assert_eq!(parse_cgroup_v2_limit("0"), None);
        assert_eq!(parse_cgroup_v2_limit(""), None);
        assert_eq!(parse_cgroup_v2_limit("not-a-number"), None);
        assert_eq!(parse_cgroup_v2_limit("  8589934592\n"), Some(8_589_934_592));
    }

    #[test]
    fn cgroup_v1_unlimited_sentinel_is_ignored() {
        // v1 reports a near-u64::MAX sentinel when there is no limit — it must NOT
        // be read as a real ceiling, or the node sizes caches for exabytes it lacks.
        assert_eq!(parse_cgroup_v1_limit("9223372036854771712"), None);
        assert_eq!(parse_cgroup_v1_limit("0"), None);
        assert_eq!(parse_cgroup_v1_limit("4294967296\n"), Some(4_294_967_296));
    }

    #[test]
    fn meminfo_memtotal_parses_kb_to_bytes() {
        let sample = "MemTotal:       16384000 kB\nMemFree:  100 kB\n";
        assert_eq!(parse_meminfo_memtotal_bytes(sample), Some(16_384_000 * 1024));
        // MemTotal absent, or empty, → None (fall through to the 2 GiB default).
        assert_eq!(parse_meminfo_memtotal_bytes("MemFree: 100 kB\n"), None);
        assert_eq!(parse_meminfo_memtotal_bytes(""), None);
    }
}
