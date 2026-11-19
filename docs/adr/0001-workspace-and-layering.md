# ADR 0001：工作区分层与依赖方向

## 状态（Status）

已接受（v1），2026-11-18。相关：[README](../../README.md)、[ADR 0002](0002-image-engine.md)。

## 背景（Context）

GImage 同时包含原生 UI、图像编解码、目录扫描、SQLite 索引、缓存与批处理。若 UI、数据库和编解码库互相直接依赖，后端替换（例如 libvips）会渗入业务层和界面代码，测试也无法在无 GUI 的情况下运行。

规格书要求「UI 线程不执行同步解码、扫描或 SQL 大查询」，并要求编解码绑定封装在 `image_engine` 内、不暴露给业务层。

## 决策（Decision）

Cargo workspace 由以下成员组成：

| 成员 | 职责 | 直接依赖 |
| --- | --- | --- |
| `app`（二进制 `gimage`） | 启动、依赖注入、Slint 命令/事件接线 | 5 个 workspace crate + `slint`、`rfd`、`crossbeam-channel`、`tracing`、`tracing-subscriber`、`anyhow` |
| `ui/`（非 crate） | `*.slint` 视图与主题，由 `app/build.rs` 编译进 app | Slint 标准库 |
| `crates/core`（`gimage-core`） | 领域类型、`ImageEngine` trait、编辑参数、导出选项、设置、路径、取消令牌 | `serde`、`serde_json`、`thiserror` |
| `crates/engine`（`gimage-engine`） | 编解码实现与 `ImageEngine` 适配器 | core、`image`、`kamadak-exif`、`tracing`、`thiserror` |
| `crates/catalog`（`gimage-catalog`） | 文件夹扫描与 SQLite 索引 | core、`rusqlite`、`walkdir`、`tracing`、`thiserror` |
| `crates/cache`（`gimage-cache`） | 内存 LRU + 磁盘缩略图缓存 | core、`tracing` |
| `crates/task_queue`（`gimage-task-queue`） | 有界批处理池、进度事件、取消 | core |

依赖方向固定为：

```text
app ──> ui (Slint, build-time)
 │
 ├──> engine ──┐
 ├──> catalog ─┤
 ├──> cache ───┼──> core
 └──> task_queue ┘
```

- `core` 不依赖任何后端、数据库、Slint 或文件系统实现之外的库；它只定义 trait、值类型和纯逻辑。
- 叶子组件在工作区内只依赖 `core`，彼此不直接依赖。`app` 负责把它们组合起来。
- Slint 类型不进入 `core`；`core` 不出现 `image::DynamicImage`、`rusqlite` 或 libvips 类型。
- **UI 不解码图片。** 源图读取、EXIF 方向归一、缩略图、渲染与编码一律经过 `gimage_core::ImageEngine`。UI 只接收已准备好的 `RgbaImage` 或由它转换出的 Slint 图像句柄；不得用 Slint 直接从源文件路径加载像素绕过引擎。
- 后台线程把结果以小型事件或图像句柄交回 UI 线程；UI 线程不调用 `probe` / `thumbnail` / `render` / `encode` 的阻塞路径。

## 后果（Consequences）

- 替换后端只需新增 `ImageEngine` 实现并在 `app` 的选择点注入，业务层与 UI 无需改动；后端差异通过 `CodecCapabilities` 暴露。
- `core` 的测试不依赖 GUI、数据库或编解码器，可在 CI 中快速运行。
- 代价是边界处存在一次像素缓冲区拷贝或句柄封装；对缩略图和预览可接受，导出路径应复用同一份 `RgbaImage` 避免重复解码。
- 依赖方向靠 Cargo 依赖而非编译器强制；评审时必须检查是否有人在 `core` 里 `use image::...` 或让 UI 直接读文件。
- `ui/` 不是 crate，无法单独测试；Slint 文件只能通过 `app` 的构建脚本编译验证。