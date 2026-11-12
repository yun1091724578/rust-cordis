//! 插件依赖计划：semver range 解析与候选选择。
//!
//! 这一层只做纯数据求解，不读写动态库、不执行插件。调用方先收集
//! [`PlanEntry`]，再用 [`resolve`] 得到每个服务键的唯一提供者；失败时
//! 一次性返回所有可读问题。当前尚未接入 loader/mount。

use semver::{Version, VersionReq};
use std::collections::{BTreeMap, BTreeSet};

/// 一个待装载插件的依赖声明。
#[derive(Clone, Debug)]
pub struct PlanEntry {
    pub id: String,
    pub version: Version,
    /// 服务键 → 该插件提供的版本。
    pub provides: BTreeMap<String, Version>,
    /// 服务键 → 该插件要求的版本范围。
    pub requires: BTreeMap<String, VersionReq>,
}

/// 解析后的服务键 → 提供者。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResolvedPlan {
    pub providers: BTreeMap<String, Provider>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provider {
    pub plugin: String,
    pub version: Version,
}

/// 解析所有插件的 provides / requires。
///
/// - 同一服务键同一版本被两个插件提供 ⇒ 歧义；
/// - 没有候选满足 range ⇒ 缺失；
/// - 多个候选满足时选最高版本；同版本多提供者仍是歧义；
/// - 插件 id + version 重复 ⇒ 错误。
pub fn resolve(entries: &[PlanEntry]) -> Result<ResolvedPlan, Vec<String>> {
    let mut errors = Vec::new();
    let mut seen = BTreeSet::new();
    let mut by_key: BTreeMap<String, Vec<Provider>> = BTreeMap::new();

    for entry in entries {
        if !seen.insert((entry.id.clone(), entry.version.clone())) {
            errors.push(format!("插件 {} {} 重复声明", entry.id, entry.version));
        }
        for (key, version) in &entry.provides {
            by_key.entry(key.clone()).or_default().push(Provider {
                plugin: entry.id.clone(),
                version: version.clone(),
            });
        }
    }

    for (key, providers) in &mut by_key {
        providers.sort_by(|a, b| b.version.cmp(&a.version).then(a.plugin.cmp(&b.plugin)));
        if providers.len() > 1 && providers[0].version == providers[1].version {
            errors.push(format!(
                "服务键 {key} 的版本 {} 被多个插件提供：{} 和 {}",
                providers[0].version, providers[0].plugin, providers[1].plugin
            ));
        }
    }

    let mut resolved = ResolvedPlan::default();
    for entry in entries {
        for (key, req) in &entry.requires {
            let Some(candidates) = by_key.get(key) else {
                errors.push(format!(
                    "插件 {} {} 需要 {key} {}，但没有提供者",
                    entry.id, entry.version, req
                ));
                continue;
            };
            let Some(provider) = candidates
                .iter()
                .find(|provider| req.matches(&provider.version))
            else {
                let available = candidates
                    .iter()
                    .map(|provider| format!("{} {}", provider.plugin, provider.version))
                    .collect::<Vec<_>>()
                    .join(", ");
                errors.push(format!(
                    "插件 {} {} 需要 {key} {}，可用版本为 [{}]",
                    entry.id, entry.version, req, available
                ));
                continue;
            };
            if let Some(previous) = resolved.providers.get(key) {
                if previous.plugin != provider.plugin || previous.version != provider.version {
                    errors.push(format!(
                        "服务键 {key} 被解析到不同提供者：{} {} 与 {} {}",
                        previous.plugin, previous.version, provider.plugin, provider.version
                    ));
                    continue;
                }
            }
            resolved.providers.insert(
                key.clone(),
                Provider {
                    plugin: provider.plugin.clone(),
                    version: provider.version.clone(),
                },
            );
        }
    }

    if errors.is_empty() {
        Ok(resolved)
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        id: &str,
        version: &str,
        provides: &[(&str, &str)],
        requires: &[(&str, &str)],
    ) -> PlanEntry {
        PlanEntry {
            id: id.to_string(),
            version: Version::parse(version).unwrap(),
            provides: provides
                .iter()
                .map(|(key, version)| (key.to_string(), Version::parse(version).unwrap()))
                .collect(),
            requires: requires
                .iter()
                .map(|(key, req)| (key.to_string(), VersionReq::parse(req).unwrap()))
                .collect(),
        }
    }

    #[test]
    fn resolves_a_chain_and_picks_the_highest_version() {
        let entries = vec![
            entry("a", "1.0.0", &[("svc", "1.0.0")], &[]),
            entry("b", "1.0.0", &[("svc", "1.2.0")], &[]),
            entry("c", "2.0.0", &[], &[("svc", "^1.1")]),
        ];
        let plan = resolve(&entries).unwrap();
        assert_eq!(plan.providers["svc"].plugin, "b");
        assert_eq!(plan.providers["svc"].version, Version::new(1, 2, 0));
    }

    #[test]
    fn rejects_missing_and_unsatisfied_ranges() {
        let entries = vec![entry("c", "2.0.0", &[], &[("svc", "^2")])];
        let errors = resolve(&entries).unwrap_err();
        assert!(
            errors.iter().any(|error| error.contains("没有提供者")),
            "{errors:?}"
        );

        let entries = vec![
            entry("a", "1.0.0", &[("svc", "1.0.0")], &[]),
            entry("c", "2.0.0", &[], &[("svc", "^2")]),
        ];
        let errors = resolve(&entries).unwrap_err();
        assert!(
            errors.iter().any(|error| error.contains("可用版本")),
            "{errors:?}"
        );
    }

    #[test]
    fn rejects_duplicate_identity_and_ambiguous_same_version() {
        let entries = vec![
            entry("a", "1.0.0", &[("svc", "1.0.0")], &[]),
            entry("a", "1.0.0", &[("svc", "1.0.0")], &[]),
        ];
        let errors = resolve(&entries).unwrap_err();
        assert!(
            errors.iter().any(|error| error.contains("重复声明")),
            "{errors:?}"
        );
        assert!(
            errors.iter().any(|error| error.contains("多个插件提供")),
            "{errors:?}"
        );
    }

    #[test]
    fn requires_can_be_satisfied_by_own_provides() {
        let entries = vec![entry("a", "1.0.0", &[("svc", "1.0.0")], &[("svc", "^1")])];
        let plan = resolve(&entries).unwrap();
        assert_eq!(plan.providers["svc"].plugin, "a");
    }
}
