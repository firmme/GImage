# GImage · 原生轻量图片管理器

Windows x64 便携版可从 [GitHub Releases](https://github.com/firmme/GImage/releases/latest) 下载。解压 ZIP 后运行 `gimage.exe`。在「设置 → 关于」可检查更新。确认后，GImage 会下载对应版本的可执行文件，验证 SHA-256，然后替换并重启。

发布包在本地构建。每个正式版本上传 ZIP 和同版本的 `GImage-<version>-win64.exe`，后者供应用内更新使用。

GImage 是基于 **Rust + Slint** 的本地桌面图片工具，支持浏览、预览、标注和批量转换。编辑以草稿开始，确认保存时才覆盖原图。

当前版本已具备浏览、预览、非破坏性编辑、导出与批处理的代码和 UI 接线，正在进行稳定性完善。下面区分已存在的实现和仍待验收的能力。

## 当前状态（2026-11-18）

| 功能 | 实现状态 |
| --- | --- |
| 原生桌面窗口 | 无边框 Slint 窗口、应用图标、自定义窗口控件、浏览 / 预览 / 编辑 / 任务 / 设置 |
| 目录浏览 | 后台递归扫描、目录变更监听、SQLite 索引、分页、排序、搜索、格式及收藏筛选 |
| 缩略图 | 后台生成、内存 LRU + 磁盘缓存、网格按行请求 |
| 预览与编辑 | 原生全屏、可输入的缩放百分比、EXIF、旋转 / 翻转 / 裁剪 / 调色、标注、草稿、撤销重做 |
| 右键与剪贴板 | 文件、预览、最近文件夹、输入框和图片信息支持右键操作。可复制文件、图片、文件名及普通 Windows 路径 |
| 图像后端 | 纯 Rust image 后端，JPEG / PNG / WebP / GIF / BMP / TIFF / ICO；实际读写能力由运行时探测决定，动画仅首帧 |
| 导出 | 格式转换、尺寸限制、冲突策略、原子写入、原图覆盖保护 |
| 批处理 | 导出、重命名、有界工作池、进度、取消、失败重试、重启后显示中断任务 |
| 设置 | 主题、扫描选项、并发数、缓存配置及清理 |
| 目录同步安全 | 不完整扫描保留未发现的索引；过期扫描不能更新当前目录计数 |

上述为源码和接线状态，不代表所有 UI、格式和性能验收已经完成。此次执行 `cargo test --workspace --offline`，**76 项单元测试通过**，其中包含 4 项新增回归测试。记录见 [测试日志](docs/test-results-2026-11-18.log) 和 [开发进度](docs/progress.md)。

仍未实现：批量回收站删除、标签、GPU 画布、RAW / PSD、动画播放。万图滚动、内存峰值、DPI 与完整 UI 交互仍需按 [测试计划](docs/test-plan.md) 验收；默认离线，不计划云同步。

## 本轮修复

- 扫描取消、达到数量上限或发生 IO 错误时，仅更新已发现的文件，不把其余索引误标为丢失；完整扫描仍能识别真实删除。
- 状态栏显示数量上限和读取失败；扫描末次进度回调发出的取消也会被保留。
- 快速切换目录时，旧扫描的计数及分页事件不能改写新会话状态。
- 开发和打包脚本自动配置当前进程的 MinGW PATH；支持 `GIMAGE_MINGW_BIN` 自定义位置。

## 技术栈与关键决策

| 主题 | 结论 | 记录 |
| --- | --- | --- |
| UI | Slint + Rust，原生窗口，无 WebView | [ADR 0001](docs/adr/0001-workspace-and-layering.md) |
| 图像后端 | v1 用纯 Rust `image` crate；libvips 保留为计划中的可替换后端 | [ADR 0002](docs/adr/0002-image-engine.md) |
| 工具链 | `stable-x86_64-pc-windows-gnu`（本机无 MSVC 链接器） | [ADR 0003](docs/adr/0003-toolchain-windows-gnu.md) |
| 缩略图缓存 | 路径哈希 + 文件大小 + mtime + 尺寸档 + 版本；临时文件 + 原子替换 | [ADR 0004](docs/adr/0004-thumbnail-cache.md) |
| 批处理安全 | 有界队列、协作式取消、原子导出、默认自动重命名且不改原图 | [ADR 0005](docs/adr/0005-batch-safety.md) |

规格书见 [docs/spec.md](docs/spec.md)（由 `scripts/extract_spec.py` 从 `Image_Toolbox_开发规格书_v1.0.docx` 自动转换，8 个表格完整保留）。

## 快速开始

### 前置条件

- Windows 10/11 x64。
- Rust stable（仓库 `rust-toolchain.toml` 固定 `stable-x86_64-pc-windows-gnu`，rustup 会自动安装对应工具链）。
- MinGW-w64（需要 `gcc.exe`、`dlltool.exe`、`ar.exe` 和用于嵌入 Windows 图标的 `windres.exe`）。本机已装在 `E:\tools\mingw64\bin`，其他机器可设置 `GIMAGE_MINGW_BIN`。
- 首次构建需要能获取 crates.io 依赖；之后可离线构建（依赖已在本地 Cargo 缓存时）。

### 构建、测试、运行

```powershell
# 配置当前 PowerShell 的 GNU 工具路径（不修改系统环境变量）
. ./scripts/setup-toolchain.ps1

# 构建应用（二进制名 gimage）
cargo build -p gimage-app

# 运行整个 workspace 的单元测试
cargo test --workspace

# 运行调试版二进制
E:/RustBuild/gimage/debug/gimage.exe
```

在图库右击图片可复制、预览、编辑、导出或打开文件位置。预览页底栏显示序号、文件大小和像素尺寸。缩放框支持下拉选项、滚轮以 5 个百分点为步长调节和键入百分比。按 `F11` 进入全屏，移动鼠标唤出工具栏，按 `Esc` 退出。点击左侧圆形感叹号显示或关闭 EXIF。

标注工具支持自定义颜色和粗细，直线与箭头可用实线、虚线、点线和波浪线。矩形、椭圆、三角形、五边形和六边形支持拖拽绘制，按住 `Shift` 可约束为正形。自由多边形单击加点，按回车或右击闭合。文字先输入再点击放置，橡皮按整条标注擦除。

编辑和撤销记录保留在本次运行的内存草稿中，离开编辑页后可继续。在编辑页点击「保存」或按 `Ctrl+S`，确认后原子替换图片。重新编码可能移除元数据。也可使用「导出副本」保存草稿，动画和多页图片通过此方式输出当前帧。

构建产物不在仓库内：`.cargo/config.toml` 把 target 目录固定到 `E:/RustBuild/gimage`。release 产物位于 `E:/RustBuild/gimage/release/gimage.exe`。

### 开发脚本

```powershell
# 构建并启动调试版（-Release 切换 release）
pwsh -ExecutionPolicy Bypass -File scripts/run-dev.ps1

# release 构建 + 打包为 dist/GImage-<version>-win64.zip（默认离线）
pwsh -ExecutionPolicy Bypass -File scripts/package.ps1
```

使用 Visual Studio 2022 或更新版本的 C++ Build Tools 和 Windows MSVC Rust 工具链时，可加 `-Skia` 构建并启动 Windows 高 DPI 版。发布脚本同样支持 `-Skia`。

`scripts/package.ps1` 会从 `Cargo.toml` 的 `[workspace.package].version` 读取版本号，构建失败或找不到 `gimage.exe` 时直接报错退出。

## 数据与缓存位置

以下路径由 `crates/core/src/paths.rs` 定义，应用启动时初始化相关目录。

| 内容 | 路径 |
| --- | --- |
| 设置 | `%APPDATA%\GImage\settings.json` |
| 目录索引数据库 | `%APPDATA%\GImage\catalog.sqlite3` |
| 批处理恢复记录 | `%APPDATA%\GImage\task-recovery\` |
| 缩略图磁盘缓存 | `%LOCALAPPDATA%\GImage\thumbs\`（`.gthm` 文件） |
| 日志 | `%LOCALAPPDATA%\GImage\logs\` |

- 删除 `settings.json` 会恢复默认设置；损坏的设置文件不会阻塞启动，而是回退默认值并给出警告。
- 删除 `thumbs\` 可强制重建缩略图；缓存写入失败只会记录日志，不影响浏览。
- 数据库存放路径、尺寸、修改时间和旧版本的编辑会话。新的编辑草稿只存在本次运行的内存中。

## 项目结构

```text
GImage/
├─ app/                    # 可执行程序、Slint 构建脚本、依赖注入与命令接线
├─ ui/                     # *.slint：主窗口、主题令牌与各页面
├─ crates/
│  ├─ core/                # 领域类型、ImageEngine trait、编辑参数、设置、路径
│  ├─ engine/              # 纯 Rust 编解码后端（libvips 适配器预留）
│  ├─ catalog/             # 文件夹扫描 + SQLite 索引
│  ├─ cache/               # 内存 LRU + 磁盘缩略图缓存
│  └─ task_queue/          # 有界批处理池 + 协作式取消
├─ docs/
│  ├─ spec.md              # 规格书 Markdown 版（自动转换）
│  ├─ test-plan.md         # 可勾选的测试矩阵
│  └─ adr/                 # 架构决策记录 0001–0005
├─ scripts/
│  ├─ extract_spec.py      # 从 .docx 重新生成 docs/spec.md
│  ├─ run-dev.ps1          # 构建并启动
│  └─ package.ps1          # release 构建 + 打包
├─ Cargo.toml              # workspace 与 profile
├─ rust-toolchain.toml     # stable-x86_64-pc-windows-gnu
└─ .cargo/config.toml      # target-dir = E:/RustBuild/gimage
```

## 开发约定

1. **不引入 WebView / Electron / CEF。** UI 使用 Slint 原生窗口；默认不联网。
2. **UI 线程只做轻量状态更新。** 扫描、解码、缩略图、编码、SQL 大查询全部在后台；回调 UI 只传小型事件、路径或已解码的图像句柄。
3. **UI 不解码图片。** 所有像素操作经 `gimage_core::ImageEngine`；UI 只消费 `RgbaImage` / Slint 图像句柄。新增后端时不要改业务层类型。
4. **写文件必须原子。** 先写同目录临时文件，`flush` / `sync_all` 后 `rename`；崩溃不能留下可见的半成品。
5. **草稿与保存。** 编辑期间原图只读。可以导出副本，或显式确认后覆盖原文件。
6. **有界并发 + 协作式取消。** 批量任务通过有界队列运行；取消只停止领取新任务，正在编码的项完成并清理临时文件，不强制杀线程。
7. **能力运行时探测。** 不硬编码“支持全部格式”；`CodecCapabilities` 决定 UI 展示哪些导出选项。
8. **路径用 `PathBuf` / `OsString`。** 不要在全局提前转 `String` 后再做 IO；用户可见错误用中文。
9. **测试与证据。** 新功能至少配一个成功用例和一个失败/取消用例；不得把未执行的测试写成已通过。
10. **构建目录不提交。** target 在 `E:/RustBuild/gimage`，`dist/` 为打包产物，均已忽略。

## 文档

- [产品规格书（Markdown）](docs/spec.md)
- [测试计划与验收矩阵](docs/test-plan.md)
- ADR：
  - [0001 工作区与分层](docs/adr/0001-workspace-and-layering.md)
  - [0002 图像引擎：纯 Rust 后端与 libvips 迁移路径](docs/adr/0002-image-engine.md)
  - [0003 Windows GNU 工具链](docs/adr/0003-toolchain-windows-gnu.md)
  - [0004 缩略图缓存](docs/adr/0004-thumbnail-cache.md)
  - [0005 批处理与文件安全](docs/adr/0005-batch-safety.md)

## 许可证

GImage 源码使用 [MIT 许可证](LICENSE)。应用的「设置 → 关于」包含 Slint 署名。Windows 包附带 [第三方组件与许可证](THIRD_PARTY_LICENSES.md)。
