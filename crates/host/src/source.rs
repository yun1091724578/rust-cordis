//! P0-1a：插件来源清单与加载前 artifact 校验。
//!
//! 这一层只回答“这个文件是否被允许加载”，不证明插件行为安全。调用方必须在
//! `dlopen`/`LoadLibrary` 之前调用 [`PluginSourcePolicy::verify`]。

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

/// 插件随产物一起发布的清单。文件名是 `<artifact>.manifest.json`。
///
/// `provides` / `requires` 是 **P1-1 的可选依赖声明**：前者是服务键 → 该插件提供的
/// 版本，后者是服务键 → 版本范围。旧 manifest 不带这两个字段仍然合法（缺省为空）；
/// 它们只用于装载前的依赖预检，不是运行时注入或隔离的证明。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct PluginManifest {
    pub id: String,
    pub version: String,
    pub source_revision: String,
    pub abi_version: u32,
    pub sha256: String,
    /// 服务键 → 该插件提供的 semver 版本（字符串形态，转换计划时才解析）。
    #[serde(default)]
    pub provides: BTreeMap<String, String>,
    /// 服务键 → 该插件要求的 semver range（字符串形态，转换计划时才解析）。
    #[serde(default)]
    pub requires: BTreeMap<String, String>,
}

/// 允许加载的插件身份。`sha256` 是 artifact 的完整 SHA-256。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct AllowedPlugin {
    pub id: String,
    pub version: String,
    pub source_revision: String,
    pub abi_version: u32,
    pub sha256: String,
}

#[derive(Debug, Deserialize)]
struct AllowList {
    allowed: Vec<AllowedPlugin>,
}

static ACTIVE_POLICY: OnceLock<RwLock<Option<Arc<PluginSourcePolicy>>>> = OnceLock::new();
static ALLOW_UNVERIFIED: AtomicBool = AtomicBool::new(false);

fn active_policy_cell() -> &'static RwLock<Option<Arc<PluginSourcePolicy>>> {
    ACTIVE_POLICY.get_or_init(|| RwLock::new(None))
}

/// 设置显式开发模式：没有来源策略时允许未校验插件。
///
/// 默认是 **拒绝**；只有 CLI 的 `--allow-unverified` 或嵌入方的显式调用才能关闭
/// 默认拒绝。这个开关不是安全边界，只用于本地开发/旧测试。
pub fn set_allow_unverified(allow: bool) {
    ALLOW_UNVERIFIED.store(allow, Ordering::SeqCst);
}

pub fn allow_unverified() -> bool {
    ALLOW_UNVERIFIED.load(Ordering::SeqCst)
}

/// 诊断口径：当前进程的准入模式。
pub fn admission_mode() -> &'static str {
    if active_policy().is_some() {
        "enforced"
    } else if allow_unverified() {
        "dev-unverified"
    } else {
        "denied"
    }
}

/// 当前允许的准入模式：有策略 → 强制校验；无策略但显式开发模式 → 放行；
/// 两者都没有 → 拒绝。
pub fn admission() -> Result<Option<Arc<PluginSourcePolicy>>, String> {
    if let Some(policy) = active_policy() {
        return Ok(Some(policy));
    }
    if allow_unverified() {
        return Ok(None);
    }
    Err("未配置插件来源策略；请传 --source-policy，或显式启用开发模式（set_allow_unverified/--allow-unverified）".to_string())
}

/// 设置进程级生效的来源策略。`None` = 没有策略；此时默认拒绝，除非显式
/// [`set_allow_unverified(true)`]。
///
/// # 局限
/// 这是**进程级**全局量，不是 per-App / per-[`crate::loader::LoaderConfig`] 配置：
/// 后写覆盖先写。同一进程里并行运行多个宿主（或多个改动全局状态的测试）时，
/// 调用方必须在使用完毕后显式 `set_active_policy(None)` + `set_allow_unverified(false)`
/// 复位，否则会互相污染。
pub fn set_active_policy(policy: Option<Arc<PluginSourcePolicy>>) {
    *active_policy_cell()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = policy;
}

/// 当前进程级生效的来源策略（[`set_active_policy`] 的读半边）。
pub fn active_policy() -> Option<Arc<PluginSourcePolicy>> {
    active_policy_cell()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// 通过校验的 artifact；`manifest` 是插件自述，`sha256` 是宿主实际计算值。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedPluginSource {
    pub path: PathBuf,
    pub manifest: PluginManifest,
    pub sha256: String,
}

/// 一个不可变 allow-list。空列表拒绝所有插件。
#[derive(Clone, Debug, Default)]
pub struct PluginSourcePolicy {
    allowed: Vec<AllowedPlugin>,
}

impl PluginSourcePolicy {
    pub fn new(allowed: Vec<AllowedPlugin>) -> Self {
        Self { allowed }
    }

    pub fn from_json(text: &str) -> Result<Self, String> {
        let list: AllowList =
            serde_json::from_str(text).map_err(|err| format!("解析来源 allow-list 失败: {err}"))?;
        Ok(Self::new(list.allowed))
    }

    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|err| format!("读取来源 allow-list {} 失败: {err}", path.display()))?;
        Self::from_json(&text)
    }

    /// 在加载动态库**之前**读取 artifact hash 与 sidecar manifest，并匹配 allow-list。
    pub fn verify(&self, artifact: impl AsRef<Path>) -> Result<VerifiedPluginSource, String> {
        let artifact = std::path::absolute(artifact.as_ref()).map_err(|err| {
            format!(
                "规范化插件 artifact {} 失败: {err}",
                artifact.as_ref().display()
            )
        })?;
        let actual = sha256_file(&artifact)?;
        let manifest = read_manifest(&artifact)?;
        validate_manifest(&manifest)?;
        if manifest.sha256.to_ascii_lowercase() != actual {
            return Err(format!(
                "插件 artifact SHA256 不匹配: manifest={} actual={actual}",
                manifest.sha256
            ));
        }
        let allowed = self
            .allowed
            .iter()
            .find(|item| item.sha256.eq_ignore_ascii_case(&actual))
            .ok_or_else(|| {
                format!(
                    "插件 artifact 不在来源 allow-list 中: {} ({actual})",
                    artifact.display()
                )
            })?;
        if allowed.id != manifest.id {
            return Err(format!(
                "插件 id 与 allow-list 不匹配: manifest={} allow-list={}",
                manifest.id, allowed.id
            ));
        }
        if allowed.version != manifest.version {
            return Err(format!(
                "插件 version 与 allow-list 不匹配: manifest={} allow-list={}",
                manifest.version, allowed.version
            ));
        }
        if allowed.source_revision != manifest.source_revision {
            return Err(format!(
                "插件 source_revision 与 allow-list 不匹配: manifest={} allow-list={}",
                manifest.source_revision, allowed.source_revision
            ));
        }
        if allowed.abi_version != manifest.abi_version {
            return Err(format!(
                "插件 abi_version 与 allow-list 不匹配: manifest={} allow-list={}",
                manifest.abi_version, allowed.abi_version
            ));
        }
        Ok(VerifiedPluginSource {
            path: artifact,
            manifest,
            sha256: actual,
        })
    }
}

pub fn manifest_path(artifact: impl AsRef<Path>) -> PathBuf {
    let artifact = artifact.as_ref();
    let name = artifact
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "plugin".to_string());
    artifact.with_file_name(format!("{name}.manifest.json"))
}

/// 读取 artifact 旁边的 sidecar manifest（不校验身份与 hash）。
///
/// 供审计面（`inspect` / `status` 的 provenance）使用；加载路径必须用
/// [`PluginSourcePolicy::verify`]。
pub fn read_manifest(artifact: impl AsRef<Path>) -> Result<PluginManifest, String> {
    let artifact = artifact.as_ref();
    let path = manifest_path(artifact);
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("读取插件 manifest {} 失败: {err}", path.display()))?;
    serde_json::from_str(&text)
        .map_err(|err| format!("解析插件 manifest {} 失败: {err}", path.display()))
}

pub(crate) fn validate_manifest(manifest: &PluginManifest) -> Result<(), String> {
    if manifest.id.trim().is_empty() {
        return Err("插件 manifest 缺少 id".to_string());
    }
    if manifest.version.trim().is_empty() {
        return Err("插件 manifest 缺少 version".to_string());
    }
    semver::Version::parse(&manifest.version).map_err(|err| {
        format!(
            "插件 manifest version 不是 semver（{}）：{err}",
            manifest.version
        )
    })?;
    if manifest.source_revision.trim().is_empty() {
        return Err("插件 manifest 缺少 source_revision".to_string());
    }
    if manifest.abi_version != cordis_abi::ABI_VERSION {
        return Err(format!(
            "插件 manifest 的 ABI 版本 {} 与宿主 {} 不匹配",
            manifest.abi_version,
            cordis_abi::ABI_VERSION
        ));
    }
    if manifest.sha256.len() != 64 || !manifest.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(format!(
            "插件 manifest sha256 必须是 64 位十六进制: {}",
            manifest.sha256
        ));
    }
    Ok(())
}

/// 计算一个文件的 SHA-256（审计/诊断面；加载路径用 [`PluginSourcePolicy::verify`]）。
pub fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path)
        .map_err(|err| format!("读取插件 artifact {} 失败: {err}", path.display()))?;
    Ok(sha256_hex(&bytes))
}

/// 无依赖 SHA-256，和 `xtask seal` 保持同一实现语义。
fn sha256_hex(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = bytes.to_vec();
    let bit_len = (bytes.len() as u64) * 8;
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (index, word) in w.iter_mut().take(16).enumerate() {
            let start = index * 4;
            *word = u32::from_be_bytes([
                chunk[start],
                chunk[start + 1],
                chunk[start + 2],
                chunk[start + 3],
            ]);
        }
        for index in 16..64 {
            let s0 = w[index - 15].rotate_right(7)
                ^ w[index - 15].rotate_right(18)
                ^ (w[index - 15] >> 3);
            let s1 = w[index - 2].rotate_right(17)
                ^ w[index - 2].rotate_right(19)
                ^ (w[index - 2] >> 10);
            w[index] = w[index - 16]
                .wrapping_add(s0)
                .wrapping_add(w[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[index])
                .wrapping_add(w[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    h.iter().map(|word| format!("{word:08x}")).collect()
}
