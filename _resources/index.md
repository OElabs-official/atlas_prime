# _resources 资源索引

> 规范 _std_general.md 0.2.3 §工程文档结构：工程手动/自动引入的资源，二级分类 + 本索引。
> 本目录内资产全部被 `include_str!` / `include_bytes!` 编译期内嵌进二进制；
> 修改任何文件都需重编生效（dashboard 另有数据目录热覆盖路径，见下）。

| 分类 | 文件 | 作用 | 内嵌点 |
| --- | --- | --- | --- |
| `dashboard/` | `dashboard.html` | 面板前端，单文件自包含（~166 KB） | `api/mod.rs` → `DASHBOARD_HTML`（`GET /dashboard.html`） |
| `vendor/` | `d3.min.js` | D3 v7.9.2（280,622 B，ISC，Copyright 2010-2026 Mike Bostock，d3js.org；来源 `~/_ai_ws_v2/_html/resources/vendor/d3/`）——面板折线/柱状 SVG 渲染层 | `api/tui.rs` → `GET /vendor/d3.min.js`；消费方 = `dashboard.html`（drawLines/柱状机件） |
| `vendor/` | `xterm.js` / `xterm.css` | 浏览器-TUI 终端仿真器 | `api/tui.rs` → `GET /vendor/{name}` |
| `vendor/` | `addon-fit.js` | xterm 自适应尺寸插件 | 同上 |
| `vendor/` | `addon-webgl.js` | xterm WebGL 渲染插件 | 同上 |
| `vendor/` | `addon-unicode-graphemes.js` | xterm Unicode 字素簇支持 | 同上 |
| `sv/` | `atlas_prime/run` | runit 主服务脚本（`--sv-install` 落盘 0700） | `service.rs` → `RUN_SCRIPT` |
| `sv/` | `atlas_prime/log/run` | runit 日志服务脚本（落盘 0700） | `service.rs` → `LOG_RUN_SCRIPT` |
| `systemd/` | `atlas-prime.service` | systemd unit 模板（非 Termux 环境参考） | 不内嵌，随仓库分发 |

约定：

- `dashboard/dashboard.html` 若被拷贝到数据目录
  `~/.local/share/atlas_prime/dashboard.html`，则每次请求覆盖内嵌版（开发热替换）。
- 新增资产须同步更新本索引与对应 `include_*!` 白名单。
