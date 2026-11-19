# ADR 0003：Windows GNU 工具链与构建目录

## 状态（Status）

已接受（v1），2026-11-18。相关：[README](../../README.md)。

## 背景（Context）

规格书以 Windows 为首要目标。当前开发机没有安装 Visual Studio / MSVC 链接器（`link.exe`），MSVC 目标无法完成链接；系统里也没有完整的 MinGW 环境。

2026-11-18 续开发验证：仅 Rust GNU 工具链不足以完成本项目构建。`getrandom` 需要 `dlltool.exe`，SQLite 等原生依赖也需要 GNU C 工具。当前机器的 MinGW-w64 位于 `E:\tools\mingw64\bin`；运行 `scripts/setup-toolchain.ps1` 可将其加入当前进程 PATH，也可通过 `GIMAGE_MINGW_BIN` 指定其他安装位置。此操作不修改系统环境变量。

## 决策（Decision）

1. `rust-toolchain.toml` 固定：

   ```toml
   [toolchain]
   channel = "stable-x86_64-pc-windows-gnu"
   components = ["rustfmt", "clippy"]
   targets = []
   ```

2. 不要求、也不假设 MSVC；代码与脚本不得依赖 `link.exe`、`vcpkg` 或 MSVC 专用环境变量。
3. `.cargo/config.toml` 把构建目录移出仓库：

   ```toml
   [build]
   target-dir = "E:/RustBuild/gimage"

   [net]
   retry = 3
   ```

   目标盘符按实际机器调整；`target/` 永远不提交 Git。
4. 构建 profile 继续使用根 `Cargo.toml` 中面向开发机的设置：`debug = 0`、`incremental = false`；release 使用 `strip = "symbols"`、`lto = "thin"`、`codegen-units = 8`。
5. 打包脚本从 `cargo metadata` 解析实际 target 目录，不硬编码 `E:/RustBuild`；若 `gimage.exe` 不存在或构建失败则立即报错。
6. 规划：CI 使用同一 GNU target；构建缓存需设生命周期，并在发布机上复测编解码器与 DLL 依赖。

### 切换到 MSVC（当机器具备 Visual Studio Build Tools 时）

1. 安装 VS 2022 Build Tools，勾选 “Desktop development with C++” 与 Windows SDK。
2. `rustup toolchain install stable-x86_64-pc-windows-msvc`。
3. 把 `rust-toolchain.toml` 的 `channel` 改为 `stable-x86_64-pc-windows-msvc`。
4. 在 “Developer PowerShell for VS” 中运行，或确保 `link.exe` 与 Windows SDK 在 `PATH` 中。
5. 清理旧目标产物（不同 ABI 不能混用）：删除 `E:/RustBuild/gimage` 或改用独立的 target 目录，然后重新 `cargo build` / `cargo test`。
6. 重新验证打包产物：MSVC 版通常不再需要 MinGW 运行库 DLL，发布前用依赖查看工具核对。

## 后果（Consequences）

- 贡献者无需 MSVC 即可构建；但 GNU 产物可能动态依赖 MinGW 运行库（如 `libgcc_s_seh-1.dll`、`libwinpthread-1.dll`），打包与发布验收必须检查 DLL 依赖。
- 切换到 MSVC 属于 ABI 与链接器变更，必须全量重新构建与回归测试，不能复用 GNU 的增量产物。
- 构建目录在 `E:` 盘，工作区在仓库目录：清理/迁移机器时要分别处理；`cargo clean` 只清理当前 target 目录。
- 该决定不影响跨平台代码结构；macOS/Linux 构建需要各自的工具链与 target 目录策略。
