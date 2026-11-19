# ADR 0002：图像引擎——纯 Rust 后端与 libvips 迁移路径

## 状态（Status）

已接受（v1），实现进行中。2026-11-18。相关：[ADR 0001](0001-workspace-and-layering.md)、[ADR 0005](0005-batch-safety.md)。

## 背景（Context）

规格书首选的图像处理方案是 **libvips + 经验证过的 Rust 绑定**，并要求把绑定封装在 `image_engine` 内部。但当前开发机：

- 没有安装 libvips（也没有可用的 DLL / pkg-config 环境）；
- 没有可用的 C 工具链与 MSVC 链接器，无法在本地构建需要本地依赖的绑定；
- 仍需要尽快交付可运行、可打包的纯原生 v1。

同时规格书明确要求：**输出格式能力必须运行时探测**，UI 只展示当前后端真正具备的导出选项，不能硬编码“全部格式都支持”。

## 决策（Decision）

1. v1 的 `gimage-engine` 使用**纯 Rust `image` crate** 作为编解码后端，EXIF 方向读取使用 `kamadak-exif`。
2. 所有像素能力继续只通过 `gimage_core::ImageEngine` trait 暴露；后端名称为 `"pure-rust"`，版本取 `CARGO_PKG_VERSION`。
3. libvips **不是被放弃，而是延后**：实现一个同样实现该 trait 的适配器即可替换，业务层与 UI 不感知。
4. 启动时必须调用 `capabilities()` 得到可读 / 可写 / 仅单帧格式列表；导出对话框的格式选项由此生成。
5. 当前启用的 `image` 特性（见根 `Cargo.toml`）：`jpeg`、`png`、`webp`、`gif`、`bmp`、`tiff`、`ico`。未启用的格式（如 AVIF、HEIC、SVG）必须由能力探测报告为不可读，而不是在 UI 中假称支持。

### 引擎契约

```rust
pub trait ImageEngine: Send + Sync {
    fn name(&self) -> &str;
    fn version(&self) -> &str;
    fn capabilities(&self) -> CodecCapabilities;
    fn probe(&self, path: &Path) -> Result<ImageInfo, EngineError>;
    fn thumbnail(&self, path: &Path, max_edge: u32) -> Result<RgbaImage, EngineError>;
    fn render(
        &self,
        path: &Path,
        params: &EditParams,
        max_edge: Option<u32>,
    ) -> Result<RgbaImage, EngineError>;
    fn encode(
        &self,
        image: &RgbaImage,
        format: ExportFormat,
        quality: u8,
        path: &Path,
    ) -> Result<(), EngineError>;
}
```

不变式：

- 实现必须 `Send + Sync`，缩略图线程池和批量 worker 会并发调用 `thumbnail` / `render` / `encode`。
- `probe` 只读文件头/元数据，不分配整幅像素缓冲。
- `thumbnail` / `render` 先应用 EXIF 方向，再按 `max_edge` 等比缩小，**不放大**。
- `EditParams` 中的裁剪框使用相对已方向归一化图像的 0..=1 坐标；固定顺序：方向 → 裁剪 → 旋转 → 翻转 → 色调 → 缩放。
- `encode` 只写调用方给的路径；调用方负责临时文件 + 原子重命名，引擎不得覆盖源文件。
- `EngineOptions::max_decode_pixels` 默认 180,000,000，超过限制返回 `EngineError::TooLarge`，用于防解压炸弹。
- 像素格式统一为 8-bit RGBA、行优先、直通（非预乘）alpha。

### 迁移路径（libvips）

1. 在 `crates/engine` 新增 `VipsImageEngine`（或独立 `engine-vips` crate），只实现上述 trait，不改变 `core` 类型。
2. 初始化时探测 libvips 版本与编解码器，填充 `CodecCapabilities`；无法初始化则回退纯 Rust 后端并记录日志。
3. 在 `app` 的依赖注入点按配置/探测结果选择实现，UI 与业务层不变。
4. 用同一组样例图跑 golden image 对比（尺寸、方向、裁剪区域、容差、导出体积），确认预览与导出一致后再切换默认后端。
5. 打包脚本负责随包携带 libvips 运行时及其依赖 DLL，并核对许可证。

## 后果（Consequences）

- 当前 `crates/engine` 仍是脚手架：`capabilities` / `probe` / `thumbnail` / `render` / `encode` 以及 `export_image`、`write_atomic` 等函数尚未实现（`todo!()`）。在实现完成前，应用不能解码或导出任何图片。
- **能力以运行时探测为准，不以本文或规格书表格为准。** 例如当前 `image` 特性未包含 AVIF/HEIC 解码器，它们必须显示为不可读；JPEG/PNG/WebP 是否可导出也应由 `capabilities().writable` 决定。CMYK JPEG、多页 TIFF、动画 GIF 等行为同样必须探测或明确降级（首版动画只取首帧）。
- 纯 Rust 后端不需要本地 C 依赖，便于在无 MSVC 的机器上构建和打包；代价是超大图/批量转换的吞吐可能低于 libvips，M7 前需用基准测试验证。
- `MetadataPolicy` 目前只实现 `Strip`；EXIF 方向在渲染时归一化到像素，输出不携带原始方向标签。
- 若将来启用 AVIF/HEIC 特性，需要重新评估编译时间、二进制体积与专利/许可，并更新 `CodecCapabilities` 与本文档。