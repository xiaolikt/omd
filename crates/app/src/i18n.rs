//! 原生 UI 的中英文案。
//!
//! dsh 的 `locale.preference` 是唯一语言来源（`zh` / `en`），设置页写入、
//! settings/describe 读回后都走这里。`tr` 只接受字面量，返回 `'static str`，
//! 因此可以直接替换原来的英文字符串而不改变类型；动态生成的文案用 `trs`。

use std::sync::atomic::{AtomicU8, Ordering};

static LANG: AtomicU8 = AtomicU8::new(0);

/// 根据 dsh 的 locale 值切换界面语言。`zh`、`zh-CN`、`ZH_hans` 都视为中文。
pub fn set_preference(preference: &str) {
    let primary = preference
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let zh = primary == "zh";
    LANG.store(if zh { 1 } else { 0 }, Ordering::Relaxed);
}

pub fn is_zh() -> bool {
    LANG.load(Ordering::Relaxed) == 1
}

/// 没有存储偏好的时候按系统 locale 兜底（dsh 网页版按 navigator 兜底）。
pub fn init_from_env() {
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|key| std::env::var(key).ok())
        .find(|value| !value.is_empty())
        .unwrap_or_default();
    set_preference(&locale);
}

fn lookup(en: &str) -> Option<&'static str> {
    Some(match en {
        // ---- 设置页导航 ----
        "Account & balance" => "账户与余额",
        "General" => "通用",
        "Models" => "模型",
        "Built-in plugins" => "内置插件",
        "Agent presets" => "智能体预设",
        "Browser" => "浏览器",
        "Git" => "Git",
        "File" => "文件",
        "Settings" => "设置",

        // ---- 通用设置 ----
        "Appearance" => "外观",
        "Light" => "浅色",
        "Dark" => "深色",
        "System" => "跟随系统",
        "Font size" => "字号",
        "Only affects conversation content" => "仅影响对话内容",
        "Work details" => "工作详情",
        "Choose how much detail to show for tool calls" => "选择工具调用的详情展示程度",
        "Compact" => "紧凑",
        "Standard" => "标准",
        "Detailed" => "详细",
        "Verbose" => "冗长",
        "When to Collapse Work Details" => "何时折叠工作详情",
        "Choose when to automatically collapse work details" => "选择自动折叠工作详情的时机",
        "On completion" => "完成时",
        "On next input" => "下一次输入时",
        "On next message" => "下一条消息时",
        "Replies After Collapse" => "折叠后保留的回复",
        "Choose which assistant replies stay visible when work details collapse" => {
            "选择折叠工作详情后仍可见的助手回复"
        }
        "All replies" => "全部回复",
        "Final reply only" => "仅最终回复",
        "Final answer only" => "仅最终回答",
        "Open chat links in" => "聊天链接打开方式",
        "Choose where to open web links" => "选择网页链接的打开位置",
        "In-App Sidebar" => "应用内侧栏",
        "Default Browser" => "默认浏览器",
        "Send behavior while busy" => "忙碌时的发送行为",
        "What Enter and the Send button do while the agent is running; Cmd/Ctrl+Enter uses the other behavior" => {
            "智能体运行期间按 Enter 和发送按钮的行为；Cmd/Ctrl+Enter 使用另一种行为"
        }
        "Queue" => "排队",
        "Steer" => "引导",
        "Performance & usage" => "性能与用量",
        "Choose how much performance and usage information to show" => "选择性能与用量信息的展示程度",
        "Permission" => "权限",
        "Choose the default permission mode for new sessions" => "选择新会话的默认权限模式",
        "Read Only" => "只读",
        "Workspace Write" => "工作区可写",
        "Language" => "语言",
        "Show coding view" => "显示编码视图",
        "Shows trajectory, code diffs, and all Agent presets" => "显示执行轨迹、代码差异和全部智能体预设",
        "Upload Session Log when using the official model API" => "使用官方模型 API 时上传会话日志",
        "Help improve DeepSeek models and products." => "帮助改进 DeepSeek 模型和产品。",
        "Current version: unknown" => "当前版本：未知",
        "Current version" => "当前版本",

        // ---- dsh 内核 ----
        "dsh engine" => "dsh 内核",
        "the engine that runs sessions, tools and plugins" => {
            "运行会话、工具和插件的引擎"
        }
        "Upgrade engine" => "升级内核",
        "Check engine" => "检查内核",
        "current engine: unknown" => "当前内核：未知",
        "installed by omd" => "由 omd 安装",
        "system install (DSH_BIN)" => "系统安装（DSH_BIN）",
        "checking the npm registry…" => "正在查询 npm registry…",
        "releases are published on npm as @deepseek-ai/dsh" => {
            "新版发布在 npm 的 @deepseek-ai/dsh"
        }
        "Upgrading…" => "升级中…",
        "engine upgraded to v{} · restart OMD to use it" => {
            "内核已升级到 v{} · 重启 OMD 后生效"
        }
        "engine upgrade failed: {}" => "内核升级失败：{}",
        "new engine installed · restart OMD to use it" => {
            "新内核已安装 · 重启 OMD 后生效"
        }
        "Restart OMD" => "重启 OMD",

        // ---- 更新 ----
        "Checking…" => "检查中…",
        "Check now" => "立即检查",
        "this release has no build for this platform yet" => "此版本暂无该平台的构建",
        "Install updates automatically" => "自动安装更新",
        "Downloads and installs in the background; restart OMD to apply." => {
            "后台下载并安装；重启 OMD 后生效。"
        }
        "Download" => "下载",
        "Download & install" => "下载并安装",
        "contacting github.com…" => "正在连接 github.com…",
        "update available" => "有可用更新",
        "built in" => "内置",
        "already up to date" => "已是最新版本",

        // ---- 主界面 / 停靠 ----
        "New session" => "新会话",
        "No panel open in this session" => "此会话没有打开的面板",
        "No session" => "无会话",
        "Type a message below to get started" => "在下方输入消息开始",
        "Ctrl+K to pick a panel, or ＋ to create one" => "Ctrl+K 选择面板，或 ＋ 创建面板",
        "Click ＋ to create a panel" => "点击 ＋ 创建面板",
        "no tabs open" => "没有打开的标签页",
        "◆ dsh tab" => "◆ dsh 标签页",
        ">_ terminal tab" => ">_ 终端标签页",
        "⚠ Approval required" => "⚠ 需要审批",
        "Allow" => "允许",
        "Deny" => "拒绝",
        "Cancel" => "取消",
        "Save" => "保存",
        "Submit" => "提交",
        "❓ Needs your input" => "❓ 需要你输入",
        "⎇ fork here" => "⎇ 在此处分叉",
        "◎ goal" => "◎ 目标",
        "＋ goal" => "＋ 目标",
        "Ask dsh…" => "询问 dsh…",
        "Message… Enter to send" => "输入消息… Enter 发送",
        "Message… Enter to send · Shift+Enter for newline" => "输入消息… Enter 发送 · Shift+Enter 换行",
        "Pin session" => "置顶会话",
        "Unpin session" => "取消置顶",
        "Rename tab" => "重命名标签页",
        "Close tab" => "关闭标签页",
        "Close other tabs" => "关闭其它标签页",
        "Close tabs to the right" => "关闭右侧标签页",
        "Close tabs to the left" => "关闭左侧标签页",
        "Fork session" => "分叉会话",
        "Archive session" => "归档会话",
        "Tabs" => "标签页",
        "New panel" => "新建面板",
        "Terminal · bottom" => "终端 · 底部",
        "Terminal · right" => "终端 · 右侧",
        "side by side" => "并排",
        "open a page next to the chat" => "在聊天旁打开网页",
        "Switch model" => "切换模型",
        "browse the session working directory" => "浏览会话工作目录",
        "engine version, status and theme" => "引擎版本、状态与主题",
        "Git · stage all" => "Git · 全部暂存",
        "git add -A in the file root" => "在文件根目录执行 git add -A",
        "Git · unstage all" => "Git · 全部取消暂存",
        "git reset in the file root" => "在文件根目录执行 git reset",
        "Git · commit" => "Git · 提交",
        "commit staged changes with a message" => "用提交信息提交已暂存改动",
        "format document" => "格式化文档",
        "restore block" => "还原块",
        "maximize block" => "最大化块",
        "close block" => "关闭块",
        "chat block" => "聊天块",
        "terminal block" => "终端块",
        "browser block" => "浏览器块",
        "split right" => "向右分屏",
        "split down" => "向下分屏",
        "starting…" => "启动中…",
        "terminal exited" => "终端已退出",
        "↻ restart" => "↻ 重启",
        "× close" => "× 关闭",
        "load older messages" => "加载更早消息",
        "No matches" => "没有匹配项",
        "next tab" => "下一个标签页",
        "Ctrl K toggle" => "Ctrl K 切换",
        "esc close" => "esc 关闭",
        "No background jobs" => "没有后台任务",
        "Background jobs" => "后台任务",
        "No scheduled reminders for this session" => "此会话没有定时提醒",
        "Ask the agent to schedule one, e.g. “remind me every day at 9:00”." => {
            "可以让智能体安排一个，例如“每天 9:00 提醒我”。"
        }
        "No deliveries yet" => "还没有投递记录",
        "Scheduled reminders" => "定时提醒",
        "No subagents for this session" => "此会话没有子智能体",
        "Subagents" => "子智能体",

        // ---- 设置页通用卡片 ----
        "Loading settings…" => "正在加载设置…",
        "Not set" => "未设置",
        "Clear" => "清除",
        "Set" => "设置",
        "API key" => "API 密钥",
        "No plugins reported" => "没有插件信息",
        "Bundles" => "插件包",
        "packages managed by the engine" => "由引擎管理的包",
        "Inspect" => "检查",
        "Install" => "安装",
        "No bundles reported" => "没有插件包信息",
        "Remove" => "移除",
        "Market" => "市场",
        "community plugin index" => "社区插件索引",
        "No market index configured" => "未配置市场索引",
        "No plugins matched" => "没有匹配的插件",
        "installing…" => "安装中…",
        "Allow & install" => "允许并安装",
        "Update" => "更新",
        "Edit" => "编辑",
        "DeepSeek account" => "DeepSeek 账户",
        "Signed in" => "已登录",
        "Not signed in" => "未登录",
        "Third-party providers" => "第三方服务商",
        "Custom model API" => "自定义模型 API",
        "Enter a route id, base URL and model list for any compatible endpoint." => {
            "为任意兼容端点填写路由 ID、基础 URL 和模型列表。"
        }
        "New custom provider" => "新建自定义服务商",
        "DeepSeek API key" => "DeepSeek API 密钥",
        "Stored as DEEPSEEK_API_KEY" => "保存为 DEEPSEEK_API_KEY",
        "Enter a route id first, then add models" => "请先填写路由 ID，再添加模型",
        "+ Add model" => "+ 添加模型",
        "Route id" => "路由 ID",
        "Display name" => "显示名称",
        "Base URL" => "基础 URL",
        "Key env" => "密钥环境变量",
        "Protocol" => "协议",
        "No providers match" => "没有匹配的服务商",
        "+  Add model provider" => "+ 添加模型服务商",
        "ID" => "ID",
        "Context window" => "上下文窗口",
        "Max output tokens" => "最大输出 token",
        "Set default" => "设为默认",
        "No models configured" => "未配置模型",
        "Use" => "使用",
        "No models yet — add a provider or set an API key" => {
            "还没有模型——请添加服务商或设置 API 密钥"
        }
        "Sign out" => "退出登录",
        "Sign in" => "登录",
        "Account" => "账户",
        "Balance" => "余额",
        "Top up" => "充值",
        "No presets reported" => "没有预设信息",
        "Default" => "默认",
        "Renderer" => "渲染器",
        "Home URL" => "主页 URL",
        "CDP port" => "CDP 端口",
        "CDP endpoint" => "CDP 端点",
        "Add Signed-off-by when committing" => "提交时添加 Signed-off-by",
        "Append `Signed-off-by: <name> <email>` to commit messages" => {
            "在提交信息末尾追加 `Signed-off-by: <name> <email>`"
        }
        "Show hidden files" => "显示隐藏文件",
        "Include dotfiles in the file tree" => "在文件树中包含点文件",
        "Model" => "模型",
        "permission preset (sandbox + approval policy)" => "权限预设（沙箱 + 审批策略）",
        "no writes, no commands" => "不可写、不可执行命令",
        "write inside the workspace" => "仅可写工作区",
        "no sandbox, rare approvals" => "无沙箱，少量审批",
        "Pick one" => "请选择",
        "not a git repository" => "不是 Git 仓库",
        "working tree clean" => "工作区干净",
        "Staged" => "已暂存",
        "Unstage" => "取消暂存",
        "Unstaged" => "未暂存",
        "Stage all" => "全部暂存",
        "Working tree" => "工作区",
        "Summary" => "摘要",
        "✦  Write it" => "✦  写好了",
        "Amend" => "修补提交",
        "Generate message" => "生成提交信息",
        "(empty)" => "（空）",
        "commit message is empty" => "提交信息为空",
        "git failed" => "Git 操作失败",
        "New file" => "新建文件",
        "New folder" => "新建文件夹",
        "Refresh" => "刷新",
        "Cannot decode image" => "无法解码图片",
        "Open with system player" => "用系统播放器打开",
        "Media" => "媒体",
        "Video · system player" => "视频 · 系统播放器",
        "Audio · system player" => "音频 · 系统播放器",
        "binary file" => "二进制文件",
        "no hits" => "没有结果",
        "Find in scrollback…" => "在回滚缓冲区中查找…",
        "Search or enter address…" => "搜索或输入地址…",
        "Search settings" => "搜索设置",
        "Search models…" => "搜索模型…",
        "Search providers…" => "搜索服务商…",
        "Filter plugins…" => "筛选插件…",
        "Search the plugin market…" => "搜索插件市场…",
        "npm name, git URL, path or tarball…" => "npm 名称、Git URL、路径或压缩包…",
        "What should this session achieve?" => "这个会话要达成什么目标？",
        "rounds cap (optional)" => "轮次上限（可选）",
        "Message the selected subagent…" => "给选中的子智能体发消息…",
        "Title" => "标题",
        "Prompt" => "提示词",
        "Run a command or go to a pane…" => "运行命令或跳转到面板…",
        "Name…" => "名称…",
        "Why this change..." => "修改原因…",
        "Custom answer (optional)" => "自定义回答（可选）",
        "Leave blank to use the default endpoint" => "留空则使用默认端点",
        "model ids, comma separated" => "模型 ID，用逗号分隔",
        "paste API key (stored in credentials)" => "粘贴 API 密钥（保存在凭证中）",
        "model id" => "模型 ID",
        "display name" => "显示名称",
        "context window" => "上下文窗口",
        "max output tokens" => "最大输出 token",
        "route id" => "路由 ID",

        // ---- 状态 / 错误 / 日志中的可见文案 ----
        "search is disabled" => "搜索已禁用",
        "session search is disabled in this deployment" => "此部署已禁用会话搜索",
        "installed · restart the engine to apply" => "已安装 · 重启引擎后生效",
        "upload returned no receipt" => "上传未返回回执",
        "no language server for this file" => "此文件没有可用的语言服务器",
        "no symbols in this file" => "此文件中没有符号",
        "attachments are still uploading…" => "附件仍在上传…",
        "engine ready" => "引擎就绪",
        "Loading models…" => "正在加载模型…",
        "No models available" => "没有可用模型",
        "load more" => "加载更多",
        " · bundle" => " · 插件包",
        " · not a bundle" => " · 非插件包",
        "official · installed" => "官方 · 已安装",
        "not configured" => "未配置",
        "preset · not configured" => "预设 · 未配置",
        "Edit provider" => "编辑服务商",
        "Add provider" => "添加服务商",
        "Edit model" => "编辑模型",
        "New model" => "新建模型",
        "Settings panel" => "设置面板",
        "Composer" => "输入区",
        "Theme" => "主题",
        "Transcript" => "对话记录",
        "DeepSeek account limits" => "DeepSeek 账户限额",
        "Advanced model options" => "高级模型选项",
        "Subagent models" => "子智能体模型",
        "Other providers" => "其他服务商",
        "Agent loop" => "智能体循环",
        "Shell sandbox" => "终端沙箱",
        "Web tools" => "网页工具",
        "Web search" => "网页搜索",
        "DeepSeek web API" => "DeepSeek 网页 API",
        "Session log" => "会话日志",
        "Account onboarding" => "账户引导",
        "Permissions" => "权限",
        "OpenAI Completions" => "OpenAI Completions",
        "OpenAI Responses" => "OpenAI Responses",
        "Anthropic Messages" => "Anthropic Messages",
        "DeepSeek API" => "DeepSeek API",
        "models" => "个模型",
        "copy" => "复制",
        "ctx" => "上下文",
        "Value…" => "值…",
        "untracked" => "未跟踪",
        "modified" => "已修改",
        "up to date" => "已是最新",
        "{} is available (running {})" => "{} 可用（当前运行 {}）",
        "New panel in" => "新建面板于",
        "block(s)" => "个块",
        "tab" => "标签页",
        "exit" => "退出",
        "API key (env {})" => "API 密钥（环境变量 {}）",
        "update {} files" => "更新 {} 个文件",
        "just now" => "刚刚",
        "{} min ago" => "{} 分钟前",
        "{} h ago" => "{} 小时前",
        "{} d ago" => "{} 天前",
        "configured" => "已配置",
        "running" => "运行中",
        "error" => "错误",
        "done" => "完成",
        "outline" => "大纲",
        "context" => "上下文",
        "you" => "你",
        "[image]" => "[图片]",
        "multi-select" => "多选",
        "goal" => "目标",
        "default" => "默认",
        "select" => "选择",
        "run" => "运行",
        "kill" => "终止",
        "close" => "关闭",
        "history" => "历史",
        "edit" => "编辑",
        "delete" => "删除",
        "interrupt" => "中断",
        "chat" => "聊天",
        "terminal" => "终端",
        "Detached" => "已分离",
        "Choose the language for the interface and server-authored messages" => {
            "选择界面和服务器文案使用的语言"
        }
        "All" => "全部",
        "Commands" => "命令",
        "Panels" => "面板",
        "Spaces" => "空间",
        "Send" => "发送",
        "Files" => "文件",
        "panels" => "个面板",
        " · current" => " · 当前",
        "items" => "项",
        "fields" => "个字段",
        "turn ended" => "回合结束",
        "Usage" => "用量",
        "installed" => "已安装",
        "session/create failed" => "创建会话失败",
        "(this is not the web UI: the local dsh daemon HTTP API may be down or on another port; restarting omd relaunches it)" => {
            "（不是 web：这是本地 dsh daemon 的 HTTP API，daemon 可能挂/换端口了，重启 omd 会重新拉起）"
        }
        "The terminal needs a session first; that is the failure above" => {
            "终端需要先有会话；上面的失败原因就是它"
        }
        "Browser engine failed to start: CEF runtime not found" => {
            "浏览器引擎启动失败：没找到 CEF 运行时"
        }
        "Failed to create browser" => "浏览器创建失败",
        "thinking…" => "思考中…",
        "loading…" => "加载中…",
        "space" => "空间",
        "The engine decides each approval on its own; switch the permission preset to reduce them →" => {
            "引擎按单次审批裁决；要减少审批，切换权限预设 →"
        }
        "▾ Custom settings" => "▾ 自定义设置",
        "▸ Custom settings" => "▸ 自定义设置",
        "credentials · {}" => "凭证 · {}",
        "{} of {} plugins" => "{} / {} 个插件",
        "build scripts: {}" => "构建脚本：{}",
        "by {}" => "作者：{}",
        "{} plugin(s) in this index" => "此索引中有 {} 个插件",
        "Env var not detected. Enter an API key, or configure {} in your env." => {
            "未检测到环境变量。请输入 API 密钥，或在环境中配置 {}。"
        }
        "installing bundled dsh engine…" => "正在安装内置 dsh 引擎…",
        "downloading and installing dsh…" => "正在下载并安装 dsh…",
        "starting dsh…" => "正在启动 dsh…",
        "pick provider / model" => "选择服务商 / 模型",
        "Pick a provider from the built-in catalog and enter its API key." => {
            "从内置目录选择服务商并输入 API 密钥。"
        }
        "llm-deepseek · API key" => "DeepSeek · API 密钥",
        "installed (kept disabled)" => "已安装（保持禁用）",
        "plugin market is not enabled in this profile (install dsh-plugin-market)" => {
            "此配置未启用插件市场（请安装 dsh-plugin-market）"
        }
        "always: read-only" => "始终：只读",
        "always: workspace-write" => "始终：工作区可写",
        "always: danger-full-access" => "始终：完全访问",
        "(no snippet)" => "（无摘要）",
        "CEF 156 · Chromium 156 (windowless)" => "CEF 156 · Chromium 156（无窗口）",
        _ => return None,
    })
}

/// 翻译字面量；没有词条时原样返回。
pub fn tr(en: &'static str) -> &'static str {
    if is_zh() {
        lookup(en).unwrap_or(en)
    } else {
        en
    }
}

/// 翻译运行时字符串（`humanize_key` 之类生成的文案）。
pub fn trs(en: &str) -> String {
    if is_zh() {
        lookup(en).map(str::to_string).unwrap_or_else(|| en.to_string())
    } else {
        en.to_string()
    }
}

/// 带格式占位符的文案：先翻译模板，再按位置填入参数。
pub fn trf(template: &str, args: &[&str]) -> String {
    let mut out = if is_zh() {
        lookup(template).map(str::to_string).unwrap_or_else(|| template.to_string())
    } else {
        template.to_string()
    };
    for arg in args {
        if let Some(index) = out.find("{}") {
            out.replace_range(index..index + 2, arg);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_and_translates() {
        set_preference("en");
        assert_eq!(tr("Save"), "Save");
        set_preference("zh-CN");
        assert_eq!(tr("Save"), "保存");
        assert_eq!(trf("{} min ago", &["5"]), "5 分钟前");
        set_preference("fr");
        assert_eq!(tr("Save"), "Save");
    }
}
