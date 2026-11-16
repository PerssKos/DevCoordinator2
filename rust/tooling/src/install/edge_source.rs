//! Source-owned edge repair and the exact source retained for pre-admission rollback.
use super::*;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedSource {
    pub schema: u8,
    pub repository: PathBuf,
    pub checkout: PathBuf,
    pub source_commit: String,
    pub source_sha256: String,
}

#[derive(Clone, Debug)]
pub struct RepairConfig {
    pub installed_manifest: PathBuf,
    pub cache_root: PathBuf,
    pub edge_env: PathBuf,
    pub edge_unit_path: PathBuf,
    pub edge_unit: String,
    pub edge_account: String,
    pub runtime_dir: PathBuf,
    pub systemctl: PathBuf,
}
impl Default for RepairConfig {
    fn default() -> Self {
        Self {
            installed_manifest: "/etc/devcoordinator2/install-manifest.json".into(),
            cache_root: "/var/lib/devcoordinator2-sources".into(),
            edge_env: "/etc/devcoordinator2/edge.env".into(),
            edge_unit_path: "/etc/systemd/system/devcoordinator2-edge.service".into(),
            edge_unit: "devcoordinator2-edge.service".into(),
            edge_account: "devcoordinator2-edge".into(),
            runtime_dir: "/run/devcoordinator2".into(),
            systemctl: "/usr/bin/systemctl".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RepairReceipt {
    pub status: &'static str,
    pub source_commit: String,
    pub source_checkout: PathBuf,
    pub source_sha256: String,
    pub restarted_unit: String,
}

fn physical(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err("edge source paths must be exact absolute paths".into());
    }
    let resolved = path
        .canonicalize()
        .map_err(|_| "edge source checkout is unavailable")?;
    if resolved != path {
        return Err("edge source path traverses an alias".into());
    }
    Ok(resolved)
}

fn source_hash<R: CommandRunner + ?Sized>(
    checkout: &Path,
    commit: &str,
    runner: &R,
) -> Result<String, String> {
    let tree = runner.source_catalog(&CommandRequest {
        program: "/usr/bin/git".into(),
        args: [
            "-c",
            "safe.directory=*",
            "-C",
            path_text(checkout)?.as_str(),
            "ls-tree",
            "-r",
            "-z",
            "--full-tree",
            commit,
            "--",
            "edge",
            "console",
        ]
        .map(OsString::from)
        .into(),
        environment: BTreeMap::from([(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))]),
        clear_environment: true,
    })?;
    if !tree.success
        || tree.stdout_truncated
        || tree.stderr_truncated
        || tree.stdout.len() > 1024 * 1024
    {
        return Err("edge source object catalog exceeds its verification boundary".into());
    }
    let mut files = Vec::new();
    for entry in tree.stdout.split('\0').filter(|entry| !entry.is_empty()) {
        if files.len() >= 4096 {
            return Err("edge source object catalog exceeds 4096 files".into());
        }
        let (metadata, relative) = entry
            .split_once('\t')
            .ok_or("edge source tree entry is invalid")?;
        let parts = metadata.split_whitespace().collect::<Vec<_>>();
        let path = Path::new(relative);
        if parts.len() != 3
            || !matches!(parts[0], "100644" | "100755")
            || parts[1] != "blob"
            || path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
            || relative.contains(['\n', '\r'])
            || relative.len() > 4096
        {
            return Err("edge source tree contains an unsupported entry".into());
        }
        let target = checkout.join(path);
        validate_regular(&target, false)?;
        if target
            .canonicalize()
            .map_err(|_| "edge source file is unavailable")?
            != target
        {
            return Err("edge source file traverses an alias".into());
        }
        files.push((relative.to_owned(), parts[2].to_owned()));
    }
    if files.is_empty()
        || !files
            .iter()
            .any(|(path, _)| path == "edge/devcoordinator2-edge.mjs")
        || !files.iter().any(|(path, _)| path == "console/index.html")
    {
        return Err("the prior commit has no complete edge and Console source".into());
    }
    let mut start = 0;
    while start < files.len() {
        let mut end = start;
        let mut argument_bytes = 0;
        while end < files.len()
            && end - start < 128
            && argument_bytes + files[end].0.len() < 32 * 1024
        {
            argument_bytes += files[end].0.len() + 1;
            end += 1;
        }
        let mut arguments = vec![
            OsString::from("hash-object"),
            OsString::from("--no-filters"),
            OsString::from("--"),
        ];
        arguments.extend(
            files[start..end]
                .iter()
                .map(|(path, _)| OsString::from(path)),
        );
        let objects = git(checkout, arguments, runner)?;
        if objects
            .lines()
            .ne(files[start..end].iter().map(|(_, object)| object.as_str()))
        {
            return Err("edge source bytes do not match the exact installed commit".into());
        }
        start = end;
    }
    let mut digest = Sha256::new();
    // Every object above was recomputed from the checkout bytes. Bind that
    // verified object manifest without reading large assets a second time.
    for (path, object) in files {
        digest.update(path.as_bytes());
        digest.update([0]);
        digest.update(object.as_bytes());
        digest.update([0]);
    }
    Ok(hex(&digest.finalize()))
}

pub fn verify<R: CommandRunner + ?Sized>(source: &PinnedSource, runner: &R) -> Result<(), String> {
    if source.schema != 1 {
        return Err("unsupported pinned edge source binding".into());
    }
    validate_commit(&source.source_commit)?;
    physical(&source.repository)?;
    physical(&source.checkout)?;
    validate_live_checkout(&source.repository, false, runner).map_err(|error| {
        format!("Console source binding is unverified; prior source was preserved: {error}")
    })?;
    if !git(&source.checkout, ["branch", "--show-current"], runner)?
        .trim()
        .is_empty()
        || git(&source.checkout, ["rev-parse", "HEAD"], runner)?.trim() != source.source_commit
        || !git(
            &source.checkout,
            ["status", "--porcelain", "--untracked-files=all"],
            runner,
        )?
        .trim()
        .is_empty()
    {
        return Err("pinned edge source must remain clean at its exact detached commit".into());
    }
    let common = |root: &Path| -> Result<PathBuf, String> {
        Path::new(
            git(
                root,
                ["rev-parse", "--path-format=absolute", "--git-common-dir"],
                runner,
            )?
            .trim(),
        )
        .canonicalize()
        .map_err(|_| "pinned source Git ownership is unavailable".into())
    };
    if common(&source.repository)? != common(&source.checkout)?
        || source_hash(&source.checkout, &source.source_commit, runner)? != source.source_sha256
    {
        return Err("pinned edge source no longer matches its canonical Git repository".into());
    }
    Ok(())
}

pub fn prepare<R: CommandRunner + ?Sized>(
    manifest: &InstallManifest,
    cache: &Path,
    owner: (u32, u32),
    runner: &R,
) -> Result<PinnedSource, String> {
    validate_commit(&manifest.source_commit)?;
    let repository = physical(Path::new(&manifest.source_root))?;
    validate_live_checkout(&repository, false, runner)?;
    if !cache.is_absolute()
        || cache
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err("pinned source directory must be exact and absolute".into());
    }
    if let Ok(metadata) = cache.symlink_metadata()
        && (metadata.file_type().is_symlink() || !metadata.is_dir() || metadata.uid() != owner.0)
    {
        return Err("pinned source directory is not owned by this installation".into());
    }
    ensure_owned_directory(cache, 0o711, owner)?;
    physical(cache)?;
    let checkout = cache.join(&manifest.source_commit);
    if !checkout.exists() {
        git(
            &repository,
            vec![
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("--detach"),
                OsString::from("--"),
                checkout.as_os_str().to_owned(),
                OsString::from(&manifest.source_commit),
            ],
            runner,
        )?;
    }
    physical(&checkout)?;
    let metadata = checkout
        .symlink_metadata()
        .map_err(|_| "pinned source checkout is unavailable")?;
    if !metadata.is_dir() || metadata.uid() != owner.0 {
        return Err("pinned source checkout is not installation-owned".into());
    }
    let pointer = checkout.join(".git");
    let metadata = validate_regular(&pointer, false)?;
    if metadata.uid() != owner.0 {
        return Err("pinned source Git pointer is not installation-owned".into());
    }
    std::fs::set_permissions(&pointer, std::fs::Permissions::from_mode(0o600))
        .map_err(|_| "cannot retain private Git ownership")?;
    std::fs::set_permissions(&checkout, std::fs::Permissions::from_mode(0o750))
        .map_err(|_| "cannot retain private source ownership")?;
    let source = PinnedSource {
        schema: 1,
        repository,
        source_sha256: source_hash(&checkout, &manifest.source_commit, runner)?,
        checkout,
        source_commit: manifest.source_commit.clone(),
    };
    verify(&source, runner)?;
    Ok(source)
}

pub fn bind<R: CommandRunner + ?Sized>(
    source: &PinnedSource,
    unit: &Path,
    environment: &Path,
    edge_account: &str,
    owner: u32,
    runner: &R,
) -> Result<(), String> {
    verify(source, runner)?;
    let metadata = validate_regular(unit, false)?;
    if metadata.uid() != owner {
        return Err("edge unit is not installation-owned".into());
    }
    let text = read_template(unit)?;
    let original = format!(
        "ExecStart=/usr/bin/node {}/edge/devcoordinator2-edge.mjs",
        source.repository.display()
    );
    let selected = format!(
        "ExecStart=/usr/bin/node {}/edge/devcoordinator2-edge.mjs",
        source.checkout.display()
    );
    let mut found = 0;
    let mut lines = Vec::new();
    for line in text.lines() {
        if line.starts_with("ExecStart=") {
            if line != original && line != selected {
                return Err("edge service script is outside the captured source binding".into());
            }
            found += 1;
            lines.push(selected.clone());
            lines.push(format!("ReadOnlyPaths={}", source.checkout.display()));
        } else if line != format!("ReadOnlyPaths={}", source.repository.display())
            && line != format!("ReadOnlyPaths={}", source.checkout.display())
        {
            lines.push(line.to_owned());
        }
    }
    if found != 1 {
        return Err("edge unit has no single exact source command".into());
    }
    ensure_edge_source_access(&source.checkout, edge_account, runner)?;
    let console = path_text(&source.checkout.join("console"))?;
    set_owned_edge_env_value(environment, "EDGE_CONSOLE_DIR", &console, owner)?;
    configure_edge_source_revision(environment, &source.source_commit, owner)?;
    atomic_file(
        unit,
        format!("{}\n", lines.join("\n")).as_bytes(),
        metadata.mode() & 0o777,
        Some((metadata.uid(), metadata.gid())),
    )?;
    verify(source, runner)
}

pub fn repair_host<R: CommandRunner>(
    config: &RepairConfig,
    runner: &R,
    owner: (u32, u32),
) -> Result<RepairReceipt, String> {
    if !config.edge_unit.ends_with(".service")
        || config.edge_unit.starts_with('-')
        || !config
            .edge_unit
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'@'))
    {
        return Err("edge repair requires one exact service unit".into());
    }
    let before = hash_file(&config.installed_manifest)?;
    let manifest =
        read_and_verify_prior_manifest_owned(&config.installed_manifest, runner, owner.0)?;
    let source = prepare(&manifest, &config.cache_root, owner, runner)?;
    let _guard = crate::cutover::edge_maintenance_guard(&config.runtime_dir)?;
    if hash_file(&config.installed_manifest)? != before {
        return Err("installation changed during edge source preparation".into());
    }
    bind(
        &source,
        &config.edge_unit_path,
        &config.edge_env,
        &config.edge_account,
        owner.0,
        runner,
    )?;
    run_required(
        runner,
        &config.systemctl,
        &["daemon-reload"],
        "reload exact edge source binding",
    )?;
    run_required(
        runner,
        &config.systemctl,
        &["restart", &config.edge_unit],
        "restart matching installed edge",
    )?;
    run_required(
        runner,
        &config.systemctl,
        &["is-active", "--quiet", &config.edge_unit],
        "verify matching installed edge",
    )?;
    read_and_verify_prior_manifest_owned(&config.installed_manifest, runner, owner.0)?;
    verify(&source, runner)?;
    Ok(RepairReceipt {
        status: "edge_source_repaired",
        source_commit: source.source_commit,
        source_checkout: source.checkout,
        source_sha256: source.source_sha256,
        restarted_unit: config.edge_unit.clone(),
    })
}
