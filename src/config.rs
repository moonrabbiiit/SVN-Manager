use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DirConfig {
    /// 工作副本目录
    pub path: String,
    /// 列表里显示的别名，可为空
    pub label: String,
    /// 「更多 → 绑定对比对象」里绑定的另一个目录，Beyond Compare 拿它当对比的另一侧
    pub bc_target: String,
    /// 文件夹对比的名称筛选（如 `-*.iml;-*.classpath`），由本程序自己保存，
    /// 不再依赖本机 Beyond Compare 的对比记录，换台机器、换个人也能一致地带出
    pub bc_filter: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub svn_exe: String,
    /// Beyond Compare 路径，留空则自动寻找
    pub bc_exe: String,
    /// 调用对比时服务器端放在哪一侧：left / right
    pub bc_server_side: String,
    /// 留空则使用 svn 凭据缓存
    pub auth_user: String,
    pub auth_pass: String,
    pub dirs: Vec<DirConfig>,
    /// 提交记录读取条数
    pub log_limit: i64,
    /// 自动刷新间隔（秒），0 表示关闭
    pub auto_refresh: u64,
    /// 主题：system / light / dark
    pub theme: String,
    /// 打开「提交记录」页时默认只看本机登录人的记录（关掉就是默认看所有人的）
    pub history_only_mine: bool,
    /// 更新源：official = GitHub 仓库的最新发布，custom = 下面这个自建服务端地址
    pub update_source: String,
    /// 版本更新服务端地址（服务根目录），留空则不检查更新
    pub update_server: String,
    /// 启动时自动检查版本更新
    pub check_update_on_start: bool,
    /// 运行中定期自动检查版本更新（每 5 分钟一次，见 `update::AUTO_CHECK_EVERY_SECS`）
    pub auto_update_check: bool,
    /// 提交成功后自动补跑一次 `svn update`，把整棵工作副本树的版本号推到最新
    pub update_after_commit: bool,
    /// 开机自动启动（实际生效写在注册表 HKCU Run 键里，这里只做记录）
    pub auto_start: bool,
    /// 主页目录列表的样式：false = 详细（整行 + 一排按钮），true = 简约（正方形卡片 + 「+」菜单）
    pub home_compact: bool,
    /// AI 生成工作日志：接口地址（OpenAI 兼容的 /chat/completions 完整地址）
    pub ai_url: String,
    /// AI 生成工作日志：接口密钥（明文保存在本机配置文件）
    pub ai_key: String,
    /// AI 生成工作日志：模型名（如 deepseek-chat / gpt-4o-mini）
    pub ai_model: String,
    /// AI 生成工作日志：口吻说明（生成窗口打开时预填进输入框，随改随存）
    pub ai_tone: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            svn_exe: String::new(),
            bc_exe: String::new(),
            // 默认服务器端在左：文件对比的 BASE 版本一直在左，用户的 BC 记录也是 server 在左
            bc_server_side: "left".to_owned(),
            auth_user: String::new(),
            auth_pass: String::new(),
            dirs: Vec::new(),
            log_limit: 100,
            auto_refresh: 120,
            theme: "system".to_owned(),
            history_only_mine: true,
            // 默认查官方 GitHub 发布：不用填任何地址就能检查更新；要用内网服务端在设置里切
            update_source: "official".to_owned(),
            update_server: String::new(),
            check_update_on_start: true,
            // 默认关闭：每 5 分钟联网查一次，要不要开由用户决定
            auto_update_check: false,
            // 默认开启：提交后补一次 update，列表里的「本地 r」会立刻跟上新版本
            update_after_commit: true,
            // 默认关闭：开机自启要用户自己决定，开了就写 HKCU Run 键
            auto_start: false,
            // 默认详细样式：老用户升级后看到的列表跟以前一样，要卡片自己切过去
            home_compact: false,
            // AI 日志的地址 / 密钥 / 模型都由用户自己提供，默认全空
            ai_url: String::new(),
            ai_key: String::new(),
            ai_model: String::new(),
            ai_tone: String::new(),
        }
    }
}

fn config_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("SVNManager")
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.json")
}

impl Config {
    pub fn load() -> Self {
        let text = std::fs::read_to_string(config_file()).unwrap_or_default();
        // 用记事本等编辑器手工改过的配置常带 UTF-8 BOM，先去掉再解析
        serde_json::from_str(text.trim_start_matches('\u{feff}')).unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let path = config_file();
        std::fs::create_dir_all(config_dir()).map_err(|e| e.to_string())?;
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, text.as_bytes()).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 老配置文件缺字段时按默认值补齐；已经不存在的全局「忽略空白」开关（改成按文件类型
    /// 逐条决定）留在旧配置里也必须能忽略，否则升级后配置直接解析失败。
    #[test]
    fn old_config_without_the_flag_keeps_the_default() {
        let parsed: Config =
        serde_json::from_str(r#"{"svn_exe":"svn","dirs":[{"path":"D:\\a","label":"A"}],"diff_ignore_white":true}"#).unwrap();
        assert_eq!(parsed.log_limit, 100);
        // 老配置里的目录条目没有 bc_target，也要按空值补齐而不是整个配置解析失败
        assert!(parsed.dirs[0].bc_target.is_empty());
        // 后加的「名称筛选」字段：老配置没有也要按空值补齐，且能写回
        assert!(parsed.dirs[0].bc_filter.is_empty());
        // 服务器端所在侧是后加的设置，老配置没有也要按默认值（左侧）补齐
        assert_eq!(parsed.bc_server_side, "left");
        assert_eq!(parsed.theme, "system");
        // 历史页「默认只看本人」保持开启，老用户的使用习惯不变
        assert!(parsed.history_only_mine);
        let off: Config = serde_json::from_str(r#"{"history_only_mine":false}"#).unwrap();
        assert!(!off.history_only_mine);
        // 版本更新是后加的设置：老配置没有也要按默认值补齐（不检查、地址为空）
        assert!(parsed.update_server.is_empty());
        assert!(parsed.check_update_on_start);
        // 定期自动检查是后加的设置：老配置没有要按默认值（关闭）补齐
        assert!(!parsed.auto_update_check);
        assert!(
            serde_json::from_str::<Config>(r#"{"auto_update_check":true}"#)
                .unwrap()
                .auto_update_check
        );
        // 更新源也是后加的：老配置没有要按默认值（官方 GitHub）补齐，写了 custom 才用自建地址
        assert_eq!(parsed.update_source, "official");
        assert_eq!(
            serde_json::from_str::<Config>(r#"{"update_source":"custom"}"#)
                .unwrap()
                .update_source,
            "custom"
        );
        // 提交后自动更新是后加的设置：老配置没有也要按默认值（开启）补齐
        assert!(parsed.update_after_commit);
        // 开机自启是后加的设置：老配置没有也要按默认值（关闭）补齐
        assert!(!parsed.auto_start);
        // 主页样式是后加的设置：老配置没有要按默认值（详细）补齐，写了 true 才用卡片
        assert!(!parsed.home_compact);
        assert!(
            serde_json::from_str::<Config>(r#"{"home_compact":true}"#)
                .unwrap()
                .home_compact
        );
        // AI 日志是后加的设置：老配置没有也要按空值补齐
        assert!(parsed.ai_url.is_empty());
        assert!(parsed.ai_key.is_empty());
        assert!(parsed.ai_model.is_empty());
        assert!(parsed.ai_tone.is_empty());
    }
}
