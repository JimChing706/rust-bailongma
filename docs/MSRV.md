# MSRV 决策审计（wave 2c）

**结论：工作区 MSRV = 1.88**（`rust-version = "1.88"`，三 crate 均通过 `rust-version.workspace = true` 继承）。

此前 `rust-version = "1.94"` 是按本机安装工具链填的占位值，本次依据 Cargo.lock 锁定版本逐一查证 crates.io 声明，确定真实下限。

## 判定方法

1. 从 `Cargo.lock` 取全部直接/传递依赖的**锁定版本**（430 个包）。
2. 对每个候选包，从 crates.io API（`/api/v1/crates/{name}/{version}`）读取其发布的 `rust_version` 字段；缺失时回退到 docs.rs 源码页的 `Cargo.toml`。
3. 用 `cargo tree -i` 确认该包是否真的进入**实际构建图**（区分 default 构建与 `--features desktop` 构建），排除锁文件里存在但未被编译的包。

## 证据表（锁定版本 → 声明的 rust-version → 图中位置）

| 包 | 锁定版本 | rust-version | 构建矩阵 | 引入路径 |
|---|---|---|---|---|
| **time** | 0.3.55 | **1.88.0** | desktop | wry → cookie |
| uuid | 1.24.0 | 1.85.0 | default | 直接依赖（core） |
| hashbrown | 0.17.1 | 1.85.0 | default | indexmap → h2 → hyper → axum |
| axum | 0.8.9 | 1.80 | default | 直接依赖（core） |
| wry | 0.56.0 | 1.77 | desktop | 直接依赖（app, optional） |
| tao | 0.36.0 | 1.74 | desktop | 直接依赖（app, optional） |
| windows | 0.61.3 | 1.74 | desktop | tao / wry / webview2-com |
| tray-icon | 0.24.2 | 1.73 | desktop | 直接依赖（app, optional） |
| tokio | 1.53.1 | 1.71 | default | 直接依赖 |
| thiserror | 2.0.20 | 1.71 | default | 直接依赖 |
| windows-sys | 0.61.2 | 1.71 | default | mio → tokio |
| syn | 3.0.3 | 1.71 | 构建期 | 过程宏链 |
| tower-http | 0.6.11 | 1.64 | default | 直接依赖 |
| reqwest | 0.12.28 | 1.64.0 | default | 直接依赖 |
| chrono | 0.4.45 | 1.62.0 | default | 直接依赖 |
| rusqlite / libsqlite3-sys | 0.32.1 / 0.30.1 | 未声明 | default | 直接依赖（bundled） |

> 注：`time`、`windows` 等包存在于 Cargo.lock，但默认构建图中并不编译它们；
> `time` 仅在 `--features desktop`（wry → cookie 0.18.2）链上编译，`windows` 仅在 desktop 链上编译。
> 由于 release 发布包含 desktop 二进制，MSRV 必须覆盖整条 desktop 链，故取全矩阵最大值。

## 判定

- 全矩阵下限 = **time 0.3.55 的 1.88.0**（desktop 链唯一的天花板，其余全部 ≤ 1.85）。
- 默认（无 desktop）构建下限 = 1.85.0（uuid / hashbrown）。
- 工作区统一声明 **1.88**：单一值、覆盖全部构建配置（default / desktop / 测试），避免按 feature 拆分维护成本。

## 验证

- [x] 本机 1.94 全量回归（cargo test --workspace）通过。
- [x] rustup 安装 1.88.0 minimal 后 `cargo +1.88.0 check --workspace --all-targets` 与 `-p bailongma-app --features desktop` 实机编译通过（记录见 wave 2c 提交）。
- [ ] CI 建议加 `MSRV` job（`rust-version: 1.88`）防止依赖升级悄悄抬高下限。
