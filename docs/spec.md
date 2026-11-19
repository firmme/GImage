> 本文件由 `scripts/extract_spec.py` 从 `Image_Toolbox_开发规格书_v1.0.docx` 自动转换，保留原文顺序与全部表格，未增删规格内容。

# IMAGE TOOLBOX

*原生轻量图片工具箱 · 可执行开发规格书*

技术基线：Rust + Slint + libvips  | 目标平台：Windows 优先，架构预留 macOS / Linux  | 版本：v1.0  | 2026-11-18

文档定位：给开发者或 AI 编码代理逐任务实施的工程蓝图；性能数字为目标而非现成实测结果。

## 项目原则

- 原生窗口与渲染，禁止 WebView / Electron / CEF；默认不接入联网服务。
- 启动快、交互不卡顿、内存有上限；优先交付稳定的基础功能，而非一次性实现全部专业编辑能力。
- 原图默认不修改；编辑参数非破坏性保存；危险文件操作必须有确认或可恢复机制。
- 基础预览先走 Slint Image；独立 GPU Canvas 在性能验证后再集成，避免过早复杂化。
- AI 开发每个迭代都应能构建、运行、测试和回退；不允许只提交无法运行的代码。

## 0. 范围与优先级

<!-- table 1 -->

| 优先级 | 必须实现 | 明确不在首版 |
| --- | --- | --- |
| P0 / MVP | 目录浏览、缩略图、单图预览、缩放平移、排序筛选、旋转裁剪、转换、批量重命名、任务进度 | RAW 全流程、PSD 图层、AI 抠图、云同步 |
| P1 | 非破坏性基础调色、批量尺寸和质量设置、元数据、收藏、缓存清理 | 复杂蒙版、图层编辑 |
| P2 | GPU 实时调色、曲线、LUT、颜色管理、RAW/PSD 扩展 | 视频编辑 |

## 1. 技术选型与边界

<!-- table 2 -->

| 子系统 | 首选 | 约束 / 备注 |
| --- | --- | --- |
| 桌面 UI | Slint + Rust | 原生 UI，无 WebView；先固定一个稳定版本 |
| 图像处理 | libvips + 经验证 Rust 绑定 | 将绑定封装在 image_engine，避免直接暴露给业务层 |
| 渲染 | 第一阶段 Slint 内置 Image；第二阶段 WGPU 画布 | GPU 后端与 wgpu 版本必须按 Slint 实际 API 匹配 |
| 数据 | SQLite（rusqlite） | 只存索引、标签、编辑参数和任务；不存原图字节 |
| 目录监听 | notify | 合并高频事件、防止自写文件触发重复扫描 |
| 并发 | 后台线程池 + 有界任务队列 | UI 线程不执行同步解码、扫描或 SQL 大查询 |
| 日志 | tracing | 默认本地日志、敏感路径脱敏选项 |
| 打包 | 平台安装包 + 原生 libvips 运行时 | 验证 DLL/so/dylib、编解码器及许可合规 |

决策记录：libvips Rust 绑定有多个独立项目（如 vips、rs-vips、libvips）；不要将 crate 名称视为可互换。选定版本后，用最小 demo 验证初始化、加载 JPEG、缩略图、导出 WebP、资源释放与跨平台打包，再锁定 Cargo.lock。

## 2. 系统架构

```
Slint UI (folder tree / image grid / editor / tasks)
        | commands / state / event notifications
App Core (Rust)
  |- FileCatalog / FolderScanner / Watcher
  |- ThumbnailService / PreviewService / CacheManager
  |- EditSession / ExportPipeline
  |- BatchTaskScheduler / Cancellation
  |- MetadataRepository (SQLite)
        | adapters
ImageEngine (libvips)   GPUCanvas (P2 / wgpu)   OS File APIs
```

线程原则：UI 线程只做轻量状态更新；扫描、解码、缩略图、转换在后台；回调主线程时仅传递小型事件、路径或已准备好的图像句柄。GPU 资源只在渲染上下文允许的线程创建与销毁。

## 3. 项目目录结构

```
image-toolbox/
  Cargo.toml                 # workspace
  Cargo.lock
  .cargo/config.toml
  app/                       # 可执行程序、启动、依赖注入
  ui/                        # *.slint，控件、主题、视图
  crates/
    core/                    # 业务服务、类型、命令
    image_engine/            # libvips adapter / codec capability
    catalog/                 # 文件扫描、SQLite
    task_queue/              # 并发、进度、取消
    cache/                   # 缩略图缓存 / LRU
    gpu_canvas/              # P2，独立可选 crate
  tests/fixtures/            # 小图、大图、损坏图、EXIF 样本
  docs/                      # ADR、性能基线、交互稿
  scripts/                   # benchmark / packaging / clean
```

## 4. UI 与交互规格

<!-- table 3 -->

| 页面 | 主要组件 | 关键行为 |
| --- | --- | --- |
| 浏览器 | 左侧目录树 + 中间网格 + 右侧信息栏 | 单击选中，双击预览，多选、拖拽、滚动惰性加载 |
| 预览页 | 中央画布 + 顶栏 + 底部胶片带 | 滚轮缩放（围绕光标）、拖动平移、适应窗口、100% |
| 编辑页 | 画布 + 右侧调色滑块 + 撤销重做 | 改参数即预览、原图保护、导出副本 |
| 批量任务页 | 任务列表、进度条、失败项 | 可取消，失败项重试，输出冲突处理 |
| 设置页 | 主题、缓存上限、并发量、解码限制 | 恢复默认值、清理缓存和日志 |

视觉规范：深色默认、浅色可选；统一 8 px 网格、按钮高度 32–36 px、面板最小宽度；键盘友好，支持 125%–200% DPI；低性能模式可关闭过渡动画。

## 5. 图片格式策略

<!-- table 4 -->

| 类别 | 首版支持策略 | 注意事项 |
| --- | --- | --- |
| JPEG / PNG / WebP / BMP / TIFF | 优先支持读取；JPEG / PNG / WebP 可导出 | 输出格式能力应运行时探测 |
| GIF / SVG / AVIF / HEIC | 能解码则显示；动画 GIF 第一版可先显示首帧 | SVG 安全加载；HEIC/AVIF 依赖编译时编解码器 |
| RAW / PSD | 第二阶段插件化支持 | RAW 预览与 RAW 全质量处理是不同能力 |
| 错误输入 | 显示友好错误与占位图 | 限制尺寸、内存与读取时间；防止解压炸弹 |

实现 CodecCapabilities：启动时检测实际可读/可写格式，UI 只展示具备能力的导出选项，不硬编码“支持全部格式”。

## 6. 数据模型和数据库

```
images(id, canonical_path UNIQUE, file_size, mtime_ns,
       width, height, format, orientation, checksum_nullable,
       indexed_at, missing_flag)
folders(id, canonical_path UNIQUE, last_scan_at)
tags(id, name UNIQUE)
image_tags(image_id, tag_id, PRIMARY KEY(image_id, tag_id))
edit_sessions(image_id PRIMARY KEY, params_json, version, updated_at)
tasks(id, kind, status, total, done, failed, options_json, created_at)
```

索引：images(canonical_path)、images(mtime_ns)、images(format)、tasks(status)。建议开启 WAL；schema 迁移必须可重复执行并提供备份测试。文件路径可能包含非 UTF-8 字节，内部使用 OS 原生路径类型，显示层转换为可读文本，不能简单全局转 String 后再用于 IO。

## 7. 缩略图与预览缓存

1. 目录扫描阶段只读取文件元信息，不同步解码所有图。
2. 可见窗口需要的缩略图优先入队；下一屏低优先级预取；离屏任务允许取消。
3. 建议缩略图尺寸档位 160 / 320 / 640 px；缓存键至少包括规范化路径标识、文件大小、修改时间、尺寸档、处理版本。
4. 先读磁盘缓存，再使用受限内存 LRU；缓存文件采用临时文件写入后原子替换。
5. 预览优先屏幕分辨率图；放大到一定比例后再请求更高分辨率或分块解码。

首版建议：内存缩略图缓存软上限 64 MB、磁盘缓存上限 2 GB（可设置）。这些是起始策略，随后通过压测修正；不能假定任意机器都适用。

## 8. 编辑与导出流水线

```
Source file (read-only)
 -> Orientation normalize
 -> Crop / rotate / flip
 -> Exposure / contrast / saturation / temperature
 -> Resize (optional)
 -> Color profile transform (if enabled)
 -> Encode + metadata policy
 -> Write temporary output
 -> Flush / close / atomic rename
```

“编辑会话”使用 JSON 参数+版本号保存；撤销/重做维护命令栈而非复制整张位图。GPU 预览（P2）与 CPU 导出必须采用统一的色彩计算定义；建立 golden image 测试，避免预览和导出差异过大。

<!-- table 5 -->

| 接口（示例语义） | 输入 | 输出 |
| --- | --- | --- |
| scan_folder | folder, recursive, filters | scan_id，分页图像元数据 |
| request_thumbnails | image_ids, size, priority | ready/error 事件 |
| open_preview | image_id, viewport_size | preview handle / size / orientation |
| update_edit_params | image_id, parameter_delta | session_version，预览更新 |
| submit_export | ids, format, quality, output_policy | task_id |
| cancel_task | task_id | 取消确认或最终状态 |

## 9. 批处理与文件安全

- 有界队列：普通机器先从 2 个重型图片任务并行开始，观察 CPU/RSS 后调整；避免工作队列并发 × libvips 内部并发过高。
- 任务状态：queued → running → completed / partial_failed / failed / canceled；任务项记录可重试错误。
- 处理输出先写临时文件，再原子替换/移动（尽可能同一文件系统）；禁止在完成前删原件。
- 覆盖冲突策略：跳过 / 自动重命名 / 覆盖前确认；默认“自动重命名 + 保留原件”。
- 批量删除先走系统回收站（受平台限制）；永久删除必须二次确认。
- 终止任务时合作式取消：停止领取新任务，正在编码的操作结束后清理临时文件。

## 10. 性能验收与测量方法

<!-- table 6 -->

| 指标 | 目标（不是承诺） | 测试条件 |
| --- | --- | --- |
| 冷启动至可交互 | SSD 设备目标 ≤ 1.5s | Windows 普通开发机；连续测 10 次，记录 P50/P95 |
| 已缓存图片切换 | 预览首次可见 ≤ 150ms（P95） | 1920×1080 JPEG，SSD，缓存命中 |
| 网格滚动 | 目标接近 60 FPS | 目录含 1 万条索引，连续快速滚动 60s |
| 内存常驻 | 轻度浏览目标 ≤ 250MB | 打开 1 千张图目录，滚动 5min 后静置 |
| 批量转换 | 持续处理、不冻结 UI | 100 张混合规格，记录速度/失败/峰值 RSS |
| GPU 调色（P2） | 1080p 预览目标 60 FPS | 兼容 GPU，连续拖动滑块 60s |

测试时记录硬件、OS、分辨率、图片样本、缓存冷热状态、软件 commit、libvips 版本。资源统计区分 RSS、GPU VRAM、磁盘缓存、target 构建目录。通过标准以 P95、峰值和长期稳定性为主，不仅看一次最快结果。

## 11. Rust 构建体积控制

先采用节省磁盘的开发配置；若发现频繁增量构建太慢，再按开发场景调整。不要把 target 目录提交 Git。

```
# Cargo.toml (workspace root)
[profile.dev]
debug = 0
incremental = false

[profile.release]
strip = "symbols"
lto = "thin"
codegen-units = 8
# .cargo/config.toml (Windows 示例；按实际磁盘调整)
[build]
target-dir = "D:/RustBuild/image-toolbox"
```

清理：cargo clean 清除当前 target 的构建产物；rustup toolchain list 查看额外工具链；清理 vcpkg/buildtrees 前确认没有正在构建。CI 缓存需要设置生命周期；在 Windows 系统中验证 libvips DLL 的自动打包与 PATH 策略。

## 12. AI 编码任务拆分（按顺序实施）

<!-- table 7 -->

| 迭代 | 交付内容 | 完成门槛 |
| --- | --- | --- |
| M0 基础工程 | Cargo workspace、Slint 主窗口、日志、设置、CI | Windows 可启动；无 WebView；release 可打包 |
| M1 浏览管理 | 文件夹选择、扫描、分页/虚拟列表、缩略图 | 1 万条模拟数据能滚动；扫描不冻结 UI |
| M2 预览 | 适应窗口/实际尺寸、缩放、平移、EXIF 修正 | 连续切图 500 次无持续内存增长 |
| M3 图片处理 | libvips adapter、resize、rotate、crop、格式导出 | 覆盖 JPEG/PNG/WebP 回归测试 |
| M4 批处理 | 有界队列、进度、取消、冲突策略 | 100 张混合图片失败可重试、无误覆盖 |
| M5 基础调色 | 非破坏性参数、撤销重做、CPU 导出 | 参数持久化、预览与导出对比测试 |
| M6 GPU 画布 | WGPU texture/shader、实时调色（可选） | 实测达到目标；否则保留稳定路径 |
| M7 发布 | 性能、安全、异常恢复、许可证、安装包 | 通过验收清单并附测试报告 |

## 13. 每次交给 AI 的任务模板

```
你是本项目的 Rust/Slint 工程师。仅实现当前里程碑【M?】。
约束：禁止 WebView/Electron/CEF；不改动无关模块；原图不覆盖；
后台任务不可阻塞 Slint UI 线程；所有外部依赖锁定版本。
工作步骤：
1. 阅读 docs/spec 与现有 crate API，先说明改动文件清单。
2. 以最小可编译增量实现功能，提供错误处理与取消机制。
3. 添加单元/集成测试和至少一个失败场景测试。
4. 执行 cargo fmt --check、cargo clippy、cargo test、cargo build。
5. 汇报实现、测试结果、性能测量、遗留风险。
验收条件：按本规格书 M? 的门槛逐条给出证据。
不得声称未经执行的测试已经通过。
```

## 14. 测试矩阵 / 发布验收

- 格式样本：小图、8K 大图、异常 EXIF、透明 PNG、CMYK JPEG、多页 TIFF、动画 GIF、损坏文件、无扩展名文件。
- IO 边界：中文、空格、超长路径、只读路径、网络盘、符号链接、文件变化、输出目录权限不足。
- 批处理：中途取消、磁盘已满、输出重名、目标已存在、部分失败后重试、退出重启恢复。
- 性能：冷热启动、滚动 10k、连续切图 500 次、批转 100 张、GPU/CPU 占用、缓存命中率。
- 发布：安装/卸载、DLL 依赖、三方许可证清单、杀毒误报检查、无联网时完整可用。

## 15. 风险与明确决策

<!-- table 8 -->

| 风险 | 应对方式 |
| --- | --- |
| Slint GPU API 与 wgpu 版本耦合 | M0 锁版本；M6 独立技术 spike；失败则保持 CPU 预览 |
| libvips 编解码器差异 | 启动时能力探测，发布机实际功能回归 |
| GPU 与 CPU 色彩不一致 | 统一参数定义、测试图像和容差 |
| 超大图引发 OOM | 图片尺寸限制、分级预览、有界队列和缓存上限 |
| 跨平台文件路径差异 | 保留 OsString/PathBuf，平台相关模块隔离 |
| 商业发布许可证风险 | 发布前审查 Slint 授权方案、libvips 与每个 codec 许可证 |

## 16. 参考资料与版本锁定

以下为官方资料与生态参考；开发时应以 Cargo.lock 中的实际版本及对应文档为准。

- Slint 后端与渲染器：https://docs.slint.dev/latest/docs/slint/guide/backends-and-renderers/backends_and_renderers/
- Slint Winit Backend：https://docs.slint.dev/latest/docs/slint/guide/backends-and-renderers/backend_winit/
- Slint WGPU 纹理集成：https://docs.slint.dev/latest/docs/rust/slint/wgpu_30/
- Slint Image 类型：https://docs.slint.dev/latest/docs/rust/slint/struct.Image
- libvips 官网：https://www.libvips.org/
- libvips 源码与绑定说明：https://github.com/libvips/libvips
- Rust libvips 绑定示例：https://docs.rs/vips/latest/vips/

建议的第一条开发任务：创建最小 Rust + Slint 原生程序，显示目录列表和一张由 libvips 生成的缩略图，记录首次启动、内存与编译目录体积。成功后再进入 M1。
