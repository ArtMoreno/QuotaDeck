# QuotaDeck

为 Herdr 提供按账户隔离的模型、上下文、缓存与额度信息。稳定插件 ID 仍为
`herdr-agent-quota-win`，因此已有安装可以原位升级。

**Windows 分支由 [Art Moreno](https://github.com/ArtMoreno) 创建并维护。**

> 本项目基于 [levi-qiao/herdr-agent-quota](https://github.com/levi-qiao/herdr-agent-quota)
>（MIT，© 2026 Levi Qiao），新增 Windows 支持、Hermes 与 OpenRouter 额度、
> 品牌标记和可配置 dashboard。详见
> [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md)。

[![CI](https://github.com/ArtMoreno/quota-deck/actions/workflows/ci.yml/badge.svg)](https://github.com/ArtMoreno/quota-deck/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

## 安装

需要 Herdr 0.8.2+、Git、Rust 1.95+、Windows/macOS/Linux，以及至少一个受支持的
Agent CLI。

直接使用现有 Agent 登录，无需额外的 QuotaDeck 账户。首次编译需要 Rust 链接器
（Windows 使用 MSVC C++ Build Tools），可能耗时数分钟。更新 Windows checkout 前，
请关闭 QuotaDeck dashboard 和 Settings pane，以便替换可执行文件。
未安装图标字体时，可在 Settings 中将 Brand glyphs 改为 `unicode`。

```sh
herdr plugin install ArtMoreno/quota-deck
herdr plugin action invoke configure --plugin herdr-agent-quota-win
herdr plugin log list --plugin herdr-agent-quota-win --limit 5
```

最后一条命令应显示最新任务的 `"status":"succeeded"`。随后重启已经运行的
Agent pane。也可以从本地 checkout 安装：

```sh
./install.sh                  # macOS、Linux
```

```powershell
.\install.ps1                 # Windows
```

## Dashboard

按 `prefix+shift+d` 可直接打开 split pane，不显示命令输出。下方 CLI 命令返回的
JSON 是 Herdr 的任务回执。若在 Herdr 外操作命名 session，请在 `herdr` 后添加
`--session <name>`。

```sh
herdr plugin action invoke open-dashboard --plugin herdr-agent-quota-win
herdr plugin action invoke open-dashboard-split --plugin herdr-agent-quota-win
```

Dashboard 可作为 popup 或真正可调整大小的 split pane 打开。列表过高时可使用
鼠标滚轮、↑/↓、PageUp/PageDown、Home/End；按 `r` 刷新，按 `q` 或 Esc 关闭。
刷新在后台排队执行，不会在 UI 线程读取 OpenCode 数据库。

## 设置

按 `prefix+shift+q`，或运行：

```sh
herdr plugin action invoke open-settings --plugin herdr-agent-quota-win
```

| 控件 | 可选值 | 作用 |
| --- | --- | --- |
| Percentages | `remaining`、`used` | 显示剩余或已用比例；颜色始终表示剩余额度。 |
| Sidebar layout | `packed`、`stacked` | 相关字段同行显示，或每项独占一行。 |
| Row gap | `0`、`1` | 控制 Agent 卡片间距。 |
| Watch interval | 30 秒–1 小时 | Claude、Codex、Grok、Agy、Hermes 工作时轮询；Pi/OMP 由自身事件或焦点刷新。 |
| Brand colors | `on`、`off` | 控制供应商/模型品牌色；额度严重性颜色不变。 |
| Brand glyphs | `icon`、`unicode`、`off` | 使用品牌图标、通用字符或纯名称。 |
| Agent order | `default`、`quota` | 可将剩余额度最少的 Agent 排在最前。 |
| Low quota alert | `off`、5–50% | 每个供应商首次跌破阈值时提醒一次。 |
| Fields | topic、model、cache、TTL、context、短/长额度 | 控制侧栏字段；来自 prompt 的 topic 默认关闭。 |
| Dashboard providers | 显示、顺序、`#RRGGBB`、字段 | 逐个控制 dashboard；仅插件管理的侧栏行复用颜色。Pi 使用 Codex 色，OpenCode 使用 OpenCode Go 色；用户自定义行不改色。 |
| Agents | claude、codex、grok、agy、opencode、pi、omp、hermes | 安装或移除 collector 与侧栏行。 |

使用 ↑/↓ 移动，←/→ 或 Space 修改，`a` 应用，`q` 返回。Dashboard providers
页面中使用 `u`/`d` 排序、`c` 输入颜色、Enter 选择该供应商的字段。`*` 表示尚未应用。

## 数据含义

| Agent / 供应商 | 额度 | Session 信息 |
| --- | --- | --- |
| Claude Code | 5h + 7d | model、context、cache、记录的 cache 过期时间 |
| OpenAI Codex | 5h + 7d | model、context、cache、约 30 分钟 cache TTL |
| Grok CLI | 7d 或 30d | model、context、cache |
| Agy / Antigravity | 5h + 7d | statusLine model、context、cache |
| OpenCode | 本地 30d token 与已记录花费 | 精确本地 session 的 model/context |
| OpenCode Go | 订阅额度（可用时，默认隐藏，可在设置中开启） | dashboard 独立行 |
| Pi | 账户精确匹配时复用 Codex 额度 | model、context、cache |
| omp（oh-my-pi） | OMP 返回的 `5h`、`1d`、`7d`、`Monthly` 等窗口 | model、context、cache |
| Hermes | Nous Portal 的 plan 与可验证 top-up 美元 | model、context、cache |
| OpenRouter | 账户余额，或 API key 自身的花费上限 | 仅 dashboard |

OpenCode 本地行以只读方式统计最近 30 天 token 与已记录花费；美元含义是“已花费”，
不是“剩余额度”。Hermes plan 显示真实续期时间；top-up 和 OpenRouter 余额没有固定
重置时间，因此不会显示虚构倒计时。

要显示 OpenCode Go 额度，请在 OpenCode 本身登录 Go；QuotaDeck 没有单独的登录：

```sh
opencode auth login      # 选择 OpenCode Go 并粘贴 Go key
opencode auth list       # 确认列表中有 OpenCode Go
```

QuotaDeck 使用与 OpenCode 相同的 Go 登录。OpenCode 1.x 与 2.x 均支持：OpenCode 2
把登录和 session 存在 `opencode.db` 中，QuotaDeck 以只读方式读取；旧版本则读取
`auth.json`。若在 Herdr 启动环境中设置了 `OPENCODE_API_KEY`，则优先使用它。没有 Go
登录时，Go 行显示 `credentials unavailable`。侧栏只在 session 实际使用
`opencode-go` 模型的 pane 上显示 Go 额度；Ollama 等本地供应商没有额度，只显示
model 和 context。

Topic 默认关闭。开启后，截短的可见 prompt 只写入本机 Herdr metadata，TTL
为 24 小时。通过 Windows 安装器显式传入的 OpenRouter key 会保存在当前用户的
插件配置目录中，完整卸载时删除；不会写入额度 cache 或日志。

## 品牌标记

Herdr 侧栏是文本网格，因此侧栏 logo 来自字体 glyph：`icon` 使用
`Herdr Agent Icons Max`，`unicode` 使用普通等宽字体可显示的符号，`off` 只显示名称。
[`docs/icons/`](docs/icons) 中的 SVG 与透明 64px PNG 仅用于文档和支持图片的界面。

## 安全与隐私

- 仅向各供应商自己的用量端点发送经过身份验证的请求；不会上传用量数据。
- 不刷新、轮换或写入供应商凭据，也不读取浏览器 cookie 或系统 keychain。
- Topic 默认关闭时，事件不会读取 pane 文本；开启后，每个事件只读取自身命名的
  pane，refresh 和 watch 始终不读取 pane。
- 除用户通过 `install.ps1 -OpenRouterKey` 明确保存到插件配置的 key 外，其余凭据
  只在单次请求期间保留在内存中；所有凭据都不写入 cache 或日志。
- 持久化账户与 session 标识使用按域隔离的 SHA-256 标签。
- prompt 文本、session 摘要和完整供应商 payload 不会持久化；旧 cache 会在迁移时清理。
- 配置写入可逆。未被用户修改的 Herdr 配置可逐字节恢复；卸载会保留后续用户编辑。

完整说明与排错步骤见 [英文 README](README.md)、[SECURITY.md](SECURITY.md) 和
[CHANGELOG.md](CHANGELOG.md)。

## 许可证

MIT。本项目与 Herdr、OpenAI、Anthropic、xAI、Google、OpenCode、Nous Research
或 OpenRouter 无隶属关系。
