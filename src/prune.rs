use anyhow::{Context, Result};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ArtifactKind {
    IntegrationTestRunner,
    Binary,
    Library,
}

#[derive(Clone, Debug)]
pub struct ArtifactFile {
    pub path: PathBuf,
    pub file_name: String,
    pub size: u64,
    pub mtime: SystemTime,
}

pub struct VersionGroup {
    pub files: Vec<ArtifactFile>,
    pub latest_mtime: SystemTime,
}

#[derive(Default, Debug)]
pub struct PrunePlan {
    pub test_runner_files: Vec<ArtifactFile>,
    pub old_version_files: Vec<ArtifactFile>,
}

impl PrunePlan {
    pub fn is_empty(&self) -> bool {
        self.test_runner_files.is_empty() && self.old_version_files.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct PruneOptions {
    pub keep: usize,
    pub clean_tests: bool,
    pub apply: bool,
}

#[derive(Default, Debug, Clone)]
pub struct PruneStats {
    pub test_runners_removed: usize,
    pub test_runner_bytes: u64,
    pub old_artifacts_removed: usize,
    pub old_artifact_bytes: u64,
    pub total_files_removed: usize,
    pub total_bytes_reclaimed: u64,
}

impl PruneStats {
    pub fn merge(&mut self, other: &PruneStats) {
        self.test_runners_removed += other.test_runners_removed;
        self.test_runner_bytes = self
            .test_runner_bytes
            .saturating_add(other.test_runner_bytes);
        self.old_artifacts_removed += other.old_artifacts_removed;
        self.old_artifact_bytes = self
            .old_artifact_bytes
            .saturating_add(other.old_artifact_bytes);
        self.total_files_removed += other.total_files_removed;
        self.total_bytes_reclaimed = self
            .total_bytes_reclaimed
            .saturating_add(other.total_bytes_reclaimed);
    }
}

/// 解析 Cargo 构建产物文件名
/// 返回 (raw_prefix, hash, ext)
/// 例如: "libanyhow-d88ec6cf81321b2b.rlib" -> ("libanyhow", "d88ec6cf81321b2b", ".rlib")
///       "doctor-dc754e24bfdff9b2" -> ("doctor", "dc754e24bfdff9b2", "")
///       "cargo_cslim-1cdb42e9821a6f7d.d" -> ("cargo_cslim", "1cdb42e9821a6f7d", ".d")
pub fn parse_artifact_filename(file_name: &str) -> Option<(&str, &str, &str)> {
    let (stem, ext) = match file_name.find('.') {
        Some(idx) => (&file_name[..idx], &file_name[idx..]),
        None => (file_name, ""),
    };
    let dash_idx = stem.rfind('-')?;
    let hash = &stem[dash_idx + 1..];
    if hash.len() != 16 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let raw_prefix = &stem[..dash_idx];
    if raw_prefix.is_empty() {
        return None;
    }
    Some((raw_prefix, hash, ext))
}

/// 检查 .d 文件是否包含由 tests/*.rs 生成的集成测试特征
pub fn is_integration_test_d_file(content: &str) -> bool {
    content.split_whitespace().any(|word| {
        let token = word.trim_end_matches(':');
        if !token.ends_with(".rs") {
            return false;
        }
        if token.starts_with("tests/") {
            return true;
        }
        if let Some(idx) = token.find("/tests/") {
            return !token[idx + 7..].is_empty();
        }
        false
    })
}

/// 检查文件是否为 Linux ELF 可执行文件
pub fn is_elf_executable(path: &Path) -> bool {
    let Ok(m) = fs::symlink_metadata(path) else {
        return false;
    };
    if !m.is_file() || m.mode() & 0o111 == 0 {
        return false;
    }
    let Ok(mut f) = fs::File::open(path) else {
        return false;
    };
    use std::io::Read;
    let mut magic = [0u8; 4];
    if f.read_exact(&mut magic).is_ok() {
        magic == [0x7f, b'E', b'L', b'F']
    } else {
        false
    }
}

/// 查找 target 目录下所有的 deps 目录（如 debug/deps, release/deps 等）
pub fn find_deps_dirs(target_dir: &Path) -> Vec<PathBuf> {
    let mut deps_dirs = Vec::new();
    find_deps_dirs_recursive(target_dir, &mut deps_dirs);
    deps_dirs
}

fn find_deps_dirs_recursive(dir: &Path, acc: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(m) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !m.is_dir() || m.file_type().is_symlink() {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name == "deps" {
                acc.push(path);
                continue;
            }
            if name == ".git" || name == "incremental" {
                continue;
            }
            find_deps_dirs_recursive(&path, acc);
        }
    }
}

/// 为单个 deps 目录生成裁剪计划
pub fn plan_deps_dir(deps_dir: &Path, opts: &PruneOptions) -> Result<PrunePlan> {
    let mut raw_files = Vec::new();
    let entries = match fs::read_dir(deps_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(PrunePlan::default()),
        Err(e) => {
            return Err(e).with_context(|| format!("读取 deps 目录失败: {}", deps_dir.display()));
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(m) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !m.is_file() || m.file_type().is_symlink() {
            continue;
        }
        let Some(file_name) = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(ToOwned::to_owned)
        else {
            continue;
        };
        let size = m.blocks().saturating_mul(512);
        let mtime = m.modified().unwrap_or(UNIX_EPOCH);
        raw_files.push(ArtifactFile {
            path,
            file_name,
            size,
            mtime,
        });
    }

    // 建立 (stem, hash) 到文件和 .d 信息的映射
    // 找出所有集成测试 runner 对应的前缀和 hash
    let mut integration_test_hashes = std::collections::BTreeSet::new();

    for file in &raw_files {
        if file.file_name.ends_with(".d")
            && let Some((raw_prefix, hash, _ext)) = parse_artifact_filename(&file.file_name)
        {
            // 检查同目录下是否存在同名同 hash 的二进制
            let bin_name = format!("{raw_prefix}-{hash}");
            let bin_path = deps_dir.join(&bin_name);
            if is_elf_executable(&bin_path)
                && let Ok(content) = fs::read_to_string(&file.path)
                && is_integration_test_d_file(&content)
            {
                integration_test_hashes.insert((raw_prefix.to_owned(), hash.to_owned()));
            }
        }
    }

    // 分类并聚合各版本产物
    // key: (ArtifactKind, normalized_name) -> BTreeMap<hash, VersionGroup>
    let mut groups: BTreeMap<(ArtifactKind, String), BTreeMap<String, VersionGroup>> =
        BTreeMap::new();

    for file in raw_files {
        let Some((raw_prefix, hash, ext)) = parse_artifact_filename(&file.file_name) else {
            continue;
        };

        let is_int_test =
            integration_test_hashes.contains(&(raw_prefix.to_owned(), hash.to_owned()));

        let (kind, norm_name) = if is_int_test {
            (ArtifactKind::IntegrationTestRunner, raw_prefix.to_owned())
        } else if ext.is_empty() && is_elf_executable(&file.path) {
            (ArtifactKind::Binary, raw_prefix.to_owned())
        } else {
            // 库产物（.rlib, .rmeta, .so, 以及库对应的 .d）
            let norm = if raw_prefix.starts_with("lib") && raw_prefix.len() > 3 {
                &raw_prefix[3..]
            } else {
                raw_prefix
            };
            (ArtifactKind::Library, norm.to_owned())
        };

        let kind_groups = groups.entry((kind, norm_name)).or_default();
        let version = kind_groups
            .entry(hash.to_owned())
            .or_insert_with(|| VersionGroup {
                files: Vec::new(),
                latest_mtime: file.mtime,
            });
        if file.mtime > version.latest_mtime {
            version.latest_mtime = file.mtime;
        }
        version.files.push(file);
    }

    let mut plan = PrunePlan::default();

    for ((kind, _name), versions_map) in groups {
        if kind == ArtifactKind::IntegrationTestRunner && opts.clean_tests {
            // 集成测试 runner 二进制及其 .d 文件无条件清理
            for (_, group) in versions_map {
                plan.test_runner_files.extend(group.files);
            }
            continue;
        }

        // 普通构建产物或未开启 clean_tests 的测试 runner：
        // 按版本 mtime 从新到旧排序
        let mut versions: Vec<VersionGroup> = versions_map.into_values().collect();
        versions.sort_by_key(|a| std::cmp::Reverse(a.latest_mtime));

        // 保留最新的 opts.keep 个版本，淘汰多余的旧版本
        if versions.len() > opts.keep {
            for old_ver in versions.into_iter().skip(opts.keep) {
                plan.old_version_files.extend(old_ver.files);
            }
        }
    }

    Ok(plan)
}

/// 执行单 target 的 prune 计划并应用删除
pub fn prune_artifacts_dir(artifacts_dir: &Path, opts: &PruneOptions) -> Result<PruneStats> {
    let deps_dirs = find_deps_dirs(artifacts_dir);
    let mut stats = PruneStats::default();

    for deps_dir in deps_dirs {
        let plan = plan_deps_dir(&deps_dir, opts)?;
        if plan.is_empty() {
            continue;
        }

        let rel_deps = deps_dir
            .strip_prefix(artifacts_dir)
            .unwrap_or(&deps_dir)
            .display();

        if !plan.test_runner_files.is_empty() {
            let count = plan.test_runner_files.len();
            let bytes: u64 = plan.test_runner_files.iter().map(|f| f.size).sum();
            stats.test_runners_removed += count;
            stats.test_runner_bytes = stats.test_runner_bytes.saturating_add(bytes);
            stats.total_files_removed += count;
            stats.total_bytes_reclaimed = stats.total_bytes_reclaimed.saturating_add(bytes);

            if opts.apply {
                for file in &plan.test_runner_files {
                    let _ = fs::remove_file(&file.path);
                }
            }
            eprintln!(
                "  {} {} [测试 runner] {} 个文件, {}",
                if opts.apply { "清理" } else { "预览" },
                rel_deps,
                count,
                human(bytes)
            );
        }

        if !plan.old_version_files.is_empty() {
            let count = plan.old_version_files.len();
            let bytes: u64 = plan.old_version_files.iter().map(|f| f.size).sum();
            stats.old_artifacts_removed += count;
            stats.old_artifact_bytes = stats.old_artifact_bytes.saturating_add(bytes);
            stats.total_files_removed += count;
            stats.total_bytes_reclaimed = stats.total_bytes_reclaimed.saturating_add(bytes);

            if opts.apply {
                for file in &plan.old_version_files {
                    let _ = fs::remove_file(&file.path);
                }
            }
            eprintln!(
                "  {} {} [旧版本产物] {} 个文件, {}",
                if opts.apply { "清理" } else { "预览" },
                rel_deps,
                count,
                human(bytes)
            );
        }
    }

    Ok(stats)
}

fn human(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{:.2} GiB", bytes as f64 / (1u64 << 30) as f64)
    } else {
        format!("{:.2} MiB", bytes as f64 / (1u64 << 20) as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;
    use tempfile::TempDir;

    #[test]
    fn parse_filenames() {
        assert_eq!(
            parse_artifact_filename("libanyhow-d88ec6cf81321b2b.rlib"),
            Some(("libanyhow", "d88ec6cf81321b2b", ".rlib"))
        );
        assert_eq!(
            parse_artifact_filename("doctor-dc754e24bfdff9b2"),
            Some(("doctor", "dc754e24bfdff9b2", ""))
        );
        assert_eq!(
            parse_artifact_filename("cargo_cslim-1cdb42e9821a6f7d.d"),
            Some(("cargo_cslim", "1cdb42e9821a6f7d", ".d"))
        );
        assert_eq!(
            parse_artifact_filename("libfoo-bar-0123456789abcdef.so"),
            Some(("libfoo-bar", "0123456789abcdef", ".so"))
        );
        // Invalid hashes or format
        assert_eq!(parse_artifact_filename("not-a-hash.rlib"), None);
        assert_eq!(parse_artifact_filename("short-1234.rlib"), None);
        assert_eq!(parse_artifact_filename("no-dash"), None);
    }

    #[test]
    fn detects_integration_test_d_files() {
        let content_test =
            "/path/to/target/debug/deps/doctor-123.d: tests/doctor.rs\n\ntests/doctor.rs:\n";
        assert!(is_integration_test_d_file(content_test));

        let content_abs_test = "/path/to/deps/test-123.d: /home/user/project/tests/sub/mod.rs\n";
        assert!(is_integration_test_d_file(content_abs_test));

        let content_src = "/path/to/target/debug/deps/main-123.d: src/main.rs src/lib.rs\n";
        assert!(!is_integration_test_d_file(content_src));
    }

    #[test]
    fn prune_old_versions_and_clean_tests() {
        let tmp = TempDir::new().unwrap();
        let deps = tmp.path().join("debug/deps");
        fs::create_dir_all(&deps).unwrap();

        // 1. 创建两个版本的 libfoo (hash1 旧，hash2 新)
        let f1_rlib = deps.join("libfoo-1111111111111111.rlib");
        let f1_rmeta = deps.join("libfoo-1111111111111111.rmeta");
        let f1_d = deps.join("foo-1111111111111111.d");
        fs::write(&f1_rlib, "old-rlib").unwrap();
        fs::write(&f1_rmeta, "old-rmeta").unwrap();
        fs::write(&f1_d, "old-d").unwrap();

        // 人为拉开修改时间
        std::thread::sleep(Duration::from_millis(10));

        let f2_rlib = deps.join("libfoo-2222222222222222.rlib");
        let f2_rmeta = deps.join("libfoo-2222222222222222.rmeta");
        let f2_d = deps.join("foo-2222222222222222.d");
        fs::write(&f2_rlib, "new-rlib").unwrap();
        fs::write(&f2_rmeta, "new-rmeta").unwrap();
        fs::write(&f2_d, "new-d").unwrap();

        // 2. 创建一个集成测试 runner
        let test_bin = deps.join("test_runner-3333333333333333");
        let test_d = deps.join("test_runner-3333333333333333.d");
        {
            let mut f = File::create(&test_bin).unwrap();
            use std::io::Write;
            // 写入 ELF magic + padding
            f.write_all(&[0x7f, b'E', b'L', b'F', 1, 2, 3, 4]).unwrap();
            let mut perms = f.metadata().unwrap().permissions();
            perms.set_mode(0o755);
            f.set_permissions(perms).unwrap();
        }
        fs::write(&test_d, "deps: tests/my_test.rs\n").unwrap();

        let opts_preview = PruneOptions {
            keep: 1,
            clean_tests: true,
            apply: false,
        };

        // 预览模式：不删除文件
        let plan = plan_deps_dir(&deps, &opts_preview).unwrap();
        assert_eq!(plan.old_version_files.len(), 3); // f1 相关的 3 个文件
        assert_eq!(plan.test_runner_files.len(), 2); // test_bin 和 test_d

        let stats = prune_artifacts_dir(tmp.path(), &opts_preview).unwrap();
        assert_eq!(stats.old_artifacts_removed, 3);
        assert_eq!(stats.test_runners_removed, 2);
        assert!(f1_rlib.exists());
        assert!(test_bin.exists());

        // 执行模式：真正删除
        let opts_apply = PruneOptions {
            keep: 1,
            clean_tests: true,
            apply: true,
        };
        let stats_apply = prune_artifacts_dir(tmp.path(), &opts_apply).unwrap();
        assert_eq!(stats_apply.total_files_removed, 5);
        assert!(!f1_rlib.exists());
        assert!(!f1_rmeta.exists());
        assert!(!f1_d.exists());
        assert!(!test_bin.exists());
        assert!(!test_d.exists());

        // 新版本依然存在
        assert!(f2_rlib.exists());
        assert!(f2_rmeta.exists());
        assert!(f2_d.exists());
    }

    #[test]
    fn keep_parameter_honored() {
        let tmp = TempDir::new().unwrap();
        let deps = tmp.path().join("debug/deps");
        fs::create_dir_all(&deps).unwrap();

        // 创建 3 个版本的 libbar
        for (i, hash) in ["aaaa1111aaaa1111", "bbbb2222bbbb2222", "cccc3333cccc3333"]
            .iter()
            .enumerate()
        {
            let rlib = deps.join(format!("libbar-{hash}.rlib"));
            fs::write(&rlib, format!("ver {i}")).unwrap();
            std::thread::sleep(Duration::from_millis(15));
        }

        // keep 2: 只删最旧的那 1 个
        let opts = PruneOptions {
            keep: 2,
            clean_tests: true,
            apply: true,
        };
        let stats = prune_artifacts_dir(tmp.path(), &opts).unwrap();
        assert_eq!(stats.old_artifacts_removed, 1);
        assert!(!deps.join("libbar-aaaa1111aaaa1111.rlib").exists());
        assert!(deps.join("libbar-bbbb2222bbbb2222.rlib").exists());
        assert!(deps.join("libbar-cccc3333cccc3333.rlib").exists());
    }
}
