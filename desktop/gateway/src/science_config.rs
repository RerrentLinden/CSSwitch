//! 启动覆盖项:CSSwitch 在启动 Science 时要替它定下来的配置键,经一份镜像配置交给它。
//!
//! Science 的 `config.toml` 只在启动时读取。有两个根表键只能从这里给:
//! - `enable_web_search = false` —— 联网搜索总开关关闭时(Science 0.1.56 起支持);
//! - `default_model = "<清单里的 id>"` —— 第三方模式下新会话默认用哪个模型。第三方地址下
//!   Science 不看模型清单里有什么,默认模型只认这个键(出厂值是官方 id `claude-opus-5`);
//!   CSSwitch 的清单里没有那个 id,不指定的话界面会显示默认模型不可用。
//!
//! CSSwitch 不改写 Science 自己的文件:把「用户原配置 + 覆盖项」写成一份镜像放在 CSSwitch
//! 自己的目录里,启动时用 `--config <镜像>` 交给 Science。`--config` 只换配置文件,不换数据
//! 目录(2026-10-10 用 0.1.60 隔离实测),登录与对话不受影响。
//!
//! 合同([`LaunchOverrides`] → 镜像):
//!
//! | `web_search` | `default_model` | 结果 |
//! | --- | --- | --- |
//! | 开 | 无 | 不生成、不带 `--config`,并删掉上次留下的镜像 |
//! | 关 | 无 | 镜像 = 源 + `enable_web_search = false` |
//! | 开 | 有 | 镜像 = 源 + `default_model` |
//! | 关 | 有 | 镜像 = 源 + 两个键 |
//!
//! - 第一行是官方模式 + 联网搜索开启:启动方式与没有 CSSwitch 的这些开关时逐项相同。
//!   Science 自己配置里的任何值(包括别人手写的 `enable_web_search = false`)原样生效;
//! - 镜像保留源文件的全部内容,只在**根表**写上面这两个键(源里已有同名键就覆盖),
//!   别的一概不动。联网搜索开启时不碰 `enable_web_search`:CSSwitch 不替用户放宽;
//! - 对 Science 的目录只读 `config.toml` 一个文件,不写、不建任何东西;
//! - 源文件读不了或解析不了就报错,不写镜像:这样的文件 Science 自己也起不来,
//!   猜着改只会掩盖问题;
//! - 每次启动都重新生成:用户改了自己的 `config.toml`、或在 CSSwitch 里换了默认槽之后,
//!   下一次经 CSSwitch 启动就带上新内容。

use std::path::{Path, PathBuf};

/// Science 根表里关闭联网搜索的键。必须写在第一个 `[table]` 之前。
const WEB_SEARCH_KEY: &str = "enable_web_search";
/// Science 根表里指定新会话默认模型的键。同样必须在根表。
const DEFAULT_MODEL_KEY: &str = "default_model";
const CONFIG_FILE: &str = "config.toml";

/// 一次启动要替 Science 定下来的配置。由调用方给出(`control.rs::launch_overrides`),
/// 这个模块不读 CSSwitch 的配置文件。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchOverrides {
    /// 联网搜索总开关的当前值。`false` → 镜像里写 `enable_web_search = false`。
    pub web_search: bool,
    /// 新会话的默认模型:模型清单里承载默认槽的那一条的 id。官方模式下是 `None`。
    pub default_model: Option<String>,
    /// 跳过审批总开关。它是 `serve` 的命令行参数,不是配置键:不进镜像,也不影响
    /// [`needs_mirror`](Self::needs_mirror)。
    pub skip_approvals: bool,
}

impl LaunchOverrides {
    /// 这次启动要不要带镜像配置。不要的时候,启动参数里没有 `--config`。
    pub fn needs_mirror(&self) -> bool {
        !self.web_search || self.default_model.is_some()
    }
}

/// `~/.csswitch/science-config/` —— 镜像目录,CSSwitch 独占管理,可随时删除。
fn mirror_dir() -> PathBuf {
    crate::profile::config_dir().join("science-config")
}

/// 官方实例本次启动要带的 `--config`。源是用户自己的 `~/.claude-science/config.toml`(只读)。
pub fn official_launch_config(overrides: &LaunchOverrides) -> Result<Option<PathBuf>, String> {
    launch_config(
        overrides,
        &crate::profile::home()
            .join(".claude-science")
            .join(CONFIG_FILE),
        &mirror_dir().join("official.toml"),
    )
}

/// 免登录沙箱本次启动要带的 `--config`。源是沙箱数据目录里的 `config.toml`。
pub fn sandbox_launch_config(overrides: &LaunchOverrides) -> Result<Option<PathBuf>, String> {
    launch_config(
        overrides,
        &crate::sandbox::sandbox_data_dir().join(CONFIG_FILE),
        &mirror_dir().join("sandbox.toml"),
    )
}

/// 本次启动要不要带 `--config`。
///
/// - 没有覆盖项 → `Ok(None)`:不生成、不带参数;顺手删掉上次留下的镜像(尽力而为),
///   免得留一份看起来还在生效的旧文件。
/// - 有覆盖项 → 由 `source` 生成 `mirror`,返回 `Ok(Some(mirror))`。
///   `source` 不存在时镜像里只有覆盖项。
pub fn launch_config(
    overrides: &LaunchOverrides,
    source: &Path,
    mirror: &Path,
) -> Result<Option<PathBuf>, String> {
    if !overrides.needs_mirror() {
        let _ = std::fs::remove_file(mirror);
        return Ok(None);
    }
    let text = mirror_text(source, overrides)?;
    write_mirror(mirror, &text)?;
    Ok(Some(mirror.to_path_buf()))
}

/// 源配置 + 根表里的覆盖项。用真正的 TOML 实现读写:按行处理文本会在多行字符串、
/// 行内表这些边角上出错,而解析失败的配置会让 Science 起不来。
fn mirror_text(source: &Path, overrides: &LaunchOverrides) -> Result<String, String> {
    let original = match std::fs::read_to_string(source) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(format!(
                "读取 Science 配置文件 {} 失败:{error}",
                source.display()
            ))
        }
    };
    let mut document = original
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| parse_failure(source, &original, &error, overrides))?;
    // 根表的键值总是写在所有 `[table]` 之前;原本有这个键就覆盖,没有就新增。
    if !overrides.web_search {
        document[WEB_SEARCH_KEY] = toml_edit::value(false);
    }
    if let Some(default_model) = &overrides.default_model {
        document[DEFAULT_MODEL_KEY] = toml_edit::value(default_model.as_str());
    }
    Ok(document.to_string())
}

/// 解析失败的报错:路径 + 行列 + 解析器原话。不带出错行的内容 —— 那一行可能是
/// 带口令的代理地址,而这条报错会进控制面响应。
///
/// 为什么需要这个文件、怎么绕开,按这次的覆盖项如实写:第三方模式下每次启动都要生成镜像,
/// 「保持联网搜索开启」对它不是出路。
fn parse_failure(
    source: &Path,
    original: &str,
    error: &toml_edit::TomlError,
    overrides: &LaunchOverrides,
) -> String {
    let position = error
        .span()
        .map(|span| {
            // span 是字节偏移;退到字符边界再切片,免得报错本身在多字节字符中间 panic。
            let mut start = span.start.min(original.len());
            while !original.is_char_boundary(start) {
                start -= 1;
            }
            let before = &original[..start];
            let line = before.matches('\n').count() + 1;
            let column = before
                .rsplit('\n')
                .next()
                .map_or(0, |tail| tail.chars().count())
                + 1;
            format!("第 {line} 行第 {column} 列:")
        })
        .unwrap_or_default();
    let mut reasons = Vec::new();
    if overrides.default_model.is_some() {
        reasons.push("在第三方模式下指定默认模型");
    }
    if !overrides.web_search {
        reasons.push("关闭联网搜索");
    }
    let way_out = if overrides.default_model.is_some() {
        "请先修正该文件。"
    } else {
        "请先修正该文件,或保持联网搜索开启。"
    };
    format!(
        "Science 配置文件 {} 无法解析({position}{})。{}需要以它为基础生成启动配置,{way_out}",
        source.display(),
        error.message().trim(),
        reasons.join("、")
    )
}

/// 同目录临时文件 → `0600` → rename;目录 `0700`。镜像带着用户原配置的全部内容
/// (可能含代理口令),权限从创建那一刻起就收紧。
fn write_mirror(mirror: &Path, text: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let failure = |detail: String| format!("写入启动配置 {} 失败:{detail}", mirror.display());
    let dir = mirror
        .parent()
        .ok_or_else(|| failure("目标没有父目录".into()))?;
    std::fs::create_dir_all(dir).map_err(|e| failure(format!("创建目录:{e}")))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| failure(format!("收紧目录权限:{e}")))?;
    crate::sandbox_forge::safe_write(mirror, text.as_bytes(), 0o600).map_err(failure)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::os::unix::fs::PermissionsExt;

    /// 一个临时目录里的「Science 目录」与「CSSwitch 镜像目录」。
    struct Dirs {
        base: PathBuf,
        source: PathBuf,
        mirror: PathBuf,
    }

    impl Dirs {
        fn new(tag: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "csswitch-science-config-test-{tag}-{}-{}",
                std::process::id(),
                crate::sandbox_forge::rand_hex(4).unwrap()
            ));
            let science = base.join("science");
            std::fs::create_dir_all(&science).unwrap();
            Self {
                source: science.join("config.toml"),
                mirror: base.join("csswitch/science-config/official.toml"),
                base,
            }
        }

        fn with_source(tag: &str, text: &str) -> Self {
            let dirs = Self::new(tag);
            std::fs::write(&dirs.source, text).unwrap();
            dirs
        }

        /// 按覆盖项生成镜像,并用 TOML 解析器把它读回来。
        fn mirrored(&self, overrides: &LaunchOverrides) -> Value {
            let path = launch_config(overrides, &self.source, &self.mirror)
                .unwrap()
                .unwrap();
            assert_eq!(path, self.mirror, "返回的路径就是要交给 --config 的镜像");
            parse_tree(&std::fs::read_to_string(&path).unwrap())
        }
    }

    /// 官方模式:没有默认模型要指定,只有联网搜索总开关这一项。
    fn official(web_search: bool) -> LaunchOverrides {
        LaunchOverrides {
            web_search,
            default_model: None,
            skip_approvals: false,
        }
    }

    /// 第三方模式:总是要把默认模型指到清单里的条目上。
    fn third_party(web_search: bool) -> LaunchOverrides {
        LaunchOverrides {
            web_search,
            default_model: Some(MENU_ID.into()),
            skip_approvals: false,
        }
    }

    /// 清单里承载默认槽的那一条的 id(具体取值由 `static_profile.rs` 决定,这里只当字符串用)。
    const MENU_ID: &str = "claude-sonnet-19";

    impl Drop for Dirs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    /// 把 TOML 文本解析成与写法无关的值树,用来比较「语义上是否相等」。
    /// 解析失败(例如同一个键写了两遍)直接 panic。
    fn parse_tree(text: &str) -> Value {
        let document: toml_edit::DocumentMut = text.parse().expect("镜像必须是合法 TOML");
        table_tree(document.as_table())
    }

    fn table_tree(table: &toml_edit::Table) -> Value {
        Value::Object(
            table
                .iter()
                .map(|(key, item)| (key.to_string(), item_tree(item)))
                .collect(),
        )
    }

    fn item_tree(item: &toml_edit::Item) -> Value {
        match item {
            toml_edit::Item::None => Value::Null,
            toml_edit::Item::Value(value) => value_tree(value),
            toml_edit::Item::Table(table) => table_tree(table),
            toml_edit::Item::ArrayOfTables(tables) => {
                Value::Array(tables.iter().map(table_tree).collect())
            }
        }
    }

    fn value_tree(value: &toml_edit::Value) -> Value {
        match value {
            toml_edit::Value::String(text) => json!(text.value()),
            toml_edit::Value::Integer(number) => json!(number.value()),
            toml_edit::Value::Float(number) => json!(number.value()),
            toml_edit::Value::Boolean(flag) => json!(flag.value()),
            toml_edit::Value::Datetime(when) => json!(when.value().to_string()),
            toml_edit::Value::Array(items) => Value::Array(items.iter().map(value_tree).collect()),
            toml_edit::Value::InlineTable(table) => Value::Object(
                table
                    .iter()
                    .map(|(key, value)| (key.to_string(), value_tree(value)))
                    .collect(),
            ),
        }
    }

    /// 管理员下发的配置里常见的形状:根键、注释、普通表、嵌套表、行内表、数组、
    /// 多行字符串(里面故意放了看起来像表头和键值的行)、表数组。
    const ADMIN_CONFIG: &str = r#"# 由 IT 统一下发,请勿修改
telemetry = false
update_channel = "stable"   # 行尾注释

[network]
proxy = "http://proxy.corp.example:8080"
ca_bundle = "/etc/ssl/corp-ca.pem"
no_proxy = ["localhost", "127.0.0.1"]
limits = { connect_timeout = 5, retries = 3 }

[conda]
channels = ["https://mirror.corp.example/conda-forge"]
notes = """
[not_a_table]
enable_web_search = true
"""

[sandbox.network]
allowed_domains = ["pypi.org", "files.pythonhosted.org"]

[[mcp.servers]]
name = "pubmed"

[[mcp.servers]]
name = "internal"
"#;

    #[test]
    fn official_mode_with_web_search_on_passes_nothing_and_writes_nothing() {
        let dirs = Dirs::with_source("on", ADMIN_CONFIG);
        assert_eq!(
            launch_config(&official(true), &dirs.source, &dirs.mirror),
            Ok(None),
            "官方模式 + 联网搜索开启:不带 --config"
        );
        assert!(
            !dirs.mirror.parent().unwrap().exists(),
            "没有覆盖项时不得生成任何文件"
        );

        // 上次留下的镜像要清掉:留着它,看起来像是还在生效。两种来路都一样 ——
        // 关过联网搜索,或者在第三方模式下启动过。
        for leftover_from in [official(false), third_party(true)] {
            launch_config(&leftover_from, &dirs.source, &dirs.mirror).unwrap();
            assert!(dirs.mirror.is_file());
            assert_eq!(
                launch_config(&official(true), &dirs.source, &dirs.mirror),
                Ok(None)
            );
            assert!(
                !dirs.mirror.exists(),
                "回到没有覆盖项的状态后不留残余镜像:{leftover_from:?}"
            );
        }
    }

    #[test]
    fn without_a_source_config_the_mirror_holds_only_the_overrides() {
        // Science 每个键都有默认值,用户没有 config.toml 是常态。
        let dirs = Dirs::new("no-source");
        assert_eq!(
            dirs.mirrored(&official(false)),
            json!({"enable_web_search": false})
        );
        assert_eq!(
            dirs.mirrored(&third_party(true)),
            json!({"default_model": MENU_ID}),
            "联网搜索开着:镜像里不出现 enable_web_search"
        );
        assert_eq!(
            dirs.mirrored(&third_party(false)),
            json!({"enable_web_search": false, "default_model": MENU_ID})
        );
        assert!(!dirs.source.exists(), "不得替用户创建 config.toml");
    }

    #[test]
    fn third_party_mode_points_the_default_model_at_the_menu_entry() {
        // 回归目标:键被追加到文件末尾(落进最后一张表而不是根表,Science 读不到,
        // 新会话的默认模型就停在清单里已经没有的 claude-opus-5 上),或者镜像丢了别的键。
        let dirs = Dirs::with_source("third-party", ADMIN_CONFIG);
        let mut expected = parse_tree(ADMIN_CONFIG);
        assert!(
            expected.get("default_model").is_none() && expected.get("enable_web_search").is_none(),
            "夹具前提:源配置的根表里没有这两个键"
        );
        expected["default_model"] = json!(MENU_ID);
        assert_eq!(
            dirs.mirrored(&third_party(true)),
            expected,
            "联网搜索开启:除根表新增的 default_model 外逐值相等"
        );

        expected["enable_web_search"] = json!(false);
        assert_eq!(
            dirs.mirrored(&third_party(false)),
            expected,
            "联网搜索关闭:两个键都在根表,其余不变"
        );
        let text = std::fs::read_to_string(&dirs.mirror).unwrap();
        assert!(text.contains("# 由 IT 统一下发,请勿修改"), "注释原样保留");
    }

    #[test]
    fn third_party_mode_overrides_the_users_default_model_and_nothing_else() {
        // 用户自己写过 default_model(多半是个官方 id):必须换成清单里有的那一条,
        // 且不能再追加一行同名键(重复键是非法 TOML,parse_tree 遇到会直接 panic)。
        // 用户手写的 enable_web_search = false 原样保留:总开关开着不等于替用户放宽。
        let dirs = Dirs::with_source(
            "own-default",
            "default_model = \"claude-opus-5\"\nenable_web_search = false\nmax_tokens = 64000\n\n[network]\nproxy = \"http://p:1\"\n",
        );
        assert_eq!(
            dirs.mirrored(&third_party(true)),
            json!({
                "default_model": MENU_ID,
                "enable_web_search": false,
                "max_tokens": 64000,
                "network": {"proxy": "http://p:1"}
            })
        );
    }

    #[test]
    fn switch_off_keeps_every_other_setting_and_lands_in_the_root_table() {
        // 回归目标:镜像丢键(代理、证书、白名单没了,Science 换了一套网络设置启动),
        // 或者把新键追加到文件末尾 —— 那样它会落进最后一张表而不是根表,Science 读不到。
        let dirs = Dirs::with_source("admin", ADMIN_CONFIG);
        let mut expected = parse_tree(ADMIN_CONFIG);
        assert!(
            expected.get("enable_web_search").is_none(),
            "夹具前提:源配置的根表里没有这个键"
        );
        expected["enable_web_search"] = json!(false);

        let mirrored = dirs.mirrored(&official(false));
        assert_eq!(mirrored, expected, "除根表新增的一个键外,逐值相等");
        // 多行字符串里那行同名文本仍然只是字符串内容。
        assert_eq!(
            mirrored["conda"]["notes"],
            json!("[not_a_table]\nenable_web_search = true\n")
        );
        let text = std::fs::read_to_string(&dirs.mirror).unwrap();
        assert!(text.contains("# 由 IT 统一下发,请勿修改"), "注释原样保留");
    }

    #[test]
    fn switch_off_overrides_an_explicit_true_without_duplicating_the_key() {
        // 源里已经写了 true:必须改成 false,且不能再追加一行同名键
        // (重复键是非法 TOML,Science 会起不来;parse_tree 遇到会直接 panic)。
        let dirs = Dirs::with_source(
            "explicit-true",
            "# 我要搜索\nenable_web_search = true\n\n[network]\nproxy = \"http://p:1\"\n",
        );
        assert_eq!(
            dirs.mirrored(&official(false)),
            json!({"enable_web_search": false, "network": {"proxy": "http://p:1"}})
        );
    }

    #[test]
    fn switch_off_survives_a_source_without_a_trailing_newline() {
        // 最后一行没有换行符时,新键不能被拼到上一行后面。
        let dirs = Dirs::with_source("no-newline", "update_channel = \"beta\"");
        assert_eq!(
            dirs.mirrored(&third_party(false)),
            json!({"update_channel": "beta", "enable_web_search": false, "default_model": MENU_ID})
        );
    }

    #[test]
    fn unparseable_source_is_an_error_and_writes_no_mirror() {
        // 这样的文件 Science 自己也起不来。吞掉错误去「尽量生成」只会让开关显示成
        // 已生效,而实际启动的是一份被猜着改过的配置。
        let broken = "[network]\nproxy = http://user:hunter2@proxy.corp.example:8080\n";
        let dirs = Dirs::with_source("broken", broken);
        for overrides in [official(false), third_party(true), third_party(false)] {
            let error = launch_config(&overrides, &dirs.source, &dirs.mirror).unwrap_err();
            assert!(
                error.contains(&dirs.source.display().to_string()),
                "报错要指出是哪个文件:{error}"
            );
            assert!(error.contains("第 2 行"), "报错要指出位置:{error}");
            assert!(
                !error.contains("hunter2"),
                "报错会进控制面响应,不得带出配置内容:{error}"
            );
            assert!(!dirs.mirror.exists(), "解析失败不得写镜像");
            // 报错里给的出路必须是真的:第三方模式下每次启动都要生成镜像,
            // 「保持联网搜索开启」绕不过去。
            assert_eq!(
                error.contains("保持联网搜索开启"),
                overrides.default_model.is_none(),
                "{overrides:?}:{error}"
            );
        }
    }

    #[test]
    fn unreadable_source_is_an_error_rather_than_an_empty_config() {
        // 只有「不存在」才等同于空配置。读不了(这里用目录顶替文件)若也当成空配置,
        // 用户的代理与证书设置会被悄悄丢掉。
        let dirs = Dirs::new("unreadable");
        std::fs::create_dir_all(&dirs.source).unwrap();
        let error = launch_config(&third_party(true), &dirs.source, &dirs.mirror).unwrap_err();
        assert!(
            error.contains(&dirs.source.display().to_string()),
            "{error}"
        );
        assert!(!dirs.mirror.exists());
    }

    #[test]
    fn source_directory_is_only_read() {
        // 对 Science 自己目录的合同:只读 config.toml,不写、不建文件。
        let dirs = Dirs::with_source("read-only", ADMIN_CONFIG);
        launch_config(&third_party(false), &dirs.source, &dirs.mirror).unwrap();
        launch_config(&official(true), &dirs.source, &dirs.mirror).unwrap();
        assert_eq!(
            std::fs::read_to_string(&dirs.source).unwrap(),
            ADMIN_CONFIG,
            "源配置逐字节不变"
        );
        let entries: Vec<_> = std::fs::read_dir(dirs.source.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, ["config.toml"], "源目录里不得多出任何文件");
    }

    #[test]
    fn mirror_is_private_and_regenerated_on_every_launch() {
        let dirs = Dirs::with_source("perm", "[network]\nproxy = \"http://old:1\"\n");
        launch_config(&third_party(true), &dirs.source, &dirs.mirror).unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        // 镜像带着用户原配置的全部内容(可能含代理口令)。
        assert_eq!(mode(&dirs.mirror), 0o600);
        assert_eq!(mode(dirs.mirror.parent().unwrap()), 0o700);

        // Science 只在启动时读配置:用户改了自己的文件,下一次启动的镜像必须跟上。
        std::fs::write(&dirs.source, "[network]\nproxy = \"http://new:2\"\n").unwrap();
        assert_eq!(
            dirs.mirrored(&third_party(true))["network"]["proxy"],
            json!("http://new:2")
        );
        let leftovers: Vec<_> = std::fs::read_dir(dirs.mirror.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, ["official.toml"], "临时文件必须被改名掉");
    }
}
