use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    env, fs,
    fs::{File, OpenOptions},
    io::ErrorKind,
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawFd,
        process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use walkdir::WalkDir;

mod migrate;
mod prune;

#[allow(unused_imports)]
pub(crate) use migrate::{CopyKind, CopyStats, reflink_or_copy};

const SCHEMA: u32 = 1;
const DAY: u64 = 86400;

#[derive(Parser)]
#[command(
    name = "cslim",
    version,
    about = "管理 target 生命周期与版本化共享编译缓存",
    after_help = "示例: cargo cslim build --release\n      cargo cslim gc --dry-run\n      cargo cslim status\nCargo 参数原样转发；不要通过本工具构建之外的进程使用托管目录。"
)]
struct Cli {
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// 查看托管目录、占用和最近使用时间
    Status {
        #[arg(long)]
        json: bool,
    },
    /// 回收过期或超出容量的闲置目录；默认仅预览
    Gc {
        #[arg(long, conflicts_with = "apply")]
        dry_run: bool,
        #[arg(long)]
        apply: bool,
        #[arg(long)]
        prune: bool,
    },
    /// 细粒度裁剪 target 中的旧版本产物与集成测试 runner；默认仅预览
    Prune {
        #[arg(long, conflicts_with = "apply")]
        dry_run: bool,
        #[arg(long)]
        apply: bool,
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u64).range(1..))]
        keep: u64,
        #[arg(long, default_missing_value = "true", num_args = 0..=1, default_value_t = true)]
        clean_tests: bool,
        #[arg(long)]
        all: bool,
    },
    /// 盘点并显式导入已注册工作树中的旧 target；默认只预览
    Migrate {
        #[arg(long = "project")]
        projects: Vec<String>,
        #[arg(long)]
        apply: bool,
        #[arg(long, default_value_t = 48)]
        min_idle_hours: u64,
        #[arg(long)]
        report: Option<PathBuf>,
        #[arg(long)]
        recover: Option<PathBuf>,
    },
    #[command(name = "__probe-references", hide = true)]
    ProbeReferences,
    /// 定期回收，只对本工具创建的目录生效
    Watch {
        #[arg(long, default_value_t=3600, value_parser=clap::value_parser!(u64).range(1..))]
        interval: u64,
    },
    /// 显示当前 workspace 的共享编译缓存统计
    CacheStats,
    /// 打印实际配置
    Config,
    /// 列出配置中的本机项目
    Projects,
    /// 列出同仓库的工作树及指定模块的 manifest
    Worktrees {
        #[arg(long)]
        project: String,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        all: bool,
    },
    /// 在已注册项目的 manifest 目录执行封装命令
    Project {
        name: String,
        #[arg(long)]
        worktree: Option<PathBuf>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    /// 输出当前 workspace 的托管构建目录，不执行构建
    TargetDir {
        #[arg(long)]
        manifest_path: Option<PathBuf>,
    },
    /// 输出迁移工具使用的目录与编译器元数据
    TargetInfo {
        #[arg(long)]
        manifest_path: PathBuf,
    },
    #[command(external_subcommand)]
    Cargo(Vec<String>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    root: PathBuf,
    target_ttl_days: u64,
    cache_ttl_days: u64,
    target_max_gib: u64,
    cache_max_gib: u64,
    cache_per_toolchain_gib: u64,
    sccache: PathBuf,
    auto_gc: bool,
    slim_profile: bool,
    auto_prune_tests: bool,
    prune_keep: usize,
    gc_prune: bool,
    projects: BTreeMap<String, PathBuf>,
}
impl Default for Config {
    fn default() -> Self {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let cache = env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache"));
        Self {
            root: cache.join("cslim"),
            target_ttl_days: 14,
            cache_ttl_days: 30,
            target_max_gib: 100,
            cache_max_gib: 40,
            cache_per_toolchain_gib: 10,
            sccache: "sccache".into(),
            auto_gc: true,
            slim_profile: true,
            auto_prune_tests: true,
            prune_keep: 1,
            gc_prune: true,
            projects: BTreeMap::new(),
        }
    }
}
impl Config {
    fn load(cli: &Cli) -> Result<Self> {
        let base = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".config")
            });
        let explicit = cli
            .config
            .clone()
            .or_else(|| env::var_os("CSLIM_CONFIG").map(PathBuf::from));
        let path = explicit
            .clone()
            .unwrap_or_else(|| base.join("cslim/config.toml"));
        let mut cfg: Self = match fs::read_to_string(&path) {
            Ok(s) => {
                toml::from_str(&s).with_context(|| format!("配置解析失败: {}", path.display()))?
            }
            Err(e) if e.kind() == ErrorKind::NotFound && explicit.is_none() => Self::default(),
            Err(e) => return Err(e).with_context(|| format!("无法读取配置: {}", path.display())),
        };
        if let Some(root) = cli
            .root
            .clone()
            .or_else(|| env::var_os("CSLIM_HOME").map(PathBuf::from))
        {
            cfg.root = root;
        }
        if !cfg.root.is_absolute() {
            bail!("root 必须为绝对路径");
        }
        for n in [
            cfg.target_max_gib,
            cfg.cache_max_gib,
            cfg.cache_per_toolchain_gib,
        ] {
            if n > u64::MAX / (1 << 30) {
                bail!("容量配置过大");
            }
        }
        if cfg.cache_per_toolchain_gib == 0 {
            bail!("cache_per_toolchain_gib 必须大于 0");
        }
        Ok(cfg)
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn hash(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))[..24].to_owned()
}
fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    use std::io::Write;
    let result = (|| -> Result<()> {
        file.write_all(&serde_json::to_vec_pretty(value)?)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
fn private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) => {
            if !m.is_dir()
                || m.file_type().is_symlink()
                || m.uid() != unsafe { libc::geteuid() }
                || m.mode() & 0o022 != 0
            {
                bail!(
                    "托管目录必须属于当前用户、非链接且不可被其他用户写入: {}",
                    path.display()
                );
            }
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
            match builder.create(path) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::AlreadyExists => return private_dir(path),
                Err(e) => return Err(e.into()),
            }
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

struct Store {
    base: PathBuf,
}
impl Store {
    fn open(root: &Path) -> Result<Self> {
        // Ancestors may be root-owned, but must never traverse a symlink.
        for ancestor in root.ancestors().collect::<Vec<_>>().into_iter().rev() {
            if let Ok(m) = fs::symlink_metadata(ancestor)
                && m.file_type().is_symlink()
            {
                bail!("root 路径不可经过符号链接: {}", ancestor.display());
            }
        }
        if let Some(parent) = root.parent() {
            fs::create_dir_all(parent)?;
        }
        private_dir(root)?;
        let base = root.join("v1");
        private_dir(&base)?;
        for sub in ["targets", "caches", "locks", "sockets"] {
            private_dir(&base.join(sub))?;
        }
        Ok(Self { base })
    }
    fn lock(&self, kind: &str, id: &str, exclusive: bool, wait: bool) -> Result<Option<File>> {
        let p = self.base.join("locks").join(format!("{kind}-{id}.lock"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .mode(0o600)
            .open(p)?;
        let result = match (exclusive, wait) {
            (true, true) => FileExt::lock_exclusive(&file),
            (false, true) => FileExt::lock_shared(&file),
            (true, false) => FileExt::try_lock_exclusive(&file),
            (false, false) => FileExt::try_lock_shared(&file),
        };
        match result {
            Ok(()) => Ok(Some(file)),
            Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    fn entry_dir(&self, kind: &str, id: &str) -> PathBuf {
        self.base.join(kind).join(id)
    }
    fn prepare(
        &self,
        kind: &str,
        id: &str,
        workspace: Option<String>,
        compiler: String,
    ) -> Result<PathBuf> {
        let dir = self.entry_dir(kind, id);
        private_dir(&dir)?;
        let meta = dir.join("entry.json");
        if meta.exists() {
            let _ = self.read_entry(kind, &dir)?;
        } else {
            if fs::read_dir(&dir)?.next().is_some() {
                bail!("拒绝接管非空目录: {}", dir.display());
            }
            write_json(
                &meta,
                &Entry {
                    schema: SCHEMA,
                    kind: kind.into(),
                    id: id.into(),
                    workspace,
                    compiler,
                    created_at: now(),
                    last_used: now(),
                },
            )?;
        }
        let data = dir.join("artifacts");
        private_dir(&data)?;
        Ok(data)
    }
    fn touch(&self, kind: &str, id: &str) -> Result<()> {
        let dir = self.entry_dir(kind, id);
        let mut entry = self.read_entry(kind, &dir)?;
        entry.last_used = now();
        write_json(&dir.join("entry.json"), &entry)
    }
    fn read_entry(&self, kind: &str, dir: &Path) -> Result<Entry> {
        private_existing_dir(dir)?;
        let p = dir.join("entry.json");
        let f = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(p)?;
        let e: Entry = serde_json::from_reader(f)?;
        let id = dir.file_name().and_then(|s| s.to_str()).unwrap_or_default();
        if e.schema != SCHEMA || e.kind != kind || e.id != id || !valid_id(id) {
            bail!("非法托管记录: {}", dir.display());
        }
        Ok(e)
    }
}
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 100 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}
fn private_existing_dir(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir()
        || m.file_type().is_symlink()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o022 != 0
    {
        bail!("不安全的托管目录: {}", path.display());
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    schema: u32,
    kind: String,
    id: String,
    workspace: Option<String>,
    compiler: String,
    created_at: u64,
    last_used: u64,
}
#[derive(Serialize)]
struct Row {
    #[serde(flatten)]
    entry: Entry,
    bytes: u64,
    busy: bool,
}
fn dir_size(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    for item in WalkDir::new(path).follow_links(false) {
        let item = match item {
            Ok(e) => e,
            Err(e)
                if e.io_error()
                    .is_some_and(|e| e.kind() == ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        if item.file_type().is_file() {
            // Use allocated bytes rather than apparent size (sparse files).
            match fs::symlink_metadata(item.path()) {
                Ok(m) => total = total.saturating_add(m.blocks().saturating_mul(512)),
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(total)
}
fn rows(store: &Store, kind: &str) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    for item in fs::read_dir(store.base.join(kind))? {
        let item = item?;
        let id = item.file_name().to_string_lossy().into_owned();
        if !valid_id(&id) {
            eprintln!("cslim: 跳过未知目录 {}", item.path().display());
            continue;
        }
        let lock = store.lock(kind, &id, true, false)?;
        match store.read_entry(kind, &item.path()) {
            Ok(entry) => rows.push(Row {
                entry,
                bytes: dir_size(&item.path())?,
                busy: lock.is_none(),
            }),
            Err(e) => eprintln!("cslim: 跳过无法验证的记录: {e:#}"),
        }
    }
    rows.sort_by_key(|r| r.entry.last_used);
    Ok(rows)
}
fn human(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{:.2} GiB", bytes as f64 / (1u64 << 30) as f64)
    } else {
        format!("{:.2} MiB", bytes as f64 / (1u64 << 20) as f64)
    }
}
fn status(store: &Store, json: bool) -> Result<()> {
    let mut all = rows(store, "targets")?;
    all.extend(rows(store, "caches")?);
    if json {
        println!("{}", serde_json::to_string_pretty(&all)?);
    } else {
        println!("托管根目录: {}", store.base.display());
        for r in &all {
            println!(
                "{} {} {} 闲置 {} 天 {} {}",
                r.entry.kind,
                r.entry.id,
                human(r.bytes),
                now().saturating_sub(r.entry.last_used) / DAY,
                if r.busy { "[使用中]" } else { "" },
                r.entry.workspace.as_deref().unwrap_or("共享缓存")
            );
        }
        println!(
            "合计 {}（{} 个目录）",
            human(all.iter().map(|r| r.bytes).sum()),
            all.len()
        );
    }
    Ok(())
}

fn gc(store: &Store, cfg: &Config, apply: bool, force_prune: bool) -> Result<()> {
    let Some(_gc_lock) = store.lock("gc", "global", true, false)? else {
        return Ok(());
    };
    let mut reclaimed: u64 = 0;

    // 细粒度清理优先阶段：在检查整个 target 是否过期前，如果开启了细粒度清理
    // (cslim gc --prune 或 cfg.gc_prune，或容量超限时的优先阶段)，先对闲置 target 执行 prune 瘦身
    let target_list = rows(store, "targets")?;
    let target_total: u64 = target_list.iter().map(|r| r.bytes).sum();
    let target_limit = cfg.target_max_gib;
    let over_capacity = target_limit > 0 && target_total > target_limit * (1 << 30);
    let should_prune = force_prune || cfg.gc_prune || over_capacity;

    if should_prune {
        let prune_opts = prune::PruneOptions {
            keep: cfg.prune_keep,
            clean_tests: cfg.auto_prune_tests,
            apply,
        };
        for r in &target_list {
            let Some(_lock) = store.lock("targets", &r.entry.id, true, false)? else {
                continue;
            };
            let artifacts = store.entry_dir("targets", &r.entry.id).join("artifacts");
            if artifacts.is_dir() {
                let stats = prune::prune_artifacts_dir(&artifacts, &prune_opts)?;
                if stats.total_bytes_reclaimed > 0 {
                    eprintln!(
                        "{} targets {} prune 瘦身 {} (清理 {} 个文件)",
                        if apply { "执行" } else { "预览" },
                        r.entry.id,
                        human(stats.total_bytes_reclaimed),
                        stats.total_files_removed
                    );
                    reclaimed = reclaimed.saturating_add(stats.total_bytes_reclaimed);
                }
            }
        }
    }

    for (kind, days, limit) in [
        ("targets", cfg.target_ttl_days, cfg.target_max_gib),
        ("caches", cfg.cache_ttl_days, cfg.cache_max_gib),
    ] {
        let list = rows(store, kind)?;
        let mut total: u64 = list.iter().map(|r| r.bytes).sum();
        for r in list {
            let expired = now().saturating_sub(r.entry.last_used) >= days.saturating_mul(DAY);
            let over = limit > 0 && total > limit * (1 << 30);
            if !expired && !over {
                continue;
            }
            let Some(_lock) = store.lock(kind, &r.entry.id, true, false)? else {
                eprintln!("cslim: 跳过使用中的 {} {}", kind, r.entry.id);
                continue;
            };
            let path = store.entry_dir(kind, &r.entry.id);
            // Re-read under the deletion lock: a build may have used this entry since the scan.
            let current = store.read_entry(kind, &path)?;
            if current.last_used != r.entry.last_used {
                continue;
            }
            eprintln!(
                "{} {} {} {} ({})",
                if apply { "回收" } else { "预览" },
                kind,
                r.entry.id,
                human(r.bytes),
                if expired { "过期" } else { "容量限制" }
            );
            if apply {
                if kind == "caches" {
                    // Never remove an active cache server's on-disk storage.
                    let mut cmd = cache_command(store, cfg, &r.entry.id);
                    let result = cmd
                        .arg("--stop-server")
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()?;
                    if !result.success() {
                        bail!("无法停止缓存服务 {}，保留目录", r.entry.id);
                    }
                }
                fs::remove_dir_all(&path)?;
            }
            total = total.saturating_sub(r.bytes);
            reclaimed = reclaimed.saturating_add(r.bytes);
        }
    }
    eprintln!(
        "{} {}",
        if apply { "已回收" } else { "可回收" },
        human(reclaimed)
    );
    Ok(())
}

fn prune_target_cmd(
    store: &Store,
    cfg: &Config,
    apply: bool,
    keep: usize,
    clean_tests: bool,
    all: bool,
    toolchain: Option<&str>,
) -> Result<()> {
    let opts = prune::PruneOptions {
        keep,
        clean_tests,
        apply,
    };
    eprintln!(
        "cslim prune: 模式={} (保留版本={}, 清理测试 runner={})",
        if apply {
            "执行 (apply)"
        } else {
            "预览 (dry-run)"
        },
        keep,
        clean_tests
    );

    let mut target_ids = Vec::new();

    // 如果未指定 --all，尝试定位当前 workspace 的 target
    if !all && let Ok(ctx) = build_context(&[], toolchain, cfg) {
        let target_dir = store.entry_dir("targets", &ctx.target_id);
        if target_dir.exists() {
            target_ids.push((ctx.target_id, Some(ctx.workspace.display().to_string())));
        }
    }

    // 如果未匹配到当前 workspace 或指定了 --all，处理所有托管 targets
    if target_ids.is_empty() {
        let list = rows(store, "targets")?;
        for r in list {
            target_ids.push((r.entry.id, r.entry.workspace));
        }
    }

    if target_ids.is_empty() {
        eprintln!("cslim prune: 未找到任何已托管的 target 目录");
        return Ok(());
    }

    let mut total_stats = prune::PruneStats::default();

    for (id, ws) in &target_ids {
        let Some(_lock) = store.lock("targets", id, true, false)? else {
            eprintln!("cslim: 跳过使用中的 targets {id}");
            continue;
        };

        let artifacts = store.entry_dir("targets", id).join("artifacts");
        if !artifacts.is_dir() {
            continue;
        }

        eprintln!(
            "\n扫描 targets {} ({})",
            id,
            ws.as_deref().unwrap_or("未知工作区")
        );
        let before_size = dir_size(&artifacts)?;
        let stats = prune::prune_artifacts_dir(&artifacts, &opts)?;
        total_stats.merge(&stats);

        if stats.total_files_removed > 0 {
            let after_size = if apply {
                dir_size(&artifacts)?
            } else {
                before_size.saturating_sub(stats.total_bytes_reclaimed)
            };
            eprintln!(
                "  {} {} -> {} (回收 {})",
                if apply {
                    "瘦身完成:"
                } else {
                    "预计瘦身:"
                },
                human(before_size),
                human(after_size),
                human(stats.total_bytes_reclaimed)
            );
        } else {
            eprintln!("  产物均为最新版本，无需清理");
        }
    }

    eprintln!(
        "\n合计: {} {} 个文件，回收空间 {}",
        if apply { "已清理" } else { "可清理" },
        total_stats.total_files_removed,
        human(total_stats.total_bytes_reclaimed)
    );
    if !apply && total_stats.total_files_removed > 0 {
        eprintln!("提示: 运行带有 --apply 的命令以真正执行删除: cslim prune --apply");
    }

    Ok(())
}

fn cargo_command(toolchain: Option<&str>) -> Command {
    let mut c = Command::new("cargo");
    if let Some(t) = toolchain {
        c.arg(t);
    }
    c
}
fn output(mut cmd: Command) -> Result<String> {
    let out = cmd.output().context("无法执行命令")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}
struct BuildContext {
    workspace: PathBuf,
    compiler: String,
    cache_id: String,
    target_id: String,
}
fn build_context(args: &[String], toolchain: Option<&str>, cfg: &Config) -> Result<BuildContext> {
    let mut locate = cargo_command(toolchain);
    locate.args(["locate-project", "--workspace", "--message-format", "plain"]);
    let pre: Vec<_> = args.iter().take_while(|a| a.as_str() != "--").collect();
    for (i, arg) in pre.iter().enumerate() {
        if arg.as_str() == "--manifest-path" {
            locate
                .arg("--manifest-path")
                .arg(pre.get(i + 1).context("--manifest-path 缺少参数")?);
        } else if arg.starts_with("--manifest-path=") {
            locate.arg(arg);
        }
        if arg.as_str() == "--target-dir"
            || arg.starts_with("--target-dir=")
            || arg.as_str() == "--config"
            || arg.starts_with("--config=")
            || arg.as_str() == "-C"
            || arg.starts_with("-C")
        {
            bail!(
                "cslim 托管模式不接受 --target-dir、--config 或 -C；请使用项目 .cargo/config.toml 配置其他选项"
            );
        }
    }
    let workspace = PathBuf::from(output(locate)?)
        .canonicalize()?
        .parent()
        .context("无 workspace")?
        .to_path_buf();
    // rustup honors the same cwd/toolchain selection as Cargo. Explicit RUSTC takes precedence.
    let explicit_rustc = env::var_os("RUSTC").or_else(|| env::var_os("CARGO_BUILD_RUSTC"));
    let mut rustc = if let Some(executable) = explicit_rustc {
        Command::new(executable)
    } else if let Some(t) = toolchain {
        let mut c = Command::new("rustup");
        c.args(["run", t.trim_start_matches('+'), "rustc"]);
        c
    } else {
        Command::new("rustc")
    };
    rustc.arg("-vV");
    let compiler = output(rustc)?;
    let mut sccache = Command::new(&cfg.sccache);
    sccache.arg("--version");
    let cache_id = hash(&format!(
        "v1\n{compiler}\n{}",
        output(sccache).context("请安装 sccache，或在配置中设置 sccache 路径")?
    ));
    let target_id = format!("{}-{}", hash(&workspace.to_string_lossy()), hash(&compiler));
    Ok(BuildContext {
        workspace,
        compiler,
        cache_id,
        target_id,
    })
}
fn cache_command(store: &Store, cfg: &Config, id: &str) -> Command {
    let mut cmd = Command::new(&cfg.sccache);
    cache_env(&mut cmd, store, cfg, id);
    cmd
}
fn cache_env(cmd: &mut Command, store: &Store, cfg: &Config, id: &str) {
    for (key, _) in env::vars_os() {
        if key.to_string_lossy().starts_with("SCCACHE_") {
            cmd.env_remove(key);
        }
    }
    cmd.env(
        "SCCACHE_DIR",
        store.entry_dir("caches", id).join("artifacts"),
    )
    .env(
        "SCCACHE_CACHE_SIZE",
        format!("{}G", cfg.cache_per_toolchain_gib),
    )
    .env(
        "SCCACHE_SERVER_UDS",
        store.base.join("sockets").join(format!("{id}.sock")),
    )
    .env("SCCACHE_IDLE_TIMEOUT", "60")
    .env("SCCACHE_CONF", store.base.join("sccache.toml"));
}
fn child_locks(cmd: &mut Command, locks: &[&File]) {
    let fds: Vec<_> = locks.iter().map(|l| l.as_raw_fd()).collect();
    // Retain flock descriptors through exec: GC stays safe even if the supervisor is SIGKILLed.
    unsafe {
        cmd.pre_exec(move || {
            for fd in &fds {
                let flags = libc::fcntl(*fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(*fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}
fn forward(
    store: &Store,
    cfg: &Config,
    args: &[String],
    toolchain: Option<&str>,
    stats: bool,
) -> Result<i32> {
    let ctx = build_context(args, toolchain, cfg)?;
    // Lock order is always cache -> target. A shared cache allows different workspaces to build concurrently.
    let cache_lock = store.lock("caches", &ctx.cache_id, false, true)?.unwrap();
    {
        let _setup = store.lock("meta", &ctx.cache_id, true, true)?;
        store.prepare("caches", &ctx.cache_id, None, ctx.compiler.clone())?;
        store.touch("caches", &ctx.cache_id)?;
        // Start the daemon before exec inherits build locks; daemon lifetimes must not pin a target.
        if !stats {
            let status = cache_command(store, cfg, &ctx.cache_id)
                .arg("--show-stats")
                .stdout(Stdio::null())
                .status()
                .context("启动 sccache 失败")?;
            if !status.success() {
                bail!("sccache 启动失败");
            }
        }
    }
    if stats {
        let mut cmd = cache_command(store, cfg, &ctx.cache_id);
        cmd.arg("--show-stats");
        child_locks(&mut cmd, &[&cache_lock]);
        return Ok(cmd.status()?.code().unwrap_or(1));
    }
    let target_lock = store.lock("targets", &ctx.target_id, true, true)?.unwrap();
    let target = store.prepare(
        "targets",
        &ctx.target_id,
        Some(ctx.workspace.display().to_string()),
        ctx.compiler,
    )?;
    store.touch("targets", &ctx.target_id)?;
    // Serialize last-used writes from parallel builds of different workspaces.
    {
        let _meta = store.lock("meta", &ctx.cache_id, true, true)?;
        store.touch("caches", &ctx.cache_id)?;
    }
    let mut cmd = cargo_command(toolchain);
    // sccache hashes CARGO_* env vars. Pass workspace-specific paths as Cargo config,
    // otherwise CARGO_TARGET_DIR itself prevents cross-workspace cache hits.
    let quoted = toml::Value::String(target.display().to_string()).to_string();
    cmd.args(["--config", &format!("build.target-dir={quoted}")])
        .args(["--config", &format!("build.build-dir={quoted}")]);
    if cfg.slim_profile {
        cmd.args([
            "--config",
            r#"profile.dev.package."*".debug=0"#,
            "--config",
            r#"profile.test.package."*".debug=0"#,
        ]);
    }
    cmd.args(args)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .env_remove("CARGO_BUILD_BUILD_DIR")
        .env("RUSTC_WRAPPER", &cfg.sccache)
        .env("CARGO_INCREMENTAL", "0");
    cache_env(&mut cmd, store, cfg, &ctx.cache_id);
    child_locks(&mut cmd, &[&cache_lock, &target_lock]);
    eprintln!("cslim: target={} cache={}", target.display(), ctx.cache_id);
    let result = cmd.status().context("Cargo 执行失败")?;
    store.touch("targets", &ctx.target_id)?;
    {
        let _meta = store.lock("meta", &ctx.cache_id, true, true)?;
        store.touch("caches", &ctx.cache_id)?;
    }
    drop(target_lock);
    drop(cache_lock);
    if cfg.auto_gc
        && let Err(e) = gc(store, cfg, true, false)
    {
        eprintln!("cslim: 自动回收失败: {e:#}");
    }
    Ok(result
        .code()
        .unwrap_or_else(|| 128 + result.signal().unwrap_or(1)))
}
fn dispatch_cargo(
    store: &Store,
    cfg: &Config,
    args: &[String],
    toolchain: Option<&str>,
) -> Result<i32> {
    let cmd = args.first().map(String::as_str).unwrap_or("");
    if ![
        "build", "b", "check", "c", "test", "t", "run", "r", "bench", "doc", "d", "rustc",
        "rustdoc", "clippy", "clean",
    ]
    .contains(&cmd)
    {
        bail!(
            "仅封装构建命令 build/check/test/run/bench/doc/rustc/rustdoc/clippy/clean，其他命令请直接使用 cargo"
        );
    }
    forward(store, cfg, args, toolchain, false)
}

fn project_manifest<'a>(cfg: &'a Config, name: &str) -> Result<&'a Path> {
    let manifest = cfg
        .projects
        .get(name)
        .with_context(|| format!("未注册项目 {name}，运行 cargo cslim projects 查看项目"))?;
    if !manifest.is_absolute() || manifest.file_name().is_none_or(|f| f != "Cargo.toml") {
        bail!("项目 {name} 必须配置 Cargo.toml 的绝对路径");
    }
    if !manifest.is_file() {
        bail!("项目 manifest 不存在: {}", manifest.display());
    }
    Ok(manifest)
}

#[derive(Debug, Serialize)]
struct Worktree {
    path: PathBuf,
    head: String,
    branch: Option<String>,
    locked: Option<String>,
    prunable: Option<String>,
    available: bool,
    manifest: PathBuf,
    manifest_exists: bool,
}
fn git_root(path: &Path) -> Result<PathBuf> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"]);
    Ok(PathBuf::from(output(cmd)?).canonicalize()?)
}
fn git_common(path: &Path) -> Result<PathBuf> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(path)
        .args(["rev-parse", "--git-common-dir"]);
    Ok(path.join(output(cmd)?).canonicalize()?)
}
fn worktrees(cfg: &Config, name: &str) -> Result<Vec<Worktree>> {
    let manifest = project_manifest(cfg, name)?.canonicalize()?;
    let root = git_root(manifest.parent().context("manifest 无父目录")?)?;
    let relative = manifest.strip_prefix(&root)?;
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(&root).args([
        "-c",
        "core.quotePath=false",
        "worktree",
        "list",
        "--porcelain",
    ]);
    let text = output(cmd)?;
    let mut entries = Vec::new();
    for record in text.split("\n\n") {
        let mut entry = Worktree {
            path: PathBuf::new(),
            head: String::new(),
            branch: None,
            locked: None,
            prunable: None,
            available: false,
            manifest: PathBuf::new(),
            manifest_exists: false,
        };
        for line in record.lines() {
            let (key, value) = line.split_once(' ').unwrap_or((line, ""));
            match key {
                "worktree" => {
                    // Older Git lacks `worktree list -z`; reject unsupported quoting safely.
                    let path = if value.starts_with('"') {
                        serde_json::from_str::<String>(value).context("不支持的 Git 路径转义")?
                    } else {
                        value.to_owned()
                    };
                    entry.path = path.into();
                }
                "HEAD" => entry.head = value.into(),
                "branch" => entry.branch = Some(value.trim_start_matches("refs/heads/").into()),
                "locked" => entry.locked = Some(value.into()),
                "prunable" => entry.prunable = Some(value.into()),
                _ => {}
            }
        }
        if !entry.path.as_os_str().is_empty() {
            entry.available = entry.path.is_dir();
            entry.manifest = entry.path.join(relative);
            entry.manifest_exists = entry.manifest.is_file();
            entries.push(entry);
        }
    }
    Ok(entries)
}
fn selected_manifest(cfg: &Config, name: &str, worktree: Option<&Path>) -> Result<PathBuf> {
    let configured = project_manifest(cfg, name)?.canonicalize()?;
    let Some(path) = worktree else {
        return Ok(configured);
    };
    let source_root = git_root(configured.parent().context("manifest 无父目录")?)?;
    let selected = path.canonicalize().context("工作树不存在")?;
    if git_root(&selected)? != selected || git_common(&selected)? != git_common(&source_root)? {
        bail!("所选目录不是同一仓库的工作树根目录: {}", selected.display());
    }
    let manifest = selected.join(configured.strip_prefix(&source_root)?);
    if !manifest.is_file() {
        bail!("该工作树没有此 Rust 模块: {}", manifest.display());
    }
    Ok(manifest)
}

fn project(
    store: &Store,
    cfg: &Config,
    name: &str,
    worktree: Option<&Path>,
    args: &[String],
    toolchain: Option<&str>,
) -> Result<i32> {
    let manifest = selected_manifest(cfg, name, worktree)?;
    // Match running Cargo in this crate, including its toolchain and .cargo/config hierarchy.
    env::set_current_dir(manifest.parent().context("项目目录不存在")?)?;
    let args = if args.first().is_some_and(|a| a == "--") {
        &args[1..]
    } else {
        args
    };
    if args
        .iter()
        .take_while(|a| a.as_str() != "--")
        .any(|a| a == "--manifest-path" || a.starts_with("--manifest-path="))
    {
        bail!("项目入口已固定 manifest，请在配置中注册其他项目或直接使用 cargo cslim");
    }
    match args.first().map(String::as_str) {
        Some("target-dir") if args.len() == 1 => {
            let ctx = build_context(&[], toolchain, cfg)?;
            println!(
                "{}",
                store
                    .entry_dir("targets", &ctx.target_id)
                    .join("artifacts")
                    .display()
            );
            Ok(0)
        }
        Some("cache-stats") if args.len() == 1 => forward(store, cfg, &[], toolchain, true),
        Some("fmt") => {
            // Formatting has no artifacts and needs no cache or GC lock.
            let mut cmd = cargo_command(toolchain);
            let status = cmd.args(args).status()?;
            Ok(status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)))
        }
        _ => dispatch_cargo(store, cfg, args, toolchain),
    }
}

fn run() -> Result<i32> {
    let mut args: Vec<String> = env::args().collect();
    if args.get(1).is_some_and(|a| a == "cslim") {
        args.remove(1);
    }
    let toolchain = if args.get(1).is_some_and(|a| a.starts_with('+')) {
        Some(args.remove(1))
    } else {
        None
    };
    let cli = Cli::parse_from(args);
    if matches!(cli.command, Action::ProbeReferences) {
        migrate::probe_stdin()?;
        return Ok(0);
    }
    let cfg = Config::load(&cli)?;
    if matches!(cli.command, Action::Config) {
        println!("{}", toml::to_string_pretty(&cfg)?);
        return Ok(0);
    }
    let store = Store::open(&cfg.root)?;
    // Empty config prevents a user's unrelated sccache backend configuration from leaking into this cache.
    let conf = store.base.join("sccache.toml");
    if !conf.exists() {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(conf)?;
    }
    match cli.command {
        Action::Status { json } => status(&store, json)?,
        Action::Gc { apply, prune, .. } => gc(&store, &cfg, apply, prune)?,
        Action::Prune {
            apply,
            keep,
            clean_tests,
            all,
            ..
        } => prune_target_cmd(
            &store,
            &cfg,
            apply,
            keep as usize,
            clean_tests,
            all,
            toolchain.as_deref(),
        )?,
        Action::Migrate {
            projects,
            apply,
            min_idle_hours,
            report,
            recover,
        } => migrate::run(
            &store,
            &cfg,
            &projects,
            apply,
            min_idle_hours,
            report.as_deref(),
            recover.as_deref(),
        )?,
        Action::Watch { interval } => loop {
            if let Err(e) = gc(&store, &cfg, true, false) {
                eprintln!("cslim: 回收失败: {e:#}");
            }
            thread::sleep(Duration::from_secs(interval));
        },
        Action::CacheStats => return forward(&store, &cfg, &[], toolchain.as_deref(), true),
        Action::Cargo(args) => return dispatch_cargo(&store, &cfg, &args, toolchain.as_deref()),
        Action::Projects => {
            for (name, manifest) in &cfg.projects {
                println!("{name}\t{}", manifest.display());
            }
        }
        Action::Project {
            name,
            worktree,
            args,
        } => {
            return project(
                &store,
                &cfg,
                &name,
                worktree.as_deref(),
                &args,
                toolchain.as_deref(),
            );
        }
        Action::Worktrees { project, json, all } => {
            let entries: Vec<_> = worktrees(&cfg, &project)?
                .into_iter()
                .filter(|e| all || e.available)
                .collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else {
                for e in entries {
                    println!(
                        "{}\t{}\t{}",
                        e.path.display(),
                        e.branch.as_deref().unwrap_or("detached"),
                        if e.manifest_exists {
                            "可构建"
                        } else {
                            "目录或模块缺失"
                        }
                    );
                }
            }
        }
        Action::TargetInfo { manifest_path } => {
            let args = vec![
                "--manifest-path".to_owned(),
                manifest_path.display().to_string(),
            ];
            let ctx = build_context(&args, toolchain.as_deref(), &cfg)?;
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "schema": SCHEMA, "root": cfg.root, "id": ctx.target_id,
                    "workspace": ctx.workspace, "compiler": ctx.compiler,
                    "target_dir": store.entry_dir("targets", &ctx.target_id).join("artifacts")
                }))?
            );
        }
        Action::ProbeReferences => unreachable!(),
        Action::TargetDir { manifest_path } => {
            let args = manifest_path
                .map(|p| vec!["--manifest-path".to_owned(), p.display().to_string()])
                .unwrap_or_default();
            let ctx = build_context(&args, toolchain.as_deref(), &cfg)?;
            println!(
                "{}",
                store
                    .entry_dir("targets", &ctx.target_id)
                    .join("artifacts")
                    .display()
            );
        }
        Action::Config => unreachable!(),
    }
    Ok(0)
}
fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("cslim: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, Store, Config) {
        let tmp = TempDir::new().unwrap();
        let cfg = Config {
            root: tmp.path().join("cslim"),
            auto_gc: false,
            ..Config::default()
        };
        let store = Store::open(&cfg.root).unwrap();
        (tmp, store, cfg)
    }
    fn target(store: &Store, id: &str, age: u64) -> PathBuf {
        let p = store
            .prepare("targets", id, Some("/example".into()), "rustc-test".into())
            .unwrap();
        fs::write(p.join("output"), "rebuildable").unwrap();
        let mut e = store.read_entry("targets", p.parent().unwrap()).unwrap();
        e.last_used = now() - age;
        write_json(&p.parent().unwrap().join("entry.json"), &e).unwrap();
        p
    }
    #[test]
    fn expired_target_is_only_removed_on_apply() {
        let (_tmp, store, cfg) = fixture();
        let old = target(&store, "abc", 15 * DAY);
        let fresh = target(&store, "def", DAY);
        gc(&store, &cfg, false, false).unwrap();
        assert!(old.exists());
        for _ in 0..20 {
            gc(&store, &cfg, true, false).unwrap();
            if !old.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(!old.exists());
        assert!(fresh.exists());
    }
    #[test]
    fn active_build_is_never_collected() {
        let (_tmp, store, cfg) = fixture();
        let p = target(&store, "abc", 15 * DAY);
        let lock = store.lock("targets", "abc", true, true).unwrap().unwrap();
        gc(&store, &cfg, true, false).unwrap();
        assert!(p.exists());
        // Explicitly unlock: parallel tests may fork a short-lived child before exec.
        FileExt::unlock(&lock).unwrap();
        drop(lock);
        for _ in 0..20 {
            gc(&store, &cfg, true, false).unwrap();
            if !p.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(!p.exists());
    }
    #[test]
    fn symlinks_and_unowned_entries_are_not_followed() {
        let (tmp, store, cfg) = fixture();
        let outside = tmp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("precious"), "source").unwrap();
        symlink(&outside, store.base.join("targets/abc")).unwrap();
        let managed = target(&store, "def", 15 * DAY);
        symlink(&outside, managed.join("external")).unwrap();
        fs::create_dir(store.base.join("targets/123")).unwrap();
        fs::write(store.base.join("targets/123/source"), "unknown").unwrap();
        for _ in 0..20 {
            gc(&store, &cfg, true, false).unwrap();
            if !managed.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(outside.join("precious").exists());
        assert!(store.base.join("targets/123/source").exists());
        assert!(!managed.exists());
    }
    #[test]
    fn capacity_reclaims_least_recently_used_first() {
        let (_tmp, store, mut cfg) = fixture();
        cfg.target_max_gib = 1;
        let old = target(&store, "abc", 2 * DAY);
        let fresh = target(&store, "def", DAY);
        // Sparse files should not trigger disk pressure.
        File::create(old.join("sparse"))
            .unwrap()
            .set_len(2 << 30)
            .unwrap();
        assert!(dir_size(&old).unwrap() < 1 << 30);
        // Actual allocation (not apparent size) exceeds the limit.
        let file = File::create(old.join("large")).unwrap();
        assert_eq!(
            unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, (1 << 30) + 4096) },
            0
        );
        for _ in 0..20 {
            gc(&store, &cfg, true, false).unwrap();
            if !old.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(!old.exists());
        assert!(fresh.exists());
    }
    #[test]
    fn active_cache_is_preserved_and_stop_failure_keeps_data() {
        let (_tmp, store, mut cfg) = fixture();
        cfg.cache_ttl_days = 0;
        cfg.sccache = "/bin/false".into();
        let data = store
            .prepare("caches", "abc", None, "compiler".into())
            .unwrap();
        fs::write(data.join("cached"), "artifact").unwrap();
        let lock = store.lock("caches", "abc", false, true).unwrap().unwrap();
        gc(&store, &cfg, true, false).unwrap();
        assert!(data.join("cached").exists());
        FileExt::unlock(&lock).unwrap();
        drop(lock);
        let mut collected = false;
        for _ in 0..20 {
            if gc(&store, &cfg, true, false).is_err() {
                collected = true;
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(collected);
        assert!(data.join("cached").exists());
    }
    #[test]
    fn refuses_symlink_root_and_bad_metadata() {
        let (tmp, store, _cfg) = fixture();
        symlink(&store.base, tmp.path().join("link")).unwrap();
        assert!(Store::open(&tmp.path().join("link/child")).is_err());
        let p = target(&store, "abc", DAY);
        let mut e = store.read_entry("targets", p.parent().unwrap()).unwrap();
        e.schema = 99;
        write_json(&p.parent().unwrap().join("entry.json"), &e).unwrap();
        assert!(store.read_entry("targets", p.parent().unwrap()).is_err());
        assert!(!valid_id("../abc"));
    }
    #[test]
    fn config_slim_profile_defaults_to_true_and_can_be_disabled() {
        let default_cfg = Config::default();
        assert!(default_cfg.slim_profile);

        let parsed: Config = toml::from_str("slim_profile = false\n").unwrap();
        assert!(!parsed.slim_profile);

        let parsed_default: Config = toml::from_str("").unwrap();
        assert!(parsed_default.slim_profile);
    }
    #[test]
    fn config_prune_settings_support_and_defaults() {
        let default_cfg = Config::default();
        assert!(default_cfg.auto_prune_tests);
        assert_eq!(default_cfg.prune_keep, 1);
        assert!(default_cfg.gc_prune);

        // 向下兼容：老配置（未包含这些字段）仍可正确反序列化，且不触发 deny_unknown_fields
        let parsed_old: Config = toml::from_str(
            r#"
            root = "/tmp/cslim"
            target_ttl_days = 7
            "#,
        )
        .unwrap();
        assert!(parsed_old.auto_prune_tests);
        assert_eq!(parsed_old.prune_keep, 1);
        assert!(parsed_old.gc_prune);

        // 自定义配置项
        let parsed_custom: Config = toml::from_str(
            r#"
            auto_prune_tests = false
            prune_keep = 3
            gc_prune = false
            "#,
        )
        .unwrap();
        assert!(!parsed_custom.auto_prune_tests);
        assert_eq!(parsed_custom.prune_keep, 3);
        assert!(!parsed_custom.gc_prune);
    }
    #[test]
    fn gc_with_prune_shrinks_target_before_eviction() {
        let (_tmp, store, mut cfg) = fixture();
        cfg.target_max_gib = 1;
        // target 大小在 prune 后会减小
        let p = store
            .prepare(
                "targets",
                "aa11",
                Some("/example".into()),
                "rustc-test".into(),
            )
            .unwrap();
        let deps = p.join("debug/deps");
        fs::create_dir_all(&deps).unwrap();

        // 写入测试 runner
        let test_bin = deps.join("test-1111111111111111");
        let test_d = deps.join("test-1111111111111111.d");
        {
            let mut f = File::create(&test_bin).unwrap();
            use std::io::Write;
            f.write_all(&[0x7f, b'E', b'L', b'F', 0, 0, 0, 0]).unwrap();
            let mut perms = f.metadata().unwrap().permissions();
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o755);
            f.set_permissions(perms).unwrap();
        }
        fs::write(&test_d, "test: tests/foo.rs\n").unwrap();

        // 写入多版本 rlib
        fs::write(deps.join("libdemo-1111111111111111.rlib"), "old").unwrap();
        std::thread::sleep(Duration::from_millis(15));
        fs::write(deps.join("libdemo-2222222222222222.rlib"), "new").unwrap();

        // 执行带 prune 的 gc
        gc(&store, &cfg, true, true).unwrap();

        // 测试 runner 与旧 rlib 应该被清理，新 rlib 应该保留，target 目录依然存在
        assert!(!test_bin.exists());
        assert!(!test_d.exists());
        assert!(!deps.join("libdemo-1111111111111111.rlib").exists());
        assert!(deps.join("libdemo-2222222222222222.rlib").exists());
        assert!(p.exists());
    }
}
