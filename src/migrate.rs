//! Conservative, explicit import of old Cargo targets from registered Git worktrees.
//! All paths and live references are checked again while the target lock is held.
use super::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CString,
    io::{Read, Write},
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    sync::mpsc,
};

const PREFIX: &str = ".cslim-migration-";
const RESERVE: u64 = 20 * (1 << 30);

#[derive(Default)]
struct Snapshot {
    newest: u64,
    bytes: u64,
    signature: String,
    contents: BTreeMap<PathBuf, String>,
}

fn snapshot(path: &Path, hashes: bool) -> Result<Snapshot> {
    let root = fs::symlink_metadata(path)?;
    if !root.is_dir()
        || root.file_type().is_symlink()
        || root.uid() != unsafe { libc::geteuid() }
        || path.canonicalize()? != path
    {
        bail!(
            "target 必须属于当前用户，且路径不可经过符号链接: {}",
            path.display()
        );
    }
    let mut result = Snapshot::default();
    let mut signature = Sha256::new();
    for entry in WalkDir::new(path).follow_links(false).sort_by_file_name() {
        let entry = entry?;
        let item = entry.path();
        let meta = fs::symlink_metadata(item)?;
        let kind = if meta.is_dir() {
            "dir"
        } else if meta.is_file() {
            "file"
        } else if meta.file_type().is_symlink() {
            "link"
        } else {
            bail!("target 内含特殊文件: {}", item.display());
        };
        if item.file_name().is_some_and(|n| n == ".git")
            || meta.dev() != root.dev()
            || meta.uid() != root.uid()
        {
            bail!(
                "target 内含 Git 数据、跨文件系统项或其他用户文件: {}",
                item.display()
            );
        }
        let rel = item.strip_prefix(path)?;
        result.newest = result
            .newest
            .max(meta.mtime().max(meta.ctime()).max(0) as u64);
        result.bytes = result
            .bytes
            .saturating_add(meta.blocks().saturating_mul(512));
        signature.update(rel.as_os_str().as_bytes());
        signature.update(
            format!(
                "\0{kind}:{}:{}:{}:{}:{}:{}:{}\n",
                meta.dev(),
                meta.ino(),
                meta.mode(),
                meta.size(),
                meta.mtime_nsec(),
                meta.mtime(),
                if item == path { 0 } else { meta.ctime_nsec() }
            )
            .as_bytes(),
        );
        if item != path {
            signature
                .update(format!("{}\n", if item == path { 0 } else { meta.ctime() }).as_bytes());
        }
        if hashes {
            let value = match kind {
                "file" => {
                    let mut file = File::open(item)?;
                    let mut digest = Sha256::new();
                    std::io::copy(&mut file, &mut DigestWriter(&mut digest))?;
                    format!("file:{}:{:x}", meta.size(), digest.finalize())
                }
                "link" => format!(
                    "link:{:x}",
                    Sha256::digest(fs::read_link(item)?.as_os_str().as_bytes())
                ),
                _ => "dir".to_owned(),
            };
            result.contents.insert(rel.to_path_buf(), value);
        }
    }
    result.signature = format!("{:x}", signature.finalize());
    Ok(result)
}

struct DigestWriter<'a>(&'a mut Sha256);
impl Write for DigestWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn require_markers(path: &Path) -> Result<()> {
    if path.join(".rustc_info.json").is_file()
        || ["debug", "release"]
            .iter()
            .any(|p| path.join(p).join(".fingerprint").is_dir())
    {
        Ok(())
    } else {
        bail!("缺少 Rust 构建标记")
    }
}
fn require_untracked(path: &Path) -> Result<()> {
    let root = git_root(path.parent().context("target 无父目录")?)?;
    let rel = path.strip_prefix(&root)?;
    let out = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["ls-files", "-z", "--"])
        .arg(rel)
        .output()?;
    if !out.status.success() || !out.stdout.is_empty() {
        bail!("target 含 Git 跟踪文件，或无法检查 Git 状态")
    }
    Ok(())
}
fn cargo_locks(path: &Path) -> Result<Vec<File>> {
    let mut locks = Vec::new();
    for item in WalkDir::new(path).follow_links(false) {
        let item = item?;
        if item.file_name() != ".cargo-lock" {
            continue;
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(item.path())?;
        FileExt::try_lock_exclusive(&file).context("Cargo 构建锁正被使用")?;
        locks.push(file);
    }
    if locks.is_empty() {
        bail!("缺少 Cargo 构建锁文件")
    }
    Ok(locks)
}
fn free_bytes(path: &Path) -> Result<u64> {
    let c = CString::new(path.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(c.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let stat = unsafe { stat.assume_init() };
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
}
fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    if fs::symlink_metadata(destination).is_ok() {
        bail!("目标目录已经存在")
    }
    let mut directories = Vec::new();
    for item in WalkDir::new(source).follow_links(false) {
        let item = item?;
        let path = item.path();
        let rel = path.strip_prefix(source)?;
        let target = destination.join(rel);
        let meta = fs::symlink_metadata(path)?;
        if meta.is_dir() {
            fs::create_dir(&target)?;
            directories.push((target, meta.mode() & 0o777));
        } else if meta.is_file() {
            fs::copy(path, &target)?;
            fs::set_permissions(&target, fs::Permissions::from_mode(meta.mode() & 0o777))?;
        } else if meta.file_type().is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(path)?, &target)?;
        } else {
            bail!("迁移源包含特殊文件")
        }
    }
    for (directory, mode) in directories.into_iter().rev() {
        fs::set_permissions(directory, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}
fn random_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn path_hits(path: &Path, text: &str) -> bool {
    let needle = path.to_string_lossy();
    text == needle || text.starts_with(&format!("{needle}/")) || text.contains(needle.as_ref())
}
fn probe_processes(paths: &[PathBuf]) -> Result<BTreeMap<String, Vec<String>>> {
    let mut hits: BTreeMap<String, Vec<String>> = paths
        .iter()
        .map(|p| (p.display().to_string(), vec![]))
        .collect();
    let mut own = BTreeSet::new();
    let mut pid = std::process::id();
    while pid > 1 && own.insert(pid) {
        let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
        pid = status
            .lines()
            .find_map(|line| {
                line.strip_prefix("PPid:")
                    .and_then(|v| v.trim().parse().ok())
            })
            .unwrap_or(1);
    }
    for item in fs::read_dir("/proc")? {
        let item = item?;
        let Some(pid) = item.file_name().to_string_lossy().parse::<u32>().ok() else {
            continue;
        };
        if own.contains(&pid) {
            continue;
        }
        let dir = item.path();
        let references = (|| -> Result<(String, Vec<String>)> {
            let comm = fs::read_to_string(dir.join("comm"))?.trim().to_owned();
            let mut values = Vec::new();
            for name in ["cwd", "exe", "root"] {
                match fs::read_link(dir.join(name)) {
                    Ok(p) => values.push(p.to_string_lossy().into_owned()),
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
            for fd in fs::read_dir(dir.join("fd"))? {
                let fd = fd?;
                match fs::read_link(fd.path()) {
                    Ok(p) => values.push(p.to_string_lossy().into_owned()),
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
            for name in ["cmdline", "environ"] {
                values.extend(
                    fs::read(dir.join(name))?
                        .split(|b| *b == 0)
                        .map(|b| String::from_utf8_lossy(b).into_owned()),
                );
            }
            for line in fs::read_to_string(dir.join("maps"))?.lines() {
                if let Some(value) = line.split_whitespace().nth(5) {
                    values.push(value.to_owned())
                }
            }
            Ok((comm, values))
        })();
        let (comm, values) = match references {
            Ok(v) => v,
            Err(_) if !dir.exists() => continue,
            Err(e) => bail!("无法完整检查进程 {pid}: {e:#}"),
        };
        for path in paths {
            if values.iter().any(|v| path_hits(path, v)) {
                hits.get_mut(&path.display().to_string())
                    .unwrap()
                    .push(format!("process {pid} {comm}"));
            }
        }
    }
    Ok(hits)
}
pub(super) fn probe_stdin() -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("历史 target 迁移仅支持 Linux")
    }
    let paths: Vec<PathBuf> = serde_json::from_reader(std::io::stdin())?;
    println!("{}", serde_json::to_string(&probe_processes(&paths)?)?);
    Ok(())
}
fn wait_output_timeout(child: std::process::Child, seconds: u64) -> Result<std::process::Output> {
    let pid = child.id();
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let _ = send.send(child.wait_with_output());
    });
    match receive.recv_timeout(Duration::from_secs(seconds)) {
        Ok(result) => Ok(result?),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
            bail!("引用检查超过 {seconds} 秒，保留原 target")
        }
        Err(e) => Err(e.into()),
    }
}
fn docker_output(args: &[&str], seconds: u64) -> Result<String> {
    let mut cmd = Command::new("docker");
    let child = cmd
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("无法运行 Docker 挂载检查")?;
    let output = wait_output_timeout(child, seconds)?;
    if !output.status.success() {
        bail!(
            "Docker 挂载检查不完整: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    Ok(String::from_utf8(output.stdout)?)
}
fn references(paths: &[PathBuf]) -> Result<BTreeMap<String, Vec<String>>> {
    let mut cmd = if unsafe { libc::geteuid() } == 0 {
        Command::new(env::current_exe()?)
    } else {
        let mut c = Command::new("sudo");
        c.arg("-n").arg(env::current_exe()?);
        c
    };
    let mut child = cmd
        .arg("__probe-references")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("无法启动进程引用检查；需要 sudo -n 权限")?;
    serde_json::to_writer(child.stdin.take().context("probe stdin")?, paths)?;
    let result = wait_output_timeout(child, 60)?;
    if !result.status.success() {
        bail!(
            "进程引用检查不完整: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        )
    }
    let mut hits: BTreeMap<String, Vec<String>> = serde_json::from_slice(&result.stdout)?;
    let inventory = docker_output(&["ps", "-aq", "--no-trunc"], 15)?;
    if !inventory.is_empty() {
        let ids: Vec<&str> = inventory.lines().collect();
        let mut inspect_args = vec!["inspect"];
        inspect_args.extend(ids);
        let inspection = docker_output(&inspect_args, 60)?;
        let containers: Vec<serde_json::Value> = serde_json::from_str(&inspection)?;
        for container in &containers {
            if let Some(mounts) = container["Mounts"].as_array() {
                for mount in mounts {
                    let Some(source) = mount["Source"].as_str() else {
                        continue;
                    };
                    let source = Path::new(source);
                    for path in paths {
                        if source == path || source.starts_with(path) || path.starts_with(source) {
                            hits.get_mut(&path.display().to_string())
                                .unwrap()
                                .push(format!(
                                    "container {} mount",
                                    container["Name"].as_str().unwrap_or("?")
                                ));
                        }
                    }
                }
            }
        }
    }
    for line in fs::read_to_string("/proc/self/mountinfo")?.lines() {
        let Some(raw) = line.split_whitespace().nth(4) else {
            continue;
        };
        let mut mount = String::new();
        let bytes = raw.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\\'
                && i + 3 < bytes.len()
                && bytes[i + 1..i + 4]
                    .iter()
                    .all(|b| (b'0'..=b'7').contains(b))
            {
                mount.push(u8::from_str_radix(&raw[i + 1..i + 4], 8)? as char);
                i += 4;
            } else {
                mount.push(bytes[i] as char);
                i += 1;
            }
        }
        for path in paths {
            if Path::new(&mount) == path || Path::new(&mount).starts_with(path) {
                hits.get_mut(&path.display().to_string())
                    .unwrap()
                    .push("mountpoint inside target".into());
            }
        }
    }
    Ok(hits)
}
fn no_references(paths: &[PathBuf]) -> Result<()> {
    let found = references(paths)?;
    if found.values().any(|v| !v.is_empty()) {
        bail!("存在进程、容器或挂载引用: {found:?}")
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct Journal {
    format: String,
    source: PathBuf,
    quarantine: PathBuf,
    destination: PathBuf,
    id: String,
    started: u64,
    state: String,
    source_signature: String,
    bytes: u64,
    mode: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}
fn journal_write(path: &Path, journal: &Journal) -> Result<()> {
    write_json(path, journal)
}
struct MoveOptions {
    min_idle_hours: u64,
    budget: u64,
    reserve: u64,
    copy_always: bool,
}
fn move_target<F: Fn(&[PathBuf]) -> Result<()>>(
    store: &Store,
    source: &Path,
    manifest: &Path,
    cfg: &Config,
    options: MoveOptions,
    refcheck: &F,
) -> Result<serde_json::Value> {
    let old_dir = env::current_dir()?;
    env::set_current_dir(manifest.parent().context("manifest 无父目录")?)?;
    let context = build_context(
        &["--manifest-path".into(), manifest.display().to_string()],
        None,
        cfg,
    );
    env::set_current_dir(old_dir)?;
    let context = context?;
    let destination = store
        .entry_dir("targets", &context.target_id)
        .join("artifacts");
    let entry_dir = destination.parent().unwrap();
    if source.file_name().is_none_or(|n| n != "target") || source.starts_with(&store.base) {
        bail!("迁移源不符合目录边界")
    }
    let _target_lock = store
        .lock("targets", &context.target_id, true, false)?
        .context("target 正在使用")?;
    if fs::symlink_metadata(entry_dir).is_ok() {
        bail!("已有托管目录或未完成迁移")
    }
    require_markers(source)?;
    require_untracked(source)?;
    let _cargo_locks = cargo_locks(source)?;
    let before = snapshot(source, false)?;
    if now().saturating_sub(before.newest) < options.min_idle_hours.saturating_mul(3600) {
        bail!("近期有文件变化")
    }
    if options.budget > 0 && before.bytes > options.budget {
        bail!("超出 target 总容量预算")
    }
    if free_bytes(&store.base)? < before.bytes.saturating_add(options.reserve) {
        bail!("托管磁盘空间不足，需保留 20 GiB")
    }
    refcheck(&[source.to_path_buf()])?;
    if snapshot(source, false)?.signature != before.signature {
        bail!("检查期间 target 已改变")
    }
    let imports = store.base.join("imports");
    private_dir(&imports)?;
    let id = random_id()?;
    let quarantine = source.with_file_name(format!("{PREFIX}{id}"));
    if fs::symlink_metadata(&quarantine).is_ok() {
        bail!("迁移暂存目录已存在")
    }
    let journal_path = imports.join(format!("{id}.json"));
    let mut journal = Journal {
        format: "rust-v1".into(),
        source: source.into(),
        quarantine: quarantine.clone(),
        destination: destination.clone(),
        id: context.target_id.clone(),
        started: now(),
        state: "prepared".into(),
        source_signature: before.signature.clone(),
        bytes: before.bytes,
        mode: fs::symlink_metadata(source)?.mode() & 0o777,
        error: None,
    };
    journal_write(&journal_path, &journal)?;
    let mut moved = false;
    let mut committed = false;
    let mut created = false;
    let result = (|| -> Result<bool> {
        private_dir(entry_dir)?;
        created = true;
        write_json(
            &entry_dir.join("importing.json"),
            &serde_json::json!({"journal":journal_path, "source":source}),
        )?;
        fs::rename(source, &quarantine)?;
        journal.state = "quarantined".into();
        journal_write(&journal_path, &journal)?;
        refcheck(&[source.into(), quarantine.clone()])?;
        if snapshot(&quarantine, false)?.signature != before.signature {
            bail!("暂存期间 target 已改变")
        }
        let cross = options.copy_always
            || fs::symlink_metadata(&quarantine)?.dev() != fs::symlink_metadata(&store.base)?.dev();
        if cross {
            journal.state = "copying".into();
            journal_write(&journal_path, &journal)?;
            let expected = snapshot(&quarantine, true)?;
            copy_tree(&quarantine, &destination)?;
            if snapshot(&destination, true)?.contents != expected.contents
                || snapshot(&quarantine, false)?.signature != before.signature
            {
                bail!("复制完整性校验失败")
            }
        } else {
            fs::rename(&quarantine, &destination)?;
            moved = true;
        }
        refcheck(&[source.into(), quarantine.clone(), destination.clone()])?;
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o700))?;
        write_json(
            &entry_dir.join("entry.json"),
            &Entry {
                schema: SCHEMA,
                kind: "targets".into(),
                id: context.target_id.clone(),
                workspace: Some(context.workspace.display().to_string()),
                compiler: context.compiler.clone(),
                created_at: now(),
                last_used: now(),
            },
        )?;
        committed = true;
        journal.state = "committed".into();
        journal_write(&journal_path, &journal)?;
        fs::remove_file(entry_dir.join("importing.json"))?;
        if cross {
            refcheck(std::slice::from_ref(&quarantine))?;
            fs::remove_dir_all(&quarantine)?;
        }
        journal.state = "complete".into();
        journal_write(&journal_path, &journal)?;
        Ok(cross)
    })();
    match result {
        Ok(cross) => Ok(
            serde_json::json!({"source":source,"destination":destination,"bytes":before.bytes,"copy_verified":cross,"journal":journal_path}),
        ),
        Err(error) => {
            if !committed {
                if moved && destination.exists() && !quarantine.exists() {
                    let _ = fs::rename(&destination, &quarantine);
                }
                if quarantine.exists() && fs::symlink_metadata(source).is_err() {
                    let _ = fs::rename(&quarantine, source);
                    let _ = fs::set_permissions(source, fs::Permissions::from_mode(journal.mode));
                }
                if created && source.exists() && entry_dir.exists() {
                    let _ = fs::remove_dir_all(entry_dir);
                }
                journal.state = if source.exists() {
                    "rolled_back"
                } else {
                    "recovery_required"
                }
                .into();
            } else {
                journal.state = "cleanup_required".into();
            }
            journal.error = Some(format!("{error:#}"));
            let _ = journal_write(&journal_path, &journal);
            Err(error)
        }
    }
}

fn recover<F: Fn(&[PathBuf]) -> Result<()>>(
    store: &Store,
    path: &Path,
    refcheck: &F,
) -> Result<()> {
    let journal_path = path.canonicalize()?;
    let imports = store.base.join("imports");
    if journal_path.parent() != Some(imports.as_path()) {
        bail!("恢复日志不属于当前 cslim 根目录")
    }
    let mut journal: Journal = serde_json::from_slice(&fs::read(&journal_path)?)?;
    if journal.format != "rust-v1"
        || !valid_id(&journal.id)
        || journal.source.file_name().is_none_or(|n| n != "target")
        || journal.destination != store.entry_dir("targets", &journal.id).join("artifacts")
        || journal.quarantine.parent() != journal.source.parent()
        || journal.quarantine.file_name().is_none_or(|n| {
            n.to_string_lossy()
                != format!(
                    "{PREFIX}{}",
                    journal_path.file_stem().unwrap().to_string_lossy()
                )
        })
    {
        bail!("恢复日志路径不符合边界")
    }
    let _lock = store
        .lock("targets", &journal.id, true, false)?
        .context("target 正在使用")?;
    let entry = journal.destination.parent().unwrap();
    if entry.join("entry.json").exists() {
        if journal.quarantine.exists() {
            let _cargo = cargo_locks(&journal.quarantine)?;
            if snapshot(&journal.quarantine, false)?.signature != journal.source_signature {
                bail!("旧暂存数据有变化")
            }
            refcheck(&[journal.quarantine.clone()])?;
            fs::remove_dir_all(&journal.quarantine)?;
        }
        journal.state = "complete".into();
    } else if journal.source.exists() {
        if journal.quarantine.exists() || journal.destination.exists() {
            bail!("原路径已被重建且迁移副本仍在，保留两份数据以供人工检查")
        }
        if entry.exists() {
            fs::remove_dir_all(entry)?;
        }
        journal.state = "rolled_back".into();
    } else {
        let origin = if journal.quarantine.exists() {
            &journal.quarantine
        } else {
            &journal.destination
        };
        if !origin.exists() {
            bail!("没有可恢复副本")
        }
        let marker: serde_json::Value =
            serde_json::from_slice(&fs::read(entry.join("importing.json"))?)?;
        if marker["journal"].as_str() != Some(journal_path.to_string_lossy().as_ref()) {
            bail!("迁移归属标记不匹配")
        }
        let _cargo = cargo_locks(origin)?;
        refcheck(&[origin.clone(), journal.source.clone()])?;
        if origin == &journal.quarantine
            && snapshot(origin, false)?.signature != journal.source_signature
        {
            bail!("暂存数据有变化")
        }
        fs::rename(origin, &journal.source)?;
        fs::set_permissions(&journal.source, fs::Permissions::from_mode(journal.mode))?;
        if entry.exists() {
            fs::remove_dir_all(entry)?;
        }
        journal.state = "rolled_back".into();
    }
    journal_write(&journal_path, &journal)?;
    println!("{}", journal.state);
    Ok(())
}

#[derive(Serialize)]
struct ReportRow {
    source: PathBuf,
    manifest: PathBuf,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    destination: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}
#[derive(Serialize)]
struct Report {
    root: PathBuf,
    started: u64,
    mode: String,
    min_idle_hours: u64,
    worktrees: BTreeMap<String, serde_json::Value>,
    targets: Vec<ReportRow>,
    imported: Vec<serde_json::Value>,
    finished: Option<u64>,
}
fn used_bytes(store: &Store) -> Result<u64> {
    let mut total = 0u64;
    for item in fs::read_dir(store.base.join("targets"))? {
        let item = item?;
        total = total.saturating_add(dir_size(&item.path())?);
    }
    Ok(total)
}
pub(super) fn run(
    store: &Store,
    cfg: &Config,
    projects: &[String],
    apply: bool,
    min_idle_hours: u64,
    report: Option<&Path>,
    recovery: Option<&Path>,
) -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("历史 target 迁移仅支持 Linux")
    }
    if let Some(journal) = recovery {
        return recover(store, journal, &no_references);
    }
    let report_path = report.context("请用 --report 指定报告文件")?;
    if !report_path.is_absolute() {
        bail!("报告路径必须为绝对路径")
    }
    fs::create_dir_all(report_path.parent().context("报告目录不存在")?)?;
    let report_parent = report_path.parent().unwrap().canonicalize()?;
    if report_parent.starts_with(cfg.root.canonicalize()?) {
        bail!("报告不能写入 cslim 托管根目录")
    }
    let selected: Vec<String> = if projects.is_empty() {
        cfg.projects.keys().cloned().collect()
    } else {
        projects.to_vec()
    };
    let mut found = BTreeMap::<PathBuf, PathBuf>::new();
    let mut worktree_counts = BTreeMap::new();
    for name in selected {
        let entries = worktrees(cfg, &name)?;
        worktree_counts.insert(name, serde_json::json!({"registered":entries.len(),"present":entries.iter().filter(|e| e.available).count()}));
        for entry in entries {
            if !entry.available || !entry.manifest_exists {
                continue;
            }
            let source = entry.manifest.parent().unwrap().join("target");
            if fs::symlink_metadata(&source).is_ok() {
                found.entry(source).or_insert(entry.manifest);
            }
        }
    }
    for source in found.keys() {
        if report_parent.starts_with(source.canonicalize()?) {
            bail!("报告不能写入待迁移的 target")
        }
    }
    let limit = cfg.target_max_gib.saturating_mul(1 << 30);
    let mut used = used_bytes(store)?;
    let mut result = Report {
        root: cfg.root.clone(),
        started: now(),
        mode: if apply { "apply" } else { "dry-run" }.into(),
        min_idle_hours,
        worktrees: worktree_counts,
        targets: vec![],
        imported: vec![],
        finished: None,
    };
    for (source, manifest) in found {
        let mut row = ReportRow {
            source: source.clone(),
            manifest: manifest.clone(),
            status: "kept".into(),
            bytes: None,
            destination: None,
            reason: None,
        };
        let outcome = (|| -> Result<()> {
            require_markers(&source)?;
            require_untracked(&source)?;
            let data = snapshot(&source, false)?;
            row.bytes = Some(data.bytes);
            if now().saturating_sub(data.newest) < min_idle_hours.saturating_mul(3600) {
                bail!("近期有文件变化")
            }
            let old_dir = env::current_dir()?;
            env::set_current_dir(manifest.parent().unwrap())?;
            let context = build_context(
                &["--manifest-path".into(), manifest.display().to_string()],
                None,
                cfg,
            );
            env::set_current_dir(old_dir)?;
            let context = context?;
            let dest = store
                .entry_dir("targets", &context.target_id)
                .join("artifacts");
            row.destination = Some(dest.clone());
            if fs::symlink_metadata(dest.parent().unwrap()).is_ok() {
                bail!("已有托管目录或未完成迁移")
            }
            if limit > 0 && used.saturating_add(data.bytes) > limit {
                bail!("超出 target 总容量预算")
            }
            let locks = cargo_locks(&source)?;
            no_references(std::slice::from_ref(&source))?;
            drop(locks);
            row.status = "eligible".into();
            if apply {
                let budget = if limit == 0 {
                    0
                } else {
                    limit.saturating_sub(used)
                };
                let imported = move_target(
                    store,
                    &source,
                    &manifest,
                    cfg,
                    MoveOptions {
                        min_idle_hours,
                        budget,
                        reserve: RESERVE,
                        copy_always: false,
                    },
                    &no_references,
                )?;
                result.imported.push(imported);
                row.status = "imported".into();
            }
            used = used.saturating_add(data.bytes);
            Ok(())
        })();
        if let Err(e) = outcome {
            row.reason = Some(format!("{e:#}"));
        }
        eprintln!(
            "{} {} {}",
            row.status,
            human(row.bytes.unwrap_or(0)),
            source.display()
        );
        result.targets.push(row);
        write_json(report_path, &result)?;
    }
    result.finished = Some(now());
    write_json(report_path, &result)?;
    println!(
        "{}",
        serde_json::json!({"found":result.targets.len(),"imported":result.imported.len(),"report":report_path})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    static CWD_LOCK: Mutex<()> = Mutex::new(());

    fn fixture() -> (tempfile::TempDir, Store, Config, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        assert!(
            Command::new("git")
                .arg("init")
                .arg("-q")
                .arg(&repo)
                .status()
                .unwrap()
                .success()
        );
        let manifest = repo.join("Cargo.toml");
        fs::write(
            &manifest,
            "[package]\nname='migration-fixture'\nversion='0.1.0'\nedition='2024'\n",
        )
        .unwrap();
        fs::create_dir(repo.join("src")).unwrap();
        fs::write(repo.join("src/lib.rs"), "pub fn value() -> u8 { 1 }\n").unwrap();
        let source = repo.join("target");
        fs::create_dir_all(source.join("debug/.fingerprint")).unwrap();
        fs::write(source.join("debug/.cargo-lock"), "").unwrap();
        fs::write(source.join("debug/output"), "valuable rebuildable data").unwrap();
        let sccache = temp.path().join("sccache");
        fs::write(&sccache, "#!/bin/sh\necho 'sccache test-version'\n").unwrap();
        fs::set_permissions(&sccache, fs::Permissions::from_mode(0o755)).unwrap();
        let cfg = Config {
            root: temp.path().join("managed"),
            sccache,
            ..Config::default()
        };
        let store = Store::open(&cfg.root).unwrap();
        (temp, store, cfg, source, manifest)
    }
    fn no_refs(_: &[PathBuf]) -> Result<()> {
        Ok(())
    }

    #[test]
    fn imports_same_filesystem_and_records_ownership() {
        let _guard = CWD_LOCK.lock().unwrap();
        let (_tmp, store, cfg, source, manifest) = fixture();
        let imported = move_target(
            &store,
            &source,
            &manifest,
            &cfg,
            MoveOptions {
                min_idle_hours: 0,
                budget: 0,
                reserve: 0,
                copy_always: false,
            },
            &no_refs,
        )
        .unwrap();
        let destination = PathBuf::from(imported["destination"].as_str().unwrap());
        assert_eq!(
            fs::read_to_string(destination.join("debug/output")).unwrap(),
            "valuable rebuildable data"
        );
        assert!(!source.exists());
        assert!(!imported["copy_verified"].as_bool().unwrap());
        assert_eq!(
            store
                .read_entry("targets", destination.parent().unwrap())
                .unwrap()
                .kind,
            "targets"
        );
        let journal: Journal =
            serde_json::from_slice(&fs::read(imported["journal"].as_str().unwrap()).unwrap())
                .unwrap();
        assert_eq!(journal.state, "complete");
    }
    #[test]
    fn verified_copy_preserves_symlinks() {
        let _guard = CWD_LOCK.lock().unwrap();
        let (tmp, store, cfg, source, manifest) = fixture();
        fs::write(tmp.path().join("outside"), "outside").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("outside"), source.join("debug/link")).unwrap();
        let imported = move_target(
            &store,
            &source,
            &manifest,
            &cfg,
            MoveOptions {
                min_idle_hours: 0,
                budget: 0,
                reserve: 0,
                copy_always: true,
            },
            &no_refs,
        )
        .unwrap();
        let destination = PathBuf::from(imported["destination"].as_str().unwrap());
        assert!(destination.join("debug/link").is_symlink());
        assert!(imported["copy_verified"].as_bool().unwrap());
        assert!(!source.exists());
    }
    #[test]
    fn reference_after_quarantine_restores_source() {
        let _guard = CWD_LOCK.lock().unwrap();
        let (_tmp, store, cfg, source, manifest) = fixture();
        let calls = std::cell::Cell::new(0);
        let check = |_: &[PathBuf]| -> Result<()> {
            calls.set(calls.get() + 1);
            if calls.get() >= 2 {
                bail!("simulated new reference")
            }
            Ok(())
        };
        assert!(
            move_target(
                &store,
                &source,
                &manifest,
                &cfg,
                MoveOptions {
                    min_idle_hours: 0,
                    budget: 0,
                    reserve: 0,
                    copy_always: false
                },
                &check
            )
            .is_err()
        );
        assert_eq!(
            fs::read_to_string(source.join("debug/output")).unwrap(),
            "valuable rebuildable data"
        );
        assert!(
            fs::read_dir(store.base.join("targets"))
                .unwrap()
                .next()
                .is_none()
        );
    }
    #[test]
    fn recovery_restores_quarantined_source() {
        let _guard = CWD_LOCK.lock().unwrap();
        let (_tmp, store, cfg, source, manifest) = fixture();
        let old = env::current_dir().unwrap();
        env::set_current_dir(manifest.parent().unwrap()).unwrap();
        let context = build_context(
            &["--manifest-path".into(), manifest.display().to_string()],
            None,
            &cfg,
        )
        .unwrap();
        env::set_current_dir(old).unwrap();
        let destination = store
            .entry_dir("targets", &context.target_id)
            .join("artifacts");
        let imports = store.base.join("imports");
        private_dir(&imports).unwrap();
        let id = random_id().unwrap();
        let journal_path = imports.join(format!("{id}.json"));
        let quarantine = source.with_file_name(format!("{PREFIX}{id}"));
        let before = snapshot(&source, false).unwrap();
        let journal = Journal {
            format: "rust-v1".into(),
            source: source.clone(),
            quarantine: quarantine.clone(),
            destination: destination.clone(),
            id: context.target_id,
            started: now(),
            state: "quarantined".into(),
            source_signature: before.signature,
            bytes: before.bytes,
            mode: 0o755,
            error: None,
        };
        journal_write(&journal_path, &journal).unwrap();
        private_dir(destination.parent().unwrap()).unwrap();
        write_json(
            &destination.parent().unwrap().join("importing.json"),
            &serde_json::json!({"journal":journal_path}),
        )
        .unwrap();
        fs::rename(&source, &quarantine).unwrap();
        recover(&store, &journal_path, &no_refs).unwrap();
        assert_eq!(
            fs::read_to_string(source.join("debug/output")).unwrap(),
            "valuable rebuildable data"
        );
        assert!(!destination.parent().unwrap().exists());
    }
    #[test]
    fn timed_probe_fails_closed() {
        let child = Command::new("sleep")
            .arg("2")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        assert!(wait_output_timeout(child, 1).is_err());
    }
}
