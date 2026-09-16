//! 免登录沙箱的虚拟登录 forge:在 CSSwitch 自有隔离 data-dir 内铸造本地虚拟 OAuth 凭证。
//!
//! 写入产物只有三件套(全部落在隔离 data-dir 内):
//! - `encryption.key` —— 4 个 key 的 `K=V` 明文(0600);
//! - `.oauth-tokens/<account_uuid>.enc` —— v2 AES-256-GCM 加密的 token blob(0600);
//! - `active-org.json` —— `{"org_uuid": "..."}` 明文(0600)。
//!
//! 字节契约(HKDF-SHA256 空 salt、info `operon:aes-256-gcm:oauth`、AAD `v2:oauth`、
//! `"v2:" + base64(IV‖密文‖tag)`、blob 字段集)逆向自 Science ≤0.1.25,经 AIUsage
//! 近期版本实测、0.1.48 动态冒烟(research/science-0148-smoke.md)与 0.1.48 静态调研
//! (research/science-0148-login-gate.md §2.1)三源确认。契约漂移时写后自校验失败,
//! 整体 fail-closed,不静默兜底。
//!
//! 护栏(写任何文件之前,优先级从高到低):
//! - 护栏 0(铁律):解析后的写入根绝不落在真实 `~/.claude-science` 之内或其本身;
//! - 护栏 1:写入根必须解析在隔离根之下(逐层 canonicalize,看穿符号链接);
//! - 护栏 2:假账号 email 必须以 `localhost.invalid` 结尾。
//!
//! 幂等三态:完整自洽 → `Reused`(凭证一字节不改);损坏 → `Repaired`(重写但沿用
//! 原 org,沙箱内旧对话不丢);首次 → `Created`。org 来源优先级固定为
//! `active-org.json` → `orgs/` 唯一目录 → 铸新;`orgs/` 下多于一个候选直接报错,
//! 绝不静默选择;也绝不从可解密的 token 借身份(旧 fork 的刻意决定,本版沿用)。
//! 旧 fork 的 marker / HistoryOrgCandidate / 历史选择层在本版刻意删除:沙箱
//! data-dir 由 CSSwitch 独占铸造,正常只会有一个 org。

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use hkdf::Hkdf;
use serde_json::json;
use sha2::Sha256;

const KEY_NAMES: [&str; 4] = [
    "ANTHROPIC_API_KEY_ENCRYPTION_KEY",
    "OAUTH_ENCRYPTION_KEY",
    "JWT_SIGNING_SECRET",
    "USER_SECRET_ENCRYPTION_KEY",
];
const HKDF_INFO: &[u8] = b"operon:aes-256-gcm:oauth";
const AAD: &[u8] = b"v2:oauth";
/// active-org.json 里的固定可见标签(0.1.48 新形态要求 org_name 字段)。
const ORG_NAME: &str = "CSSwitch Sandbox";

/// 一次铸造/修复的结果摘要;不含任何秘密材料。字段供测试与上层诊断读取,
/// 生产路径(sandbox.rs)只消费 LoginAction。
#[derive(Debug)]
#[allow(dead_code)]
pub struct ForgeResult {
    pub auth_dir: PathBuf,
    pub account_uuid: String,
    pub org_uuid: String,
    pub enc_file: PathBuf,
}

/// 本次 ensure 对沙箱虚拟登录做了什么(供控制面据实提示)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginAction {
    /// 现有登录完整自洽,凭证原样复用。
    Reused,
    /// 部分损坏,重写但沿用原 org(沙箱内旧对话不丢)。
    Repaired,
    /// 真首次,铸全新 org。
    Created,
}

impl LoginAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            LoginAction::Reused => "reused",
            LoginAction::Repaired => "repaired",
            LoginAction::Created => "created",
        }
    }
}

// ---------- 随机与编码 ----------
fn rand_bytes(n: usize) -> Result<Vec<u8>, String> {
    let mut bytes = vec![0u8; n];
    getrandom::getrandom(&mut bytes).map_err(|e| format!("获取随机数失败:{e}"))?;
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 0xf) as usize] as char);
    }
    s
}

/// n 个随机字节的 hex 串(沙箱钥匙串密码等调用方共用)。
pub(crate) fn rand_hex(n: usize) -> Result<String, String> {
    Ok(hex(&rand_bytes(n)?))
}

/// base64 编码的 32 随机字节,作为 encryption.key 里单个 key 的值。
fn b64_32() -> Result<String, String> {
    Ok(B64.encode(rand_bytes(32)?))
}

/// RFC 4122 v4 UUID(16 随机字节 + 版本/变体位)。
fn uuid_v4() -> Result<String, String> {
    let mut b = rand_bytes(16)?;
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10xx
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    ))
}

// ---------- v2 GCM(与二进制 ZtW/XtW、.mjs encryptTokenV2 一致) ----------
fn derive_key(oauth_key_b64: &str) -> Result<[u8; 32], String> {
    let ikm = B64
        .decode(oauth_key_b64.trim())
        .map_err(|e| format!("OAUTH_ENCRYPTION_KEY 非法 base64:{e}"))?;
    // salt 空(= Node hkdfSync 的 Buffer.alloc(0))。HMAC 会把空 key 补零到块长,
    // 与全零 salt 等价,故 Some(&[]) 与 None 结果相同;这里显式用 Some(&[]) 对齐 Node。
    let hk = Hkdf::<Sha256>::new(Some(&[]), &ikm);
    let mut out = [0u8; 32];
    hk.expand(HKDF_INFO, &mut out)
        .map_err(|_| "hkdf expand 失败".to_string())?;
    Ok(out)
}

/// 加密:返回 `"v2:" + base64(IV ‖ 密文 ‖ tag)`。aes-gcm 把 16 字节 tag 追加在密文末尾,
/// 故 `iv ‖ (密文‖tag)` 恰为该格式。
fn encrypt_token_v2(plaintext: &[u8], oauth_key_b64: &str) -> Result<String, String> {
    let derived = derive_key(oauth_key_b64)?;
    let iv = rand_bytes(12)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&derived));
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&iv),
            Payload {
                msg: plaintext,
                aad: AAD,
            },
        )
        .map_err(|_| "aes-gcm 加密失败".to_string())?;
    let mut framed = iv;
    framed.extend_from_slice(&ct);
    Ok(format!("v2:{}", B64.encode(&framed)))
}

/// 解密 `"v2:..."`,校验 tag;失败(含篡改/密钥不符)返回 Err。
fn decrypt_token_v2(body: &str, oauth_key_b64: &str) -> Result<Vec<u8>, String> {
    let raw = B64
        .decode(body.strip_prefix("v2:").ok_or("缺 v2: 前缀")?)
        .map_err(|e| format!("v2 体非法 base64:{e}"))?;
    if raw.len() < 12 + 16 {
        return Err("v2 密文过短".into());
    }
    let (iv, rest) = raw.split_at(12);
    let derived = derive_key(oauth_key_b64)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&derived));
    cipher
        .decrypt(
            Nonce::from_slice(iv),
            Payload {
                msg: rest,
                aad: AAD,
            },
        )
        .map_err(|_| "aes-gcm 解密/验签失败".to_string())
}

// ---------- 路径护栏与安全写 ----------
/// 逐层向上找到最近的已存在祖先并 canonicalize(看穿符号链接),再把不存在的尾巴拼回。
fn real_ancestor(p: &Path) -> PathBuf {
    let mut cur = p.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !cur.exists() {
        if let Some(name) = cur.file_name() {
            tail.push(name.to_os_string());
        }
        match cur.parent() {
            Some(par) if par != cur => cur = par.to_path_buf(),
            _ => break,
        }
    }
    let mut base = std::fs::canonicalize(&cur).unwrap_or(cur);
    for name in tail.iter().rev() {
        base.push(name);
    }
    base
}

fn is_symlink(p: &Path) -> bool {
    std::fs::symlink_metadata(p)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn assert_not_symlink(p: &Path) -> Result<(), String> {
    if is_symlink(p) {
        return Err(format!("拒绝:{} 是符号链接,绝不跟随写入。", p.display()));
    }
    Ok(())
}

/// 安全写:拒符号链接 + O_EXCL 临时文件 + rename + chmod,避免跟随/竞态写到非预期目标。
/// 权限位在创建临时文件时即生效:秘密文件不以 umask 默认权限存在过任何瞬间。
/// 沙箱编排层(sandbox.rs)写钥匙串密码文件复用同一实现。
pub(crate) fn safe_write(path: &Path, data: &[u8], mode: u32) -> Result<(), String> {
    assert_not_symlink(path)?;
    let parent = path.parent().ok_or("目标无父目录")?;
    let suffix = rand_hex(6)?;
    let tmp = parent.join(format!(".tmp-{suffix}"));
    let result = (|| -> Result<(), String> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true) // O_CREAT|O_EXCL
            .mode(mode)
            .open(&tmp)
            .map_err(|e| format!("建临时文件失败:{e}"))?;
        f.write_all(data)
            .map_err(|e| format!("写临时文件失败:{e}"))?;
        drop(f);
        std::fs::rename(&tmp, path).map_err(|e| format!("rename 失败:{e}"))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("chmod 失败:{e}"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn chmod_best_effort(p: &Path, mode: u32) {
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode));
}

fn current_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and does not dereference pointers.
    unsafe { libc::geteuid() }
}

// ---------- 主流程 ----------
/// 护栏(写任何东西之前;`real_ancestor` 已看穿符号链接):真实目录保护(护栏 0,铁律最高
/// 优先)→ 隔离根内(护栏 1)→ 假账号 email(护栏 2)。全过则返回解析后的写入根 `resolved`。
fn resolve_guarded(
    auth_dir: &Path,
    email: &str,
    sandbox_root: &Path,
    real_cred_dir: &Path,
) -> Result<PathBuf, String> {
    let resolved = real_ancestor(auth_dir);
    // 载重护栏 0(铁律,最高优先,先于隔离根检查):解析后的写入根绝不落在真实
    // ~/.claude-science 之内或其本身。防「把 ~/.csswitch/science-sandbox(或其祖先)预置成
    // 指向真实目录的符号链接」——此时 sandbox_root 也会解析进真实树,令下方「隔离根内」
    // 检查失效(resolved 与 root 同处真实树内而放行)。这条不依赖隔离根,是对真实目录的
    // 绝对保护:任何异常布局都绝不触碰真实目录。
    let real_root = real_ancestor(real_cred_dir);
    if resolved.starts_with(&real_root) {
        return Err(format!(
            "拒绝:auth_dir 解析到真实 Science 目录({})之内或本身,铁律禁止触碰。",
            real_root.display()
        ));
    }
    // 护栏 1:resolved 必须落在隔离根之下。预置符号链接把 auth_dir 或其祖先链到隔离根外
    // 任意目录会让 canonicalize 解引用到别处;把写重定向挡在写任何文件之前。
    let root = real_ancestor(sandbox_root);
    if !resolved.starts_with(&root) {
        return Err(format!(
            "拒绝:auth_dir 解析到隔离根之外({} 不在 {} 下),疑似符号链接重定向。",
            resolved.display(),
            root.display()
        ));
    }
    if !email.ends_with("localhost.invalid") {
        return Err(format!(
            "拒绝:email 必须以 localhost.invalid 结尾(当前 {email}),确保是假账号。"
        ));
    }
    Ok(resolved)
}

/// 在已通过护栏的 `resolved` 写一套虚拟登录。`prefer_org` 为 Some 则复用(修复时保住 org,
/// 令旧对话 DB 仍挂得上),为 None 则新铸;account_uuid 每次写入都重新生成。
fn write_login(resolved: &Path, email: &str, prefer_org: Option<String>) -> Result<ForgeResult, String> {
    std::fs::create_dir_all(resolved).map_err(|e| format!("建 auth_dir 失败:{e}"))?;
    chmod_best_effort(resolved, 0o700);

    // —— encryption.key:复用已存在的(保持旧 .enc 可解),否则新造 ——
    let key_file = resolved.join("encryption.key");
    assert_not_symlink(&key_file)?;
    let mut keys: BTreeMap<String, String> = BTreeMap::new();
    if key_file.exists() {
        let txt = std::fs::read_to_string(&key_file)
            .map_err(|e| format!("读 encryption.key 失败:{e}"))?;
        for line in txt.lines() {
            if let Some(eq) = line.find('=') {
                if eq > 0 {
                    let v = line[eq + 1..].trim();
                    if !v.is_empty() {
                        keys.insert(line[..eq].trim().to_string(), v.to_string());
                    }
                }
            }
        }
    }
    // 复用的 OAUTH_ENCRYPTION_KEY 必须能 base64 解出 ≥16 字节,否则丢弃 → 下面 fill 循环
    // 重造。免得「present 但非法 base64」的 key 被留用 → 后续 encrypt_token_v2 里 derive_key
    // 直接报错而非自愈。只校验这一把(我们加密 .enc 用它);另三把 Science 内部用,present 则留。
    let oauth_usable = keys
        .get("OAUTH_ENCRYPTION_KEY")
        .map(|v| B64.decode(v.trim()).map(|b| b.len() >= 16).unwrap_or(false))
        .unwrap_or(false);
    if !oauth_usable {
        keys.remove("OAUTH_ENCRYPTION_KEY");
    }
    for k in KEY_NAMES {
        if !keys.contains_key(k) {
            keys.insert(k.to_string(), b64_32()?);
        }
    }
    let key_blob = KEY_NAMES
        .iter()
        .map(|k| format!("{k}={}", keys[*k]))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    safe_write(&key_file, key_blob.as_bytes(), 0o600)?;

    // —— 令牌 blob(字段对齐 _adapt / _tryOauthToken):org 有偏好则复用,account 恒新铸 ——
    let account_uuid = uuid_v4()?;
    let org_uuid = match prefer_org {
        Some(o) => o,
        None => uuid_v4()?,
    };
    let access = format!("sk-ant-virtual-{}", rand_hex(24)?);
    let blob = json!({
        "access_token": access,          // 网关会剥离入站 Bearer,值任意
        "refresh_token": "",
        "api_key": null,
        "token_expires_at": "2099-01-01T00:00:00.000Z", // 远期 → 绝不联网刷新
        "provider": "claude_ai",
        "scopes": "user:inference user:file_upload user:profile user:mcp_servers user:plugins",
        "email": email,
        "account_uuid": account_uuid.clone(),
        "subscription_type": "max",
        "rate_limit_tier": null,
        "seat_tier": null,
        "org_uuid": org_uuid.clone(),
        "billing_type": null,
        "has_extra_usage_enabled": false,
        // 0.1.48 新增的 5 个字段(research/science-0148-login-gate.md §2.1),
        // 均可 null;写全是对版本保真,取 null/false 安全默认值。
        "refresh_token_expires_at": null,
        "org_name": null,
        "allow_safety_feedback": false,
        "billing_resolved": false,
        "tier_unmappable": false
    });
    let plaintext = serde_json::to_vec(&blob).map_err(|e| format!("序列化 blob 失败:{e}"))?;
    let oauth_key = keys.get("OAUTH_ENCRYPTION_KEY").ok_or("缺 OAUTH_ENCRYPTION_KEY")?;
    let enc_body = encrypt_token_v2(&plaintext, oauth_key)?;

    // —— 写 .oauth-tokens/<sanitized>.enc;先清其它 .enc 保证唯一 ——
    let tok_dir = resolved.join(".oauth-tokens");
    assert_not_symlink(&tok_dir)?;
    std::fs::create_dir_all(&tok_dir).map_err(|e| format!("建 .oauth-tokens 失败:{e}"))?;
    chmod_best_effort(&tok_dir, 0o700);
    // 列目录失败必须显式失败(与删除失败同理):漏列 = 残留旧 .enc + 新 .enc = 多个,
    // 而 Science 预期目录内恰好一个 → 会「显示启动成功却仍登录不上」。
    let rd = std::fs::read_dir(&tok_dir).map_err(|e| format!("列 .oauth-tokens 失败:{e}"))?;
    for entry in rd {
        let e = entry.map_err(|err| format!("读 .oauth-tokens 条目失败:{err}"))?;
        let p = e.path();
        if p.extension().map(|x| x == "enc").unwrap_or(false) {
            assert_not_symlink(&p)?;
            // 删除失败必须显式失败:否则残留旧 .enc + 新 .enc = 多个,
            // 而 Science 预期目录内恰好一个 → 会「显示启动成功却仍登录不上」。
            std::fs::remove_file(&p).map_err(|err| {
                format!(
                    "删除旧令牌 {} 失败:{err}(需目录内恰好一个 .enc)",
                    p.display()
                )
            })?;
        }
    }
    let user_id: String = account_uuid
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    let enc_file = tok_dir.join(format!("{user_id}.enc"));
    safe_write(&enc_file, enc_body.as_bytes(), 0o600)?;

    // —— 自校验:用同样逻辑解密回读,确保 Science 能解开 ——
    let roundtrip = decrypt_token_v2(&enc_body, oauth_key)?;
    let rt: serde_json::Value =
        serde_json::from_slice(&roundtrip).map_err(|e| format!("自校验解析失败:{e}"))?;
    if rt.get("email").and_then(|v| v.as_str()) != Some(email) {
        return Err("自校验失败:解密回读的 email 不符".into());
    }

    // 持久化沙箱组织标识。0.1.48 的 active-org.json 新形态带 `org_name` /
    // `account_uuid` / `login_owner_data_dir`(research §2.3):印章只拦 logout 不拦使用;
    // 我们是沙箱登录的唯一来源,印章写沙箱 data-dir 的解析路径,沙箱内 logout 正常可用。
    let org_json = serde_json::to_string_pretty(&json!({
        "org_uuid": org_uuid,
        "org_name": ORG_NAME,
        "account_uuid": account_uuid,
        "login_owner_data_dir": resolved.display().to_string(),
    }))
    .map_err(|e| format!("序列化 active-org 失败:{e}"))?
        + "\n";
    safe_write(
        &resolved.join("active-org.json"),
        org_json.as_bytes(),
        0o600,
    )?;

    Ok(ForgeResult {
        auth_dir: resolved.to_path_buf(),
        account_uuid,
        org_uuid,
        enc_file,
    })
}

// ---------- 幂等:现有登录读取/校验 ----------
/// 严格校验:s 形如 8-4-4-4-12 的十六进制 UUID。
fn looks_like_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// 解析 encryption.key 拿 OAUTH_ENCRYPTION_KEY(非空才算)。
fn parse_oauth_key(resolved: &Path) -> Option<String> {
    let txt = std::fs::read_to_string(resolved.join("encryption.key")).ok()?;
    for line in txt.lines() {
        if let Some(v) = line.strip_prefix("OAUTH_ENCRYPTION_KEY=") {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// `.oauth-tokens/` 下恰好一个 `.enc` 才返回其路径;零个或多于一个都返回 None。
fn single_enc(resolved: &Path) -> Option<PathBuf> {
    let mut found: Option<PathBuf> = None;
    for e in std::fs::read_dir(resolved.join(".oauth-tokens"))
        .ok()?
        .flatten()
    {
        let p = e.path();
        if p.extension().map(|x| x == "enc").unwrap_or(false) {
            if found.is_some() {
                return None;
            }
            found = Some(p);
        }
    }
    found
}

/// active-org.json 里合法 UUID 的 org_uuid。
fn read_active_org(resolved: &Path) -> Option<String> {
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(resolved.join("active-org.json")).ok()?)
            .ok()?;
    let o = v.get("org_uuid")?.as_str()?;
    if looks_like_uuid(o) {
        Some(o.to_string())
    } else {
        None
    }
}

/// 扫描 `orgs/` 下当前用户所有的真实目录(符号链接即使指向目录也排除),返回排序后的
/// UUID 目录名。候选必须同时满足:非符号链接的真实目录、uid 匹配、严格 UUID 格式。
fn scan_org_dirs(resolved: &Path) -> Vec<String> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(resolved.join("orgs")) {
        for e in rd.flatten() {
            let Ok(metadata) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            if !metadata.file_type().is_dir() || metadata.uid() != current_uid() {
                continue;
            }
            if let Some(name) = e.file_name().to_str() {
                if looks_like_uuid(name) {
                    v.push(name.to_string());
                }
            }
        }
    }
    v.sort();
    v
}

/// 今天的 UTC 日期 `YYYY-MM-DD`(复用 crate 的时间戳格式化,前 10 字符即日期)。
fn today_utc_ymd() -> String {
    let epoch_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    crate::format_utc_timestamp_ms(epoch_ms)[..10].to_string()
}

/// `token_expires_at`(ISO8601)的日期部分是否 ≥ 今天(UTC),即尚未过期。
/// 取前 10 字符 `YYYY-MM-DD` 与今天按字典序比较(ISO8601 日期字典序即时间序);
/// 格式不对(长度不足 / 非 `dddd-dd-dd`)视为过期。粒度到「天」足够区分远期 vs 过去。
fn token_not_expired(expires_at: &str) -> bool {
    if expires_at.len() < 10 {
        return false;
    }
    let date = &expires_at[..10];
    let b = date.as_bytes();
    let shaped = b.iter().enumerate().all(|(i, &c)| match i {
        4 | 7 => c == b'-',
        _ => c.is_ascii_digit(),
    });
    shaped && date >= today_utc_ymd().as_str()
}

/// 现有登录是否「完整且自洽」;是则返回其身份(用于原样复用)。任何一步不满足返回 None
/// (→ 降级到修复,安全方向)。校验从严:读路径不得是符号链接;解密后 org 一致、email 是
/// 假账号、`account_uuid` 是合法 UUID、`provider`=claude_ai、`access_token` 非空、
/// `token_expires_at` 未过期、`subscription_type` 必须是 max(沙箱凭证必须呈现 max;
/// free/降级 blob 判不自洽 → 修复重写回 max)。
fn read_intact_login(resolved: &Path, email: &str) -> Option<ForgeResult> {
    // 读路径不得是符号链接(不跟随;可疑布局 → 视作不自洽走修复,修复端有 assert_not_symlink)。
    if is_symlink(&resolved.join("encryption.key"))
        || is_symlink(&resolved.join(".oauth-tokens"))
        || is_symlink(&resolved.join("active-org.json"))
    {
        return None;
    }
    let key = parse_oauth_key(resolved)?;
    let enc = single_enc(resolved)?;
    if is_symlink(&enc) {
        return None;
    }
    let active_org = read_active_org(resolved)?;
    let body = std::fs::read_to_string(&enc).ok()?;
    let blob: serde_json::Value =
        serde_json::from_slice(&decrypt_token_v2(&body, &key).ok()?).ok()?;
    let blob_org = blob.get("org_uuid")?.as_str()?;
    let blob_email = blob.get("email")?.as_str()?;
    let account = blob.get("account_uuid")?.as_str()?;
    let provider_ok = blob.get("provider").and_then(|v| v.as_str()) == Some("claude_ai");
    let access_ok = blob
        .get("access_token")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let expiry_ok = blob
        .get("token_expires_at")
        .and_then(|v| v.as_str())
        .map(token_not_expired)
        .unwrap_or(false);
    let subscription_ok =
        blob.get("subscription_type").and_then(|v| v.as_str()) == Some("max");
    if blob_org != active_org
        || blob_email != email
        || !blob_email.ends_with("localhost.invalid")
        || !looks_like_uuid(account)
        || !provider_ok
        || !access_ok
        || !expiry_ok
        || !subscription_ok
    {
        return None;
    }
    Some(ForgeResult {
        auth_dir: resolved.to_path_buf(),
        account_uuid: account.to_string(),
        org_uuid: active_org,
        enc_file: enc,
    })
}

/// 幂等虚拟登录:完整自洽 → 复用;部分损坏 → 修复但保 org;真首次 → 铸新。
pub fn ensure_virtual_login(
    auth_dir: &Path,
    email: &str,
    sandbox_root: &Path,
) -> Result<(ForgeResult, LoginAction), String> {
    let home = crate::profile::home();
    ensure_virtual_login_guarded(auth_dir, email, sandbox_root, &home.join(".claude-science"))
}

/// 可注入「真实凭证目录」的内层(测试用,绝不碰真实 `~/.claude-science`)。
fn ensure_virtual_login_guarded(
    auth_dir: &Path,
    email: &str,
    sandbox_root: &Path,
    real_cred_dir: &Path,
) -> Result<(ForgeResult, LoginAction), String> {
    let resolved = resolve_guarded(auth_dir, email, sandbox_root, real_cred_dir)?;
    // 完整自洽 → 凭证原样复用,一个字节都不改。
    if let Some(fr) = read_intact_login(&resolved, email) {
        return Ok((fr, LoginAction::Reused));
    }
    // org 来源优先级:active-org.json → orgs/ 唯一目录 → 铸新。
    // Never borrow account/org identity from a decryptable token:隔离 data-dir 里也可能
    // 躺着一份此前的官方登录;只有 active-org.json 或经验证的唯一历史目录可以选择组织。
    let (prior_org, action) = match read_active_org(&resolved) {
        Some(org) => (Some(org), LoginAction::Repaired),
        None => {
            let candidates = scan_org_dirs(&resolved);
            match candidates.len() {
                0 => (None, LoginAction::Created),
                1 => (Some(candidates[0].clone()), LoginAction::Repaired),
                n => {
                    return Err(format!(
                        "沙箱 {} 下存在 {n} 个历史组织目录,无法确定活动组织,已拒绝静默选择;\
                         请停止沙箱后清理隔离目录再重试。",
                        resolved.join("orgs").display()
                    ))
                }
            }
        }
    };
    let fr = write_login(&resolved, email, prior_org)?;
    Ok((fr, action))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static CTR: AtomicU32 = AtomicU32::new(0);

    fn tmpdir(tag: &str) -> PathBuf {
        let n = CTR.fetch_add(1, Ordering::SeqCst);
        let d = std::env::temp_dir().join(format!(
            "csswitch-forge-{}-{}-{}",
            std::process::id(),
            tag,
            n
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    const EMAIL: &str = "virtual@localhost.invalid";

    fn read_oauth_key(auth_dir: &Path) -> String {
        let txt = std::fs::read_to_string(auth_dir.join("encryption.key")).unwrap();
        for line in txt.lines() {
            if let Some(v) = line.strip_prefix("OAUTH_ENCRYPTION_KEY=") {
                return v.trim().to_string();
            }
        }
        panic!("no OAUTH_ENCRYPTION_KEY");
    }

    fn the_enc_file(auth_dir: &Path) -> PathBuf {
        let tok = auth_dir.join(".oauth-tokens");
        let mut encs: Vec<PathBuf> = std::fs::read_dir(&tok)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "enc").unwrap_or(false))
            .collect();
        assert_eq!(encs.len(), 1, "应恰好一个 .enc");
        encs.pop().unwrap()
    }

    fn read_active_org_uuid(auth_dir: &Path) -> String {
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(auth_dir.join("active-org.json")).unwrap(),
        )
        .unwrap();
        v["org_uuid"].as_str().unwrap().to_string()
    }

    /// 用当前 key 重写一个「字段可定制」的 .enc,模拟 Science 或外部写入的异态凭证。
    fn rewrite_enc(auth_dir: &Path, patch: serde_json::Value) {
        let key = read_oauth_key(auth_dir);
        let body = std::fs::read_to_string(the_enc_file(auth_dir)).unwrap();
        let mut blob: serde_json::Value =
            serde_json::from_slice(&decrypt_token_v2(&body, &key).unwrap()).unwrap();
        for (k, v) in patch.as_object().unwrap() {
            blob[k] = v.clone();
        }
        let enc = encrypt_token_v2(&serde_json::to_vec(&blob).unwrap(), &key).unwrap();
        std::fs::write(the_enc_file(auth_dir), enc).unwrap();
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = b64_32().unwrap();
        let pt = br#"{"email":"virtual@localhost.invalid","x":1}"#;
        let body = encrypt_token_v2(pt, &key).unwrap();
        assert!(body.starts_with("v2:"));
        let back = decrypt_token_v2(&body, &key).unwrap();
        assert_eq!(back, pt);
    }

    #[test]
    fn decrypt_fails_on_wrong_key() {
        let k1 = b64_32().unwrap();
        let k2 = b64_32().unwrap();
        let body = encrypt_token_v2(b"hello", &k1).unwrap();
        assert!(decrypt_token_v2(&body, &k2).is_err(), "错 key 应验签失败");
    }

    #[test]
    fn created_login_writes_files_and_selfchecks() {
        let dir = tmpdir("ok");
        let fake_real = tmpdir("realcred"); // 与 auth_dir 不同,护栏放行
        let (r, action) = ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        assert_eq!(action, LoginAction::Created);
        assert!(r.enc_file.is_file());
        assert!(dir.join("encryption.key").is_file());
        assert!(dir.join("active-org.json").is_file());
        // 解密回读一致
        let key = read_oauth_key(&dir);
        let body = std::fs::read_to_string(the_enc_file(&dir)).unwrap();
        let blob: serde_json::Value =
            serde_json::from_slice(&decrypt_token_v2(&body, &key).unwrap()).unwrap();
        assert_eq!(blob["email"], EMAIL);
        assert_eq!(blob["provider"], "claude_ai");
        assert_eq!(blob["subscription_type"], "max");
        // 0.1.48 新增的 5 个字段全部以安全默认值写齐(版本保真)。
        assert_eq!(blob["refresh_token_expires_at"], serde_json::Value::Null);
        assert_eq!(blob["org_name"], serde_json::Value::Null);
        assert_eq!(blob["allow_safety_feedback"], false);
        assert_eq!(blob["billing_resolved"], false);
        assert_eq!(blob["tier_unmappable"], false);
        // active-org.json 的 0.1.48 形态:org_uuid 与摘要一致,印章指向沙箱 data-dir 自身。
        let org: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("active-org.json")).unwrap())
                .unwrap();
        assert_eq!(org["org_uuid"], r.org_uuid);
        assert_eq!(org["org_name"], ORG_NAME);
        assert_eq!(org["account_uuid"], r.account_uuid);
        assert_eq!(
            org["login_owner_data_dir"].as_str().unwrap(),
            std::fs::canonicalize(&dir).unwrap().display().to_string(),
            "登录来源印章 = 沙箱 data-dir 的解析路径"
        );
        let blocked_target = dir.join("blocked-target");
        std::fs::create_dir(&blocked_target).unwrap();
        assert!(safe_write(&blocked_target, b"must-fail", 0o600).is_err());
        assert!(
            std::fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .all(|entry| !entry.file_name().to_string_lossy().starts_with(".tmp-")),
            "失败的写入必须清理自己携带凭证的临时文件"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&fake_real);
    }

    #[test]
    fn forge_rejects_real_cred_dir() {
        // auth_dir 指向「真实凭证目录」(这里用临时目录扮演)→ 必须拒、且不写。
        let real = tmpdir("real2");
        let r = ensure_virtual_login_guarded(&real, EMAIL, &real, &real);
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("真实 Science 目录"));
        assert!(
            !real.join("encryption.key").exists(),
            "拒绝路径不应写任何文件"
        );
        let _ = std::fs::remove_dir_all(&real);
    }

    #[test]
    fn forge_rejects_symlink_into_real_science_tree() {
        // 铁律回归:把隔离根的祖先预置成指向【真实 Science 目录】的符号链接——此时隔离根
        // 自身也解析进真实树,仅靠护栏 1「隔离根内」会放行(resolved 与 root 同在真实树内)。
        // 护栏 0 必须在写任何文件之前拒绝,且真实目录零改动。
        let real = tmpdir("real-science");
        std::fs::create_dir_all(real.join(".oauth-tokens")).unwrap();
        std::fs::write(real.join(".oauth-tokens/victim.enc"), b"keep-me").unwrap();
        std::fs::write(real.join("encryption.key"), b"KEEP=me\n").unwrap();

        let csw = tmpdir("csw");
        std::fs::create_dir_all(&csw).unwrap();
        // ~/.csswitch/science-sandbox -> 真实 Science 目录(预置的恶意/异常软链接)
        let sandbox_link = csw.join("science-sandbox");
        std::os::unix::fs::symlink(&real, &sandbox_link).unwrap();

        let sandbox_root = sandbox_link.join("home");
        let auth_dir = sandbox_root.join(".claude-science");
        let r = ensure_virtual_login_guarded(&auth_dir, EMAIL, &sandbox_root, &real);
        assert!(r.is_err(), "隔离根经符号链接落入真实树必须被拒");
        assert!(r.unwrap_err().contains("真实 Science 目录"));
        // 真实目录零改动。
        assert_eq!(
            std::fs::read(real.join("encryption.key")).unwrap(),
            b"KEEP=me\n"
        );
        assert!(
            real.join(".oauth-tokens/victim.enc").exists(),
            "真实 .enc 不该被碰"
        );
        assert!(!real.join("home").exists(), "不该在真实树里建任何目录");
        for d in [real, csw] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn forge_rejects_symlink_escaping_sandbox_root() {
        // 把隔离根内的 auth_dir 预置成指向隔离根外目录的符号链接,forge 必须在写任何
        // 文件之前拒绝,且绝不碰链接目标。
        let root = tmpdir("sbroot");
        std::fs::create_dir_all(&root).unwrap();
        let outside = tmpdir("outside");
        std::fs::create_dir_all(&outside).unwrap();
        // 预置目标里一个「不该被删」的旧 .enc 与一个「不该被覆盖」的 key 文件。
        std::fs::create_dir_all(outside.join(".oauth-tokens")).unwrap();
        std::fs::write(outside.join(".oauth-tokens/victim.enc"), b"keep-me").unwrap();
        std::fs::write(outside.join("encryption.key"), b"KEEP=me\n").unwrap();

        let auth_dir = root.join(".claude-science");
        std::os::unix::fs::symlink(&outside, &auth_dir).unwrap(); // auth_dir -> outside

        let fake_real = tmpdir("realcred5");
        let r = ensure_virtual_login_guarded(&auth_dir, EMAIL, &root, &fake_real);
        assert!(r.is_err(), "符号链接逃出隔离根应被拒");
        assert!(r.unwrap_err().contains("隔离根之外"));
        // 目标目录零改动。
        assert_eq!(
            std::fs::read(outside.join("encryption.key")).unwrap(),
            b"KEEP=me\n"
        );
        assert!(
            outside.join(".oauth-tokens/victim.enc").exists(),
            "旧 .enc 不该被删"
        );
        assert!(!outside.join("active-org.json").exists(), "不该写入目标");
        for d in [root, outside, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn forge_rejects_non_localhost_email() {
        let dir = tmpdir("email");
        let fake_real = tmpdir("realcred3");
        let r = ensure_virtual_login_guarded(&dir, "attacker@example.com", &dir, &fake_real);
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("localhost.invalid"));
    }

    // ---------- 幂等 ensure_virtual_login ----------
    #[test]
    fn ensure_reuses_intact_login() {
        let dir = tmpdir("reuse");
        let fake_real = tmpdir("realcred-reuse");
        let (first, _) = ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        let org0 = read_active_org_uuid(&dir);
        let enc0 = std::fs::read(the_enc_file(&dir)).unwrap();
        let key0 = std::fs::read(dir.join("encryption.key")).unwrap();
        let (r, action) = ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        assert_eq!(action, LoginAction::Reused);
        assert_eq!(r.org_uuid, first.org_uuid, "org 不变");
        assert_eq!(r.org_uuid, org0);
        assert_eq!(
            std::fs::read(the_enc_file(&dir)).unwrap(),
            enc0,
            ".enc 字节不变"
        );
        assert_eq!(
            std::fs::read(dir.join("encryption.key")).unwrap(),
            key0,
            "key 字节不变"
        );
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn ensure_repairs_missing_enc_keeps_org() {
        let dir = tmpdir("rep-missing");
        let fake_real = tmpdir("realcred-rm");
        ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        let org0 = read_active_org_uuid(&dir);
        std::fs::remove_file(the_enc_file(&dir)).unwrap();
        let (r, action) = ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        assert_eq!(action, LoginAction::Repaired);
        assert_eq!(r.org_uuid, org0, "修复必须沿用原 org");
        assert_eq!(read_active_org_uuid(&dir), org0);
        let key = read_oauth_key(&dir);
        let body = std::fs::read_to_string(the_enc_file(&dir)).unwrap();
        assert!(decrypt_token_v2(&body, &key).is_ok());
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn ensure_repairs_extra_enc_keeps_org() {
        let dir = tmpdir("rep-extra");
        let fake_real = tmpdir("realcred-re");
        ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        let org0 = read_active_org_uuid(&dir);
        std::fs::write(dir.join(".oauth-tokens/stale.enc"), b"v2:garbage").unwrap();
        let (r, action) = ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        assert_eq!(action, LoginAction::Repaired);
        assert_eq!(r.org_uuid, org0);
        let _ = the_enc_file(&dir); // 内部断言恰好一个 .enc
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn ensure_repairs_replaced_key_keeps_org() {
        // E2E-5 的单测模拟:encryption.key 被换掉 → 旧 .enc 不可解 → 修复保 org。
        let dir = tmpdir("rep-key");
        let fake_real = tmpdir("realcred-rk");
        ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        let org0 = read_active_org_uuid(&dir);
        let mut blob = String::new();
        for k in KEY_NAMES {
            blob.push_str(&format!("{k}={}\n", b64_32().unwrap()));
        }
        std::fs::write(dir.join("encryption.key"), blob).unwrap();
        let (r, action) = ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        assert_eq!(
            action,
            LoginAction::Repaired,
            "active-org.json 仍在 → 修复而非铸新"
        );
        assert_eq!(r.org_uuid, org0, "换 key 也不换 org");
        let key = read_oauth_key(&dir);
        let body = std::fs::read_to_string(the_enc_file(&dir)).unwrap();
        assert!(decrypt_token_v2(&body, &key).is_ok(), "重铸令牌应可解");
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn ensure_creates_on_first_run() {
        let dir = tmpdir("create");
        let fake_real = tmpdir("realcred-cr");
        let (r, action) = ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        assert_eq!(action, LoginAction::Created);
        assert!(r.enc_file.is_file());
        assert!(looks_like_uuid(&r.org_uuid));
        assert_eq!(read_active_org_uuid(&dir), r.org_uuid);
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn ensure_adopts_single_org_dir_when_active_and_token_gone() {
        // active-org.json 和 .enc 都没了,但 orgs/ 下恰好一个历史 org → 采用它。
        let dir = tmpdir("one-orgdir");
        let fake_real = tmpdir("realcred-1o");
        ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        let org0 = read_active_org_uuid(&dir);
        std::fs::create_dir_all(dir.join("orgs").join(&org0)).unwrap();
        std::fs::remove_file(the_enc_file(&dir)).unwrap();
        std::fs::remove_file(dir.join("active-org.json")).unwrap();
        let (r, action) = ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        assert_eq!(action, LoginAction::Repaired);
        assert_eq!(r.org_uuid, org0, "应采用唯一历史 org 目录");
        assert_eq!(read_active_org_uuid(&dir), org0);
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn ensure_errors_on_ambiguous_multi_org() {
        // 无 active-org、orgs/ 下多个历史组织 → 报错中止,绝不静默选择也不静默新铸。
        let dir = tmpdir("multi-org");
        let fake_real = tmpdir("realcred-mo");
        ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        let a = uuid_v4().unwrap();
        let b = uuid_v4().unwrap();
        std::fs::create_dir_all(dir.join("orgs").join(&a)).unwrap();
        std::fs::create_dir_all(dir.join("orgs").join(&b)).unwrap();
        std::fs::remove_file(the_enc_file(&dir)).unwrap();
        std::fs::remove_file(dir.join("active-org.json")).unwrap();
        let r = ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real);
        assert!(r.is_err(), "多历史组织无法定位活动者应报错");
        assert!(r.unwrap_err().contains("历史组织"));
        assert!(
            !dir.join("active-org.json").exists(),
            "报错不应写 active-org.json"
        );
        assert_eq!(scan_org_dirs(&dir).len(), 2, "不应静默新铸 org");
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn org_directory_scan_ignores_symlinks() {
        let dir = tmpdir("org-symlink");
        let outside = tmpdir("org-symlink-outside");
        std::fs::create_dir_all(dir.join("orgs")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let real_org = uuid_v4().unwrap();
        let linked_org = uuid_v4().unwrap();
        std::fs::create_dir(dir.join("orgs").join(&real_org)).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("orgs").join(&linked_org)).unwrap();
        let candidates = scan_org_dirs(&dir);
        assert_eq!(candidates, vec![real_org]);
        for d in [dir, outside] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn decryptable_token_is_not_used_as_org_identity() {
        // 删掉 active-org.json 后,即使 .enc 仍可解密且内含 org_uuid,也必须铸新 org。
        let dir = tmpdir("token-not-identity");
        let fake_real = tmpdir("realcred-token-not-identity");
        ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        let token_org = read_active_org_uuid(&dir);
        std::fs::remove_file(dir.join("active-org.json")).unwrap();

        let (created, action) = ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        assert_eq!(action, LoginAction::Created);
        assert_ne!(created.org_uuid, token_org, "不得从可解 token 借用组织身份");
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn ensure_recreates_key_on_invalid_base64() {
        // OAUTH_ENCRYPTION_KEY 是非法 base64 → 不报错,重造合法 key,org 复用,新 .enc 可解。
        let dir = tmpdir("badkey");
        let fake_real = tmpdir("realcred-bk");
        ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        let org0 = read_active_org_uuid(&dir);
        let mut blob = String::new();
        for k in KEY_NAMES {
            if k == "OAUTH_ENCRYPTION_KEY" {
                blob.push_str(&format!("{k}=!!!!not-base64!!!!\n"));
            } else {
                blob.push_str(&format!("{k}={}\n", b64_32().unwrap()));
            }
        }
        std::fs::write(dir.join("encryption.key"), blob).unwrap();
        let (r, action) = ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        assert_eq!(action, LoginAction::Repaired, "active-org.json 仍在 → 修复");
        assert_eq!(r.org_uuid, org0, "换 key 也不换 org");
        let key = read_oauth_key(&dir);
        let body = std::fs::read_to_string(the_enc_file(&dir)).unwrap();
        assert!(
            decrypt_token_v2(&body, &key).is_ok(),
            "重造 key 后新 .enc 应可解"
        );
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn ensure_repairs_when_token_structurally_damaged() {
        // .enc 能解密但结构损坏(provider 篡改 / account 非 UUID)→ 不误判 Reused,走修复保 org。
        let dir = tmpdir("bad-struct");
        let fake_real = tmpdir("realcred-bs");
        ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        let org0 = read_active_org_uuid(&dir);
        rewrite_enc(
            &dir,
            serde_json::json!({"account_uuid": "not-a-uuid", "provider": "tampered"}),
        );
        let (r, action) = ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        assert_eq!(action, LoginAction::Repaired, "结构损坏应修复而非复用");
        assert_eq!(r.org_uuid, org0, "修复仍保 org");
        // 修复后应重新自洽(provider=claude_ai、account 合法 UUID)
        assert!(
            looks_like_uuid(&r.account_uuid),
            "修复后 account 应为合法 UUID"
        );
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn ensure_repairs_when_token_expired() {
        // .enc 可解但 token 已过期 → 不误判 Reused,走修复;修复后(远期)应可复用。
        let dir = tmpdir("expired");
        let fake_real = tmpdir("realcred-exp");
        ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        let org0 = read_active_org_uuid(&dir);
        rewrite_enc(
            &dir,
            serde_json::json!({"token_expires_at": "2000-01-01T00:00:00.000Z"}),
        );
        let (r, action) = ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        assert_eq!(action, LoginAction::Repaired, "过期令牌不应误判 Reused");
        assert_eq!(r.org_uuid, org0);
        // 修复后新令牌远期未过期 → 再次 ensure 应可复用。
        let (_r2, a2) = ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        assert_eq!(a2, LoginAction::Reused, "修复后应可复用");
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn free_subscription_blob_is_not_intact() {
        // R5/E2E-4 的单元锚点:subscription_type 被降级成 free 的 blob 不得判为自洽,
        // ensure 必须修复回 max 并保住 org。
        let dir = tmpdir("free-blob");
        let fake_real = tmpdir("realcred-free");
        ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        let org0 = read_active_org_uuid(&dir);
        rewrite_enc(&dir, serde_json::json!({"subscription_type": "free"}));
        let (r, action) = ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        assert_eq!(action, LoginAction::Repaired, "free blob 不得判 intact");
        assert_eq!(r.org_uuid, org0, "修复仍保 org");
        let key = read_oauth_key(&dir);
        let body = std::fs::read_to_string(the_enc_file(&dir)).unwrap();
        let blob: serde_json::Value =
            serde_json::from_slice(&decrypt_token_v2(&body, &key).unwrap()).unwrap();
        assert_eq!(blob["subscription_type"], "max", "修复后必须回到 max");
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn email_outside_invalid_domain_is_not_intact() {
        // email 不是假账号域 → 判非 intact,修复回虚拟身份,保 org。
        let dir = tmpdir("real-email");
        let fake_real = tmpdir("realcred-email");
        ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        let org0 = read_active_org_uuid(&dir);
        rewrite_enc(&dir, serde_json::json!({"email": "someone@example.com"}));
        let (r, action) = ensure_virtual_login_guarded(&dir, EMAIL, &dir, &fake_real).unwrap();
        assert_eq!(action, LoginAction::Repaired, "真实域 email 不得判 intact");
        assert_eq!(r.org_uuid, org0);
        let key = read_oauth_key(&dir);
        let body = std::fs::read_to_string(the_enc_file(&dir)).unwrap();
        let blob: serde_json::Value =
            serde_json::from_slice(&decrypt_token_v2(&body, &key).unwrap()).unwrap();
        assert_eq!(blob["email"], EMAIL);
        for d in [dir, fake_real] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn token_expiry_check() {
        assert!(token_not_expired("2099-01-01T00:00:00.000Z"));
        assert!(!token_not_expired("2000-01-01T00:00:00.000Z"));
        assert!(!token_not_expired(""), "空串视为过期");
        assert!(!token_not_expired("2099-13"), "太短视为过期");
        assert!(!token_not_expired("20990101ZZ"), "格式不对视为过期");
        let t = today_utc_ymd();
        assert_eq!(t.len(), 10);
        assert_eq!(&t[4..5], "-");
        assert_eq!(&t[7..8], "-");
    }
}
