//! Lockfile vs ym.json diff for `--frozen-lockfile` (ADR-016).
//!
//! Compares user-facing direct dependencies against the lockfile and emits
//! a pnpm-style diff (added/removed/version-changed). Used by ymc build to
//! fail CI runs whose lockfile is out of sync with ym.json.

use crate::config::schema::{is_maven_dep, Lockfile, YmConfig};
use crate::workspace::resolver::version_compare;
use std::collections::BTreeMap;

/// Diff between ym.json's direct deps and the on-disk lockfile.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LockfileDiff {
    /// Direct deps in ym.json that have no entry in the lock for the same `groupId:artifactId`.
    pub added: Vec<String>,
    /// Direct deps where ym.json's pinned version differs from the lock's winner version.
    /// Tuples: (`groupId:artifactId`, lock_version, ym_json_version).
    pub version_changed: Vec<(String, String, String)>,
}

impl LockfileDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.version_changed.is_empty()
    }
}

/// Compute the diff between ym.json's direct deps and the lockfile.
///
/// Only Maven coordinates with explicit versions are considered. Workspace,
/// URL, and Git deps are not represented in the lock and are skipped.
pub fn compute_diff(cfg: &YmConfig, lock: &Lockfile) -> LockfileDiff {
    let mut ym_direct: BTreeMap<String, String> = BTreeMap::new();
    if let Some(ref deps) = cfg.dependencies {
        for (coord, value) in deps {
            if !is_maven_dep(coord) {
                continue;
            }
            if value.is_workspace() || value.url().is_some() || value.git().is_some() {
                continue;
            }
            if let Some(version) = value.version() {
                // ym.json keys may be aliases like `@angus/angus-mail` while the lockfile
                // is indexed by `groupId:artifactId`. Resolve through scopeMapping /
                // global registry so the two sides compare on the same namespace —
                // otherwise every aliased direct dep falsely surfaces as "missing
                // from ym-lock.json" in --frozen-lockfile failures.
                let resolved = cfg.resolve_key(coord);
                if resolved.contains(':') {
                    ym_direct.insert(resolved, version.to_string());
                }
            }
        }
    }

    // For each direct dep in ym.json, find the highest version of the same GA in the lock.
    let mut lock_winners: BTreeMap<String, String> = BTreeMap::new();
    for gav in lock.dependencies.keys() {
        let parts: Vec<&str> = gav.splitn(3, ':').collect();
        if parts.len() != 3 {
            continue;
        }
        let ga = format!("{}:{}", parts[0], parts[1]);
        if !ym_direct.contains_key(&ga) {
            continue;
        }
        lock_winners
            .entry(ga)
            .and_modify(|existing| {
                if version_compare(parts[2], existing) > 0 {
                    *existing = parts[2].to_string();
                }
            })
            .or_insert_with(|| parts[2].to_string());
    }

    let mut diff = LockfileDiff::default();
    for (ga, ym_ver) in &ym_direct {
        match lock_winners.get(ga) {
            Some(lock_ver) if lock_ver != ym_ver => {
                diff.version_changed
                    .push((ga.clone(), lock_ver.clone(), ym_ver.clone()));
            }
            None => diff.added.push(ga.clone()),
            _ => {}
        }
    }
    diff
}

/// Format a diff as a pnpm-style error message for `--frozen-lockfile` failures.
pub fn format_diff_error(diff: &LockfileDiff) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "Lockfile is out of sync with ym.json:");
    if !diff.added.is_empty() {
        let _ = writeln!(out, "\n  Added in ym.json (missing from ym-lock.json):");
        for ga in &diff.added {
            let _ = writeln!(out, "    + {}", ga);
        }
    }
    if !diff.version_changed.is_empty() {
        let _ = writeln!(out, "\n  Version changed:");
        for (ga, from, to) in &diff.version_changed {
            let _ = writeln!(out, "    ~ {}: {} → {}", ga, from, to);
        }
    }
    let _ = writeln!(
        out,
        "\n  Run `ym install` (or just `ymc build` without --frozen-lockfile) to update ym-lock.json, then commit it."
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{DependencySpec, DependencyValue, ResolvedDependency};

    fn make_cfg(deps: &[(&str, &str)]) -> YmConfig {
        let mut cfg = YmConfig::default();
        let mut map = BTreeMap::new();
        for (k, v) in deps {
            map.insert(k.to_string(), DependencyValue::Simple(v.to_string()));
        }
        cfg.dependencies = Some(map);
        cfg
    }

    fn make_lock(deps: &[&str]) -> Lockfile {
        let mut lock = Lockfile::default();
        for gav in deps {
            lock.dependencies
                .insert(gav.to_string(), ResolvedDependency::default());
        }
        lock
    }

    #[test]
    fn test_diff_empty_when_in_sync() {
        let cfg = make_cfg(&[("com.google.guava:guava", "33.4.0")]);
        let lock = make_lock(&["com.google.guava:guava:33.4.0"]);
        let diff = compute_diff(&cfg, &lock);
        assert!(diff.is_empty());
    }

    #[test]
    fn test_diff_detects_added_direct_dep() {
        let cfg = make_cfg(&[
            ("com.google.guava:guava", "33.4.0"),
            ("org.junit.jupiter:junit-jupiter", "5.11.0"),
        ]);
        let lock = make_lock(&["com.google.guava:guava:33.4.0"]);
        let diff = compute_diff(&cfg, &lock);
        assert_eq!(diff.added, vec!["org.junit.jupiter:junit-jupiter".to_string()]);
        assert!(diff.version_changed.is_empty());
    }

    #[test]
    fn test_diff_detects_version_change() {
        let cfg = make_cfg(&[("com.google.guava:guava", "33.5.0")]);
        let lock = make_lock(&["com.google.guava:guava:33.4.0"]);
        let diff = compute_diff(&cfg, &lock);
        assert_eq!(
            diff.version_changed,
            vec![("com.google.guava:guava".to_string(), "33.4.0".to_string(), "33.5.0".to_string())]
        );
    }

    #[test]
    fn test_diff_picks_latest_lock_version_for_ga() {
        // Lock has multiple versions of same GA (transitive conflict). We compare against
        // the highest (latest-wins, ADR-016).
        let cfg = make_cfg(&[("com.google.guava:guava", "33.5.0")]);
        let mut lock = make_lock(&[
            "com.google.guava:guava:33.4.0",
            "com.google.guava:guava:33.6.0",
        ]);
        // Touch lock to ensure both keys exist (BTreeMap dedupes already)
        let _ = &mut lock;
        let diff = compute_diff(&cfg, &lock);
        // Lock's highest is 33.6.0; ym.json wants 33.5.0 → version_changed
        assert_eq!(diff.version_changed.len(), 1);
        assert_eq!(diff.version_changed[0].1, "33.6.0");
        assert_eq!(diff.version_changed[0].2, "33.5.0");
    }

    #[test]
    fn test_diff_skips_workspace_url_git() {
        let mut cfg = YmConfig::default();
        let mut map = BTreeMap::new();
        map.insert(
            "core".to_string(),
            DependencyValue::Detailed(DependencySpec {
                workspace: Some(true),
                ..Default::default()
            }),
        );
        map.insert(
            "external-jar".to_string(),
            DependencyValue::Detailed(DependencySpec {
                url: Some("https://example.com/lib.jar".to_string()),
                ..Default::default()
            }),
        );
        cfg.dependencies = Some(map);
        let lock = Lockfile::default();
        let diff = compute_diff(&cfg, &lock);
        // Workspace + URL deps are not in the lock and not subject to frozen check
        assert!(diff.is_empty());
    }

    #[test]
    fn test_diff_resolves_alias_via_scope_mapping() {
        // Regression: previously, ym.json keys like `@angus/angus-mail` were inserted
        // into ym_direct as-is and then compared against lockfile GA keys like
        // `org.eclipse.angus:angus-mail`. Every aliased direct dep was reported as
        // "missing from ym-lock.json", swamping --frozen-lockfile failures with
        // false positives. After the fix, resolve_key normalises both sides.
        let mut cfg = YmConfig::default();
        let mut mapping = BTreeMap::new();
        mapping.insert(
            "@angus/angus-mail".to_string(),
            "org.eclipse.angus:angus-mail".to_string(),
        );
        cfg.scope_mapping = Some(mapping);
        let mut deps = BTreeMap::new();
        deps.insert(
            "@angus/angus-mail".to_string(),
            DependencyValue::Simple("2.0.4".to_string()),
        );
        cfg.dependencies = Some(deps);
        let lock = make_lock(&["org.eclipse.angus:angus-mail:2.0.4"]);
        let diff = compute_diff(&cfg, &lock);
        assert!(
            diff.is_empty(),
            "aliased direct dep should resolve to GA and match lockfile, got {:?}",
            diff
        );
    }

    #[test]
    fn test_diff_skips_unresolvable_alias() {
        // An alias with no scopeMapping / global registry entry can't be compared.
        // Better to skip than to wrongly flag every alias as "added".
        let mut cfg = YmConfig::default();
        let mut deps = BTreeMap::new();
        deps.insert(
            "@nosuch/dep".to_string(),
            DependencyValue::Simple("1.0.0".to_string()),
        );
        cfg.dependencies = Some(deps);
        let lock = Lockfile::default();
        let diff = compute_diff(&cfg, &lock);
        assert!(diff.added.is_empty());
        assert!(diff.version_changed.is_empty());
    }

    #[test]
    fn test_format_diff_error_lists_changes() {
        let diff = LockfileDiff {
            added: vec!["org.junit.jupiter:junit-jupiter".to_string()],
            version_changed: vec![(
                "com.google.guava:guava".to_string(),
                "33.4.0".to_string(),
                "33.5.0".to_string(),
            )],
        };
        let msg = format_diff_error(&diff);
        assert!(msg.contains("Added in ym.json"));
        assert!(msg.contains("+ org.junit.jupiter:junit-jupiter"));
        assert!(msg.contains("Version changed"));
        assert!(msg.contains("~ com.google.guava:guava: 33.4.0 → 33.5.0"));
        assert!(msg.contains("Run `ym install`"));
    }
}
