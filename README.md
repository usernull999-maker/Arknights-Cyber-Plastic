# Arknights Super

面向《明日方舟》实战记录与对照的 Windows 桌面工具。应用使用
ArknightsCostBarRuler 提供画面捕获、战斗状态和费用条时间推理，在本项目中完成
输入归因、操作记录、OCR 名称识别、存储和 Tauri/React GUI。

## 功能

- 从 Windows 明日方舟客户端或 MuMu 捕获战斗画面。
- 以费用条时间和战斗状态关联键盘、鼠标操作，生成可编辑的操作记录。
- 将历史记录与当前战斗对照，并可在接近下一操作时提示。
- 在 GUI 中选择目标、进行校准、管理记录、配置快捷键和 OCR 名称区域。
- 后端状态变更即时推送；实时战斗时间线以 20Hz 推送给 GUI，不让 WebView 参与录制正确性。

## 要求

- Windows 10/11。
- Rust stable 工具链与 Node.js 22 或更高版本。
- 已安装并可运行的明日方舟 Windows 客户端，或可手动配置的 MuMu 实例。

MuMu 12 支持自动目标发现。MuMu 15 不承诺自动发现或额外 DLL 布局兼容；如上游
CostBarRuler 本身可连接，可通过手动配置继续尝试使用。

## 获取与运行

克隆时必须初始化子模块：

```powershell
git clone --recurse-submodules <your-repository-url>
cd arknights-super
```

已有工作树可执行：

```powershell
git submodule update --init --recursive
```

启动桌面开发环境：

```powershell
cd apps/desktop
npm ci
npm run tauri -- dev
```

构建前端和无安装包的 release 可执行文件：

```powershell
cd apps/desktop
npm run build
npm run tauri -- build --no-bundle
```

可执行文件输出到根目录的 `target/release/arknights-super-tauri.exe`。

可选的命令行入口：

```powershell
cargo run -p arknights-super-cli -- --help
cargo run -p arknights-super-cli --bin capture_debug -- --help
```

首次启动后，依次在设置页选择目标、保存配置并完成校准。校准或上游版本更新导致
校准文件不兼容时，应重新校准，不应继续使用旧 profile。

在设置页的「关卡地图」中手动选择「墨染书阁」或「JT8-3 昂首，足践烈焰」后再进入
战斗。JT8-3 使用普通关卡的 15×9 格子和独立镜头参数；目前不自动识别关卡，也不将
该选项标记为突袭模式。战斗进行中不能切换地图。新记录保存所选关卡 ID 和名称，旧
配置与旧记录仍按墨染书阁读取；不同地图的记录不会触发对照进度和操作提示。

命令行录制可指定 `--stage JT8-3`，省略时保持原有墨染书阁默认值。

技能键需要与模拟器的实际绑定一致。桌面端默认 `RBUTTON`（鼠标右键），也接受
`RMB`、`MOUSE_RIGHT` 或 `鼠标右键`；键盘键仍支持 `Q`、`F1` 等。若空格用于暂停，
请勿把技能键设为 `SPACE`。留空则关闭对应操作的快捷键记录。已有设置保留原绑定。

名称截图框的四个数值是从画面各边裁掉的比例。例如右侧裁掉 95% 会只保留左侧 5%，
导致姓名被截断。可点击「恢复默认截图框」，选中干员后通过实时预览确认完整姓名。
OCR 保留整个姓名宽度，不再重复缩窄截图；亮度计算使用 32 位整数，避免白字二值化
溢出；纯标点不会被当作干员名字。

## 目录

```text
apps/
  desktop/                 Tauri 壳与 React 前端
  cli/                     调试和捕获命令行工具
crates/
  app-contract/            后端与 WebView 的可序列化事件契约
  app-core/                录制、输入、OCR、存储和运行时逻辑
  ruler-bridge/            唯一直接依赖 ruler-core 的兼容边界
resources/ocr/             最小 OCR 资源包及来源记录
resources/maps/            带版本和来源信息的地图数据，编译时嵌入程序
third_party/cost-bar-ruler/ 官方 ArknightsCostBarRuler Git 子模块
```

`ruler-bridge` 是跟进上游变更的唯一入口。更新 CostBarRuler 时，先在干净子模块中
更新 SHA，运行回归和 Windows 冒烟测试，再提交主仓库的 gitlink 更新。

## 架构文档

[架构与核心运行时](docs/architecture.md) 说明了线程模型、录制归因、后端到 GUI 的
同步协议、上游升级边界，以及当前地图格子投影的范围与来源记录。

## 开发检查

```powershell
pwsh scripts/check-boundaries.ps1
cargo test --workspace
cd apps/desktop
npm run build
```

`scripts/check-boundaries.ps1` 确保 `ruler-core` 不会以 Cargo 依赖形式泄漏到
`ruler-bridge` 以外的 crate。GitHub Actions 会运行同一组检查。

## 第三方资源与许可

CostBarRuler 的源码和许可证位于其 Git 子模块中。OCR 资源来自
MaaAssistantArknights 提交 `6ceabc3cfc7143b53342337065f3d055ae78bc8d`，来源文件、
SHA-256 和 AGPL-3.0 文本记录在 `resources/ocr` 与
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。

在公开发布包含 OCR 资源的安装程序前，必须完成其再分发许可审查。
