# cslim

管理 Rust 构建产物的 Cargo 封装。每个 workspace 保留独立 `target`，共享的编译结果由 sccache 按版本缓存；通过 TTL、容量上限和定时回收控制长期占用。

命令为 `cslim` 或 `cargo cslim`，安装包为 `cargo-cslim`。存储格式版本为 `v1`。

面向 Linux/Unix。需要 Rust/Cargo ≥ 1.91、sccache；Linux x86_64 和 sccache 0.18.0 已验证。该项目不依赖 cargo-sweep 或 cargo-clean-all。

## 安装

```bash
# 已有 sccache 可跳过；此脚本安装 0.18.0 并核对固定 SHA-256
python3 scripts/install-sccache.py

# 安装到 ~/.local/bin，并启用用户级每小时 GC
./scripts/install.sh --timer

mkdir -p ~/.config/cslim
cp config.example.toml ~/.config/cslim/config.toml
# 如需另一块磁盘，在配置中设置 root 的绝对路径
```

确保 `~/.local/bin` 和 Rustup 的 Cargo 在 PATH 中。安装后可使用 `cslim build` 或 Cargo 子命令 `cargo cslim build`。配置根目录的父目录必须可写；由工具创建的根目录仅供当前用户使用。如果不配置，默认根目录为 `${XDG_CACHE_HOME:-~/.cache}/cslim`。

`--timer` 会安装 `deploy/` 下的 systemd user service/timer。无需 systemd 时可使用 `cargo cslim watch --interval 3600`，或自己定期运行 `cargo cslim gc --apply`。用户级定时任务需要 user manager 持续运行；可通过 `loginctl show-user "$USER" -p Linger` 查看退出登录后的运行条件。

## 使用

在 Rust 项目目录中，把构建命令的 `cargo` 换成 `cargo cslim`：

```bash
cargo cslim build --release
cargo cslim test --locked
cargo cslim check
cargo cslim clippy --all-targets -- -D warnings
cargo cslim run -- --your-program-argument
cargo +stable cslim build
# 也支持 cargo cslim +stable build
cargo cslim build --manifest-path /path/to/project/Cargo.toml

cargo cslim status             # 所有托管 target/cache 的占用、闲置时间
cargo cslim status --json      # 含编译器版本、workspace、使用状态
cargo cslim cache-stats        # 当前工具链缓存的命中统计
cargo cslim config            # 实际生效配置
cargo cslim gc                # 默认只预览
cargo cslim gc --dry-run
cargo cslim gc --apply        # 实际回收
cargo cslim clean             # 清当前 workspace/工具链的 target，保留共享缓存
```

支持 build/check/test/run/bench/doc/rustc/rustdoc/clippy/clean 及 Cargo 常见短别名。依赖管理、发布、安装等其他命令直接使用原生 Cargo。默认每次封装命令结束后执行 GC，长期不构建时由 timer 触发。

封装器日志写 stderr，保留 Cargo stdout（包括 `--message-format=json`）及正常失败码。可执行文件位于日志或 `status` 显示的托管 `artifacts/` 中，不再写入项目原来的 `target/`。

## 命名项目与本机开发脚本

在 `~/.config/cslim/config.toml` 登记绝对 manifest 路径，即可从任意目录调用；命令会切换到 manifest 目录，保留项目的 `.cargo/config.toml` 与 Rustup 工具链选择：

```toml
[projects]
demo = "/absolute/path/to/demo/Cargo.toml"
```

```bash
cslim projects
cslim project demo check --locked
cslim project demo target-dir
cslim project demo cache-stats
```

脚本中写死了 `cargo` 时，可用 `cslim-dev` 在当前命令的进程树内接入。构建子命令通过 cslim，`fmt`、`metadata` 等非构建命令仍由原生 Cargo 执行：

```bash
cd /path/to/project
cslim-dev cargo check --locked
cslim-dev pnpm run check:rust
```

`cslim-dev` 只作用于显式运行的命令，不改全局 Cargo、CI 或发布脚本。构建脚本内部运行 Cargo 时恢复原 PATH，避免重复获取同一 target 的锁。若脚本硬编码读取原项目 `target/` 内的二进制，需改为使用 `cslim target-dir` 的输出。`--target-dir` 和 Cargo `--config` 等覆盖托管路径的参数会被明确拒绝。

`target-dir` 只计算路径，不创建 target，也不提供长期租约。长时间运行的程序应通过 `cslim run` 持锁，或先复制到托管目录之外。

## 历史工作树与旧 target

历史 checkout 无需逐个登记，直接从已注册项目选择同仓库的工作树：

```bash
cslim worktrees --project demo
cslim worktrees --project demo --all --json
cslim project demo --worktree /path/to/old-worktree build --locked
```

工具核对 Git common directory 和工作树根路径，拒绝其他仓库、子目录或缺少相应模块的 checkout。每个工作树的 target 独立，共享兼容的 sccache；不会删除、切换、prune 或提交工作树源码。不存在的登记仅在 `--all` 输出中保留，不会迁移。

旧 target 需要显式导入。迁移脚本只查找已注册仓库工作树中对应 Cargo manifest 旁的默认 `target/`，不猜测自定义 `CARGO_TARGET_DIR` 与 workspace 的对应关系：

```bash
# 默认仅预览；report 必须放在源码与被迁移目录之外
python3 scripts/migrate-targets.py --report /path/to/private/plan.json
# 核对报告后执行，或通过 --project 限定一个模块
python3 scripts/migrate-targets.py --project demo \
  --apply --report /path/to/private/result.json
```

迁移使用当前 `cslim config` 中的 root。默认要求目录递归 mtime/ctime 均至少 48 小时无变化，并核对：Rust 标记、Git 未跟踪状态、当前用户所有权、Cargo 构建锁、cslim target 锁、进程 cwd/exe/fd/maps/参数/环境引用、所有 Docker 容器挂载（包含停止的容器）、内部挂载点及目标容量预算。检查进程时通过 `sudo -n` 执行随项目提供的只读 probe；不输出命令行或环境变量内容。检查不完整时不迁移。

已有受管目录不会覆盖；有活动引用、近期变化、经过符号链接、含 Git 数据或无法验证的目录均保留。迁移先在原盘同目录暂存并再次检查引用与元数据；同盘 rename，跨盘 copy 后逐文件 SHA-256/链接结构校验，再将所有权交给 cslim 并移除旧副本。操作失败时恢复原目录；若原路径已被其他进程重建则保留副本、报告恢复冲突，不覆盖新目录。普通 GC 不处理未提交的导入暂存数据。

导入后保留期从导入时间开始计算（默认 14 天），之后沿用现有 GC/容量规则；原工作树的 `target` 路径被移走，后续应使用本工具构建。已经编译的 `.rlib` 不会反向变成 sccache 条目；Cargo 会自行判断旧产物是否兼容，需要重建时才逐步填充共享缓存。

每个迁移事务的日志保存在 `<root>/v1/imports/*.json`。进程被 SIGKILL / 掉电等中断时，先检查日志和原盘 `.cslim-migration-*` 暂存目录，再执行恢复：

```bash
python3 scripts/migrate-targets.py --recover /absolute/root/v1/imports/TRANSACTION.json
```

恢复默认回退未提交的导入，已提交的导入只清理经过检查的旧暂存副本；原位置有新目录、数据发生变化或仍被引用时会保留数据并拒绝覆盖。

## 版本化与跨项目复用

```text
<root>/v1/
├── targets/<workspace-hash>-<rustc-hash>/
│   ├── entry.json
│   └── artifacts/           # workspace 独立的构建目录
├── caches/<cache-version-hash>/
│   ├── entry.json
│   └── artifacts/           # sccache 压缩后的共享编译结果
├── sockets/                # 每个缓存版本独立的 sccache daemon
├── locks/                  # 不随 target 回收的稳定锁文件
└── sccache.toml             # 独立配置，避免混用用户其他缓存后端
```

- `v1` 是本工具的存储格式版本。
- target 标识由 workspace 的规范化路径和 `rustc -vV` 生成，同一 workspace 的成员共用 target。
- 共享缓存命名空间由存储格式、`rustc -vV`（包含 release/commit/host）和 sccache 版本生成。升级 Rust 或 sccache 会创建新命名空间，旧版本保留到过期或容量淘汰。
- 命名空间内由 sccache 对源文件、依赖内容、编译参数、target triple、features、profile 及相关环境变量计算内容键。crate 版本、源码或配置变化不会直接复用不兼容的旧结果。Cargo.lock 的整体哈希不放入命名空间，以免不同项目无法共享相同依赖。
- 目标平台、profile 和 features 仍由 Cargo/sccache 分别识别；不手工复制 `.rlib`，不共用整个 target。
- 为避免 `CARGO_TARGET_DIR` 等不同路径进入 sccache 的环境哈希，本工具清除这几个目录变量，通过 Cargo `--config build.target-dir=...` 和 `build.build-dir=...` 传入托管位置。

本工具强制 `CARGO_INCREMENTAL=0`，因为 sccache 的 Rust 缓存要求关闭 incremental。已有 `RUSTC_WRAPPER` 被替换，`RUSTC_WORKSPACE_WRAPPER` 保留。继承的 `SCCACHE_*` 设置会被清除，使用当前命名空间自己的本地缓存和 socket。若通过 `.cargo/config.toml` 的 `build.rustc` 使用定制编译器，请同时用 `RUSTC=/absolute/path/to/rustc cargo cslim build` 显式指定，以便命名空间版本与实际编译器一致。

修改单工具链缓存容量后，需要等待该版本的 sccache daemon 空闲退出（默认 60 秒），再构建时才会应用新限额；总量 GC 限额会立即生效。

共享命中依赖编译输入一致，不保证任意两个项目都命中。sccache 对仅产出 metadata 的 `cargo check`、需要系统链接器的 bin/proc-macro 等调用有缓存限制；这些命令仍可运行并复用各自 target。有关限制见 [sccache Rust 支持说明](https://github.com/mozilla/sccache/blob/v0.18.0/docs/Rust.md)。

## 清理策略

默认配置：

| 对象 | 过期规则 | 总容量上限 |
|---|---|---|
| target | 14 天未通过封装器使用 | 100 GiB |
| 共享缓存命名空间 | 30 天未通过封装器使用 | 40 GiB |
| 单个工具链的 sccache | sccache 内部管理 | 10 GiB |

- TTL 在整个 workspace/工具链 target 或共享缓存命名空间层面生效，按最近一次封装调用开始/结束的时间计算，不扫描单个 crate 的使用时间。
- 超过总容量上限时按最近使用时间淘汰整个闲置目录。活跃目录不能淘汰，因此这是构建之间的软上限；单次构建仍可能超过上限。
- `target_max_gib` / `cache_max_gib` 为 0 表示不按总容量回收；TTL 为 0 表示闲置后立即过期。`cache_per_toolchain_gib` 必须大于 0。
- 容量按实际分配的文件块计算，符号链接不跟随。共享缓存压缩和目标文件可能同时存在，这是为了兼顾重建速度与清理能力。
- 回收共享缓存命名空间前先停止其专属 sccache daemon。目录过期时整版回收；正在使用的命名空间由 sccache 自己在容量限制内淘汰条目。

配置位置为 `${XDG_CONFIG_HOME:-~/.config}/cslim/config.toml`。可用 `CSLIM_CONFIG` 或 `cargo cslim --config /path/to/config.toml ...` 指定其他配置；root 优先级为 CLI `--root` > `CSLIM_HOME` > 配置 > 默认值。配置示例见 [config.example.toml](config.example.toml)。

## 并发与回收边界

同一 target 的封装调用互斥；不同 workspace 可并发构建，共享缓存使用读锁。GC 使用非阻塞独占锁跳过活跃目录，并在真正删除前重新核对最近使用时间。锁文件独立于被清理的目录，避免删除后重建产生两把锁。锁描述符传给 Cargo 子进程，即使封装器被 SIGKILL，仍在运行的子构建也受保护。

仅回收本工具创建且记录验证通过的目录，拒绝符号链接根目录、错误元数据和其他用户可写的托管目录。普通 GC 不扫描历史项目 `target/`、外部 `CARGO_TARGET_DIR`、Cargo registry 源码或下载缓存。首次构建会创建新 target；只有显式执行上述迁移脚本才会导入通过检查的旧产物。

持续管理的前提是构建走 `cargo cslim`。请不要直接使用原生 Cargo、IDE 或其他脚本写入本工具托管的目录，否则这些进程没有 GC 锁。IDE 可继续使用自己的默认 target；长期运行的程序应部署到托管目录之外。

## 验证

```bash
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked
python3 scripts/smoke.py
python3 scripts/test_cslim_dev.py
python3 scripts/test_worktrees.py /path/to/cargo-cslim
python3 scripts/test_migrate_targets.py
```

集成测试使用临时 workspace 和真实 Cargo/sccache，验证命名项目与项目配置加载、跨项目命中、缓存版本隔离、依赖版本升级、Cargo JSON/退出码、GC 预览/实际删除、旧 target 保留、并发构建，以及 supervisor 被 SIGKILL 后的回收保护。测试依赖本机 Cargo 缓存已有 `itoa 1.0.18`；本项目的正常构建会准备该版本，无需测试时下载 registry。

## 停用与回滚

```bash
systemctl --user disable --now cslim-gc.timer
# 后续直接使用 cargo build / cargo test，回到项目原有目录
```

需要恢复原生 Cargo 行为时无需改 shell 或全局 Cargo 配置。托管产物都是可重建数据；删除整个 root 前应停用 timer，并确保没有封装构建或缓存 daemon 运行。可先把配置中的 TTL 设为 0，再运行 `cargo cslim gc --apply`，让工具按锁安全回收，完成后删除 root。保留源码仓库即可重新安装。
