//! The nearest existing installer fixtures had no certificate or live TLS
//! surface. These acceptance tests extend that boundary with a disposable CA,
//! the actual Node edge, a persistent upstream, and an exact-unit adapter.

use super::*;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::install::{BinaryReceipt, HostRunner};
use sha2::{Digest, Sha256};

const COMMIT: &str = "1111111111111111111111111111111111111111";
const HOSTS: &str = "DNS:example.test,DNS:*.example.test";

const SERVER: &str = r#"
const { pathToFileURL } = require('node:url');
const fs = require('node:fs');
(async () => {
  const [modulePath, configurationPath] = process.argv.slice(2);
  const { createEdge } = await import(pathToFileURL(modulePath));
  const config = JSON.parse(fs.readFileSync(configurationPath));
  const quiet = { info() {}, warn() {}, error() {}, debug() {} };
  const edge = await createEdge(config, { log: quiet });
  const [port] = await edge.listen();
  process.stdout.write(JSON.stringify({ port }) + '\n');
  process.on('SIGTERM', () => edge.close().then(() => process.exit(0)));
})().catch(error => { process.stderr.write(`fixture edge startup failed: ${error.code || error.name}: ${String(error.message).slice(0, 180)}\n`); process.exit(1); });
"#;

const UPSTREAM: &str = r#"
const http = require('node:http');
const server = http.createServer((req, res) => {
  res.writeHead(200, { 'content-type': 'text/plain' });
  res.end('persistent fixture application');
});
server.listen(0, '127.0.0.1', () => process.stdout.write(JSON.stringify({ port: server.address().port }) + '\n'));
process.on('SIGTERM', () => server.close(() => process.exit(0)));
"#;

const REQUESTS: &str = r#"
const fs = require('node:fs');
const https = require('node:https');
const { pathToFileURL } = require('node:url');
(async () => {
  const [caFile, port, sessionModule] = process.argv.slice(1);
  const { createSessionManager } = await import(pathToFileURL(sessionModule));
  const sessions = createSessionManager({ secret: 'fixture-session-secret-at-least-16-bytes',
    ttlMs: 60000, cookieName: 'dc2_session', cookieDomain: '.example.test', secure: true });
  const cookie = sessions.issue({ email: 'owner@example.test', sub: 'fixture-owner' }).cookie.split(';')[0];
  const get = (host, pathname, cookie) => new Promise((resolve, reject) => {
    const req = https.get({ hostname: '127.0.0.1', port, servername: host, path: pathname,
      ca: fs.readFileSync(caFile), agent: false, headers: { host, ...(cookie ? { cookie } : {}) } }, res => {
      const fingerprint = res.socket.getPeerCertificate().fingerprint256.replaceAll(':', '').toLowerCase();
      const chunks = []; res.on('data', c => chunks.push(c));
      res.on('end', () => resolve({ status: res.statusCode, fingerprint, body: Buffer.concat(chunks).toString() }));
    });
    req.on('error', reject); req.setTimeout(5000, () => req.destroy());
  });
  const publicRoute = await get('public.example.test', '/');
  const protectedAnon = await get('protected.example.test', '/');
  const protectedOwner = await get('protected.example.test', '/', cookie);
  const health = await get('console.example.test', '/healthz');
  const body = JSON.parse(health.body);
  process.stdout.write(JSON.stringify({ public_status: publicRoute.status,
    public_body_ok: publicRoute.body === 'persistent fixture application',
    protected_anonymous_status: protectedAnon.status,
    protected_owner_status: protectedOwner.status,
    protected_body_ok: protectedOwner.body === 'persistent fixture application',
    fingerprint: health.fingerprint, route_generation: body.route_generation, healthy: body.ok }));
})().catch(() => process.exit(1));
"#;

struct FixtureRunner {
    root: PathBuf,
    unit: PathBuf,
    module: PathBuf,
    configuration: PathBuf,
    ca: PathBuf,
    service: Mutex<Option<Child>>,
    restarts: AtomicUsize,
    fail_restart: AtomicBool,
    fail_all_restarts: AtomicBool,
    fail_probe: AtomicBool,
    dirty: AtomicBool,
    wrong_unit: AtomicBool,
}

impl FixtureRunner {
    fn start(&self) -> u16 {
        let mut child = Command::new("/usr/bin/node")
            .args([
                OsString::from("-e"),
                SERVER.into(),
                "fixture-runtime".into(),
                self.module.clone().into_os_string(),
                self.configuration.clone().into_os_string(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let port = startup_port(&mut child);
        *self.service.lock().unwrap() = Some(child);
        port
    }

    fn stop(&self) {
        if let Some(mut child) = self.service.lock().unwrap().take() {
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }
}

impl Drop for FixtureRunner {
    fn drop(&mut self) {
        self.stop();
    }
}

impl CommandRunner for FixtureRunner {
    fn run(&self, request: &CommandRequest) -> Result<CommandOutput, String> {
        let args = request
            .args
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        if request.program.file_name().and_then(|s| s.to_str()) == Some("git") {
            let output = if args.iter().any(|s| s == "--show-toplevel") {
                self.root.to_string_lossy().into_owned()
            } else if args.iter().any(|s| s == "--show-current") {
                "main".into()
            } else if args.iter().any(|s| s == "status") {
                if self.dirty.load(Ordering::SeqCst) {
                    " M fixture\n".into()
                } else {
                    String::new()
                }
            } else if args.iter().any(|s| s == "fetch") {
                String::new()
            } else {
                COMMIT.into()
            };
            return Ok(output_of(true, output));
        }
        if args == ["--source-commit"] {
            return Ok(output_of(true, COMMIT.into()));
        }
        if request.program == Path::new("/usr/bin/systemctl") {
            assert!(
                args.iter().any(|s| s == UNIT),
                "only the exact stable-edge service may be controlled"
            );
            match args[0].as_str() {
                "show" => {
                    return Ok(output_of(
                        true,
                        if self.wrong_unit.load(Ordering::SeqCst) {
                            "/different/service".into()
                        } else {
                            self.unit.to_string_lossy().into_owned()
                        },
                    ));
                }
                "is-active" => {
                    return Ok(output_of(
                        self.service.lock().unwrap().is_some(),
                        String::new(),
                    ));
                }
                "restart" => {
                    self.restarts.fetch_add(1, Ordering::SeqCst);
                    self.stop();
                    if self.fail_restart.swap(false, Ordering::SeqCst)
                        || self.fail_all_restarts.load(Ordering::SeqCst)
                    {
                        return Ok(output_of(
                            false,
                            "private failure detail must not escape".into(),
                        ));
                    }
                    self.start();
                    return Ok(output_of(true, String::new()));
                }
                _ => return Err("unexpected fixture service action".into()),
            }
        }
        if request.program == Path::new("/usr/bin/node") {
            if request.args.get(1).is_some_and(|arg| arg == PROBE)
                && self.fail_probe.swap(false, Ordering::SeqCst)
            {
                return Ok(CommandOutput {
                    success: false,
                    stdout: "sensitive output".into(),
                    stderr: "private failure detail must not escape".into(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                });
            }
            let mut request = request.clone();
            request.environment.insert(
                "NODE_EXTRA_CA_CERTS".into(),
                self.ca.clone().into_os_string(),
            );
            return HostRunner.run(&request);
        }
        Err("unexpected fixture command".into())
    }
}

fn output_of(success: bool, stdout: String) -> CommandOutput {
    CommandOutput {
        success,
        stdout,
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
    }
}

fn startup_port(child: &mut Child) -> u16 {
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    if line.is_empty() {
        let mut reason = String::new();
        if let Some(stderr) = child.stderr.as_mut() {
            stderr.take(2048).read_to_string(&mut reason).unwrap();
        }
        panic!("fixture listener failed before announcing its port: {reason}");
    }
    serde_json::from_str::<serde_json::Value>(&line).expect("fixture server must announce its port")
        ["port"]
        .as_u64()
        .unwrap() as u16
}

struct World {
    _temporary: tempfile::TempDir,
    layout: Layout,
    lineage: PathBuf,
    archive: PathBuf,
    ca_key: PathBuf,
    runner: FixtureRunner,
    upstream: Child,
    port: u16,
    routes: PathBuf,
    routes_before: Vec<u8>,
    old_cert: Vec<u8>,
    old_key: Vec<u8>,
}

impl Drop for World {
    fn drop(&mut self) {
        self.runner.stop();
        self.upstream.kill().unwrap();
        self.upstream.wait().unwrap();
    }
}

impl World {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().to_path_buf();
        let owner = (
            rustix::process::geteuid().as_raw(),
            rustix::process::getegid().as_raw(),
        );
        for directory in [
            "source/deploy",
            "source/edge",
            "etc/edge",
            "system",
            "cutover",
            "certbot/live/edge",
            "certbot/archive/edge",
            "certbot/renewal-hooks/deploy",
            "binaries",
            "edge-state",
        ] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
            std::fs::set_permissions(root.join(directory), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let layout = Layout {
            runtime_dir: root.join("runtime"),
            configuration: root.join("etc/edge/tls-renewal.json"),
            manifest: root.join("etc/install-manifest.json"),
            edge_env: root.join("etc/edge.env"),
            edge_unit: root.join("system/devcoordinator2-edge.service"),
            certificate: root.join("etc/edge/tls.crt"),
            key: root.join("etc/edge/tls.key"),
            hook: root.join("certbot/renewal-hooks/deploy/devcoordinator2-edge"),
            recovery: root.join("cutover/tls-renewal"),
            owner,
        };
        let source = root.join("source");
        write_private(
            &source.join("edge/devcoordinator2-edge.mjs"),
            b"// reviewed fixture source\n",
        );
        write_private(
            &source.join("deploy/devcoordinator2-edge.service"),
            include_bytes!("../../../deploy/devcoordinator2-edge.service"),
        );
        write_private(
            &layout.edge_unit,
            install::render_edge_unit(&source, false)
                .unwrap()
                .as_bytes(),
        );
        let receipts = [
            "devcoordinator2",
            "devcoordinator2-tooling",
            "devcoordinator2-executor",
        ]
        .iter()
        .map(|name| {
            let path = root.join("binaries").join(name);
            let bytes = format!("#!/bin/sh\n# fixture {name}\n");
            write_private(&path, bytes.as_bytes());
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            BinaryReceipt {
                name: (*name).into(),
                path: path.to_string_lossy().into_owned(),
                sha256: hex(&Sha256::digest(bytes.as_bytes())),
                bytes: bytes.len() as u64,
                source_commit: COMMIT.into(),
            }
        })
        .collect();
        let manifest =
            install::manifest(&source, COMMIT, "2026-10-09T00:00:00Z", receipts).unwrap();
        install::write_manifest(&layout.manifest, &manifest, owner).unwrap();
        let ca = root.join("ca.crt");
        let ca_key = root.join("ca.key");
        openssl(&[
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:P-256",
            "-noenc",
            "-subj",
            "/CN=Disposable renewal fixture CA",
            "-days",
            "365",
            "-keyout",
            text_path(&ca_key),
            "-out",
            text_path(&ca),
        ]);
        std::fs::set_permissions(&ca_key, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut upstream = Command::new("/usr/bin/node")
            .args(["-e", UPSTREAM])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let upstream_port = startup_port(&mut upstream);
        let routes = root.join("routes.json");
        let payload = serde_json::json!({"generation":42,"published_at":"2026-10-09T00:00:00Z","domain":"example.test",
            "routes":[{"deployment_id":"dpublic","domain":"public.example.test","port":upstream_port,"scheme":"http","auth":"public","lease_id":"lpublic"},
                      {"deployment_id":"dprotected","domain":"protected.example.test","port":upstream_port,"scheme":"http","auth":"authenticated","lease_id":"lprotected"}],
            "access":{"owners":["owner@example.test"],"grants":[]}});
        // serde_json's default sorted object map matches the edge's canonical JSON.
        let checksum = hex(&Sha256::digest(serde_json::to_vec(&payload).unwrap()));
        let mut document = payload;
        document["schema"] = 2.into();
        document["payload_sha256"] = checksum.into();
        let routes_before = serde_json::to_vec(&document).unwrap();
        write_private(&routes, &routes_before);
        let configuration = root.join("node-edge.json");
        let config = serde_json::json!({"baseDomain":"example.test","consoleHost":"console.example.test","httpsPort":0,"httpPort":0,"httpOnly":false,
            "tlsCert":layout.certificate,"tlsKey":layout.key,"sessionSecret":"fixture-session-secret-at-least-16-bytes",
            "oidcIssuer":"https://issuer.example.test","oidcClientId":"fixture-client","oidcClientSecret":"fixture-only-value",
            "routesFile":routes,"stateDir":root.join("edge-state"),"daemonSocket":root.join("absent-daemon.sock"),"consoleDir":""});
        write_private(&configuration, &serde_json::to_vec(&config).unwrap());
        let runner = FixtureRunner {
            root: source,
            unit: layout.edge_unit.clone(),
            module: Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../edge/devcoordinator2-edge.mjs")
                .canonicalize()
                .unwrap(),
            configuration,
            ca,
            service: Mutex::new(None),
            restarts: AtomicUsize::new(0),
            fail_restart: AtomicBool::new(false),
            fail_all_restarts: AtomicBool::new(false),
            fail_probe: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            wrong_unit: AtomicBool::new(false),
        };
        let lineage = root.join("certbot/live/edge");
        let archive = root.join("certbot/archive/edge");
        let mut world = Self {
            _temporary: temporary,
            layout,
            lineage,
            archive,
            ca_key,
            runner,
            upstream,
            port: 0,
            routes,
            routes_before,
            old_cert: Vec::new(),
            old_key: Vec::new(),
        };
        world.issue("old", 30, HOSTS, None);
        world.old_cert = std::fs::read(world.archive.join("old.crt")).unwrap();
        world.old_key = std::fs::read(world.archive.join("old.key")).unwrap();
        write_private(&world.layout.certificate, &world.old_cert);
        write_private(&world.layout.key, &world.old_key);
        world.issue("renewed", 60, HOSTS, None);
        world.select("renewed");
        world.port = world.runner.start();
        let mut config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&world.runner.configuration).unwrap()).unwrap();
        config["httpsPort"] = world.port.into();
        write_private(
            &world.runner.configuration,
            &serde_json::to_vec(&config).unwrap(),
        );
        write_private(
            &world.layout.edge_env,
            format!(
                "EDGE_BASE_DOMAIN=example.test\nEDGE_HTTPS_PORT={}\n",
                world.port
            )
            .as_bytes(),
        );
        world
    }

    fn issue(&self, name: &str, days: u32, hosts: &str, dates: Option<(&str, &str)>) {
        let key = self.archive.join(format!("{name}.key"));
        let csr = self.archive.join(format!("{name}.csr"));
        let cert = self.archive.join(format!("{name}.crt"));
        openssl(&[
            "req",
            "-new",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:P-256",
            "-noenc",
            "-subj",
            "/CN=Renewal fixture",
            "-keyout",
            text_path(&key),
            "-out",
            text_path(&csr),
        ]);
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        let extensions = self.archive.join(format!("{name}.ext"));
        write_private(&extensions, format!("subjectAltName={hosts}\n").as_bytes());
        let days = days.to_string();
        let mut args = vec![
            "x509",
            "-req",
            "-in",
            text_path(&csr),
            "-CA",
            text_path(&self.runner.ca),
            "-CAkey",
            text_path(&self.ca_key),
            "-set_serial",
            "1",
            "-days",
            &days,
            "-extfile",
            text_path(&extensions),
            "-out",
            text_path(&cert),
        ];
        if let Some((from, to)) = dates {
            args.extend(["-not_before", from, "-not_after", to]);
        }
        openssl(&args);
        std::fs::set_permissions(cert, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    fn select(&self, name: &str) {
        for (link, extension) in [("fullchain.pem", "crt"), ("privkey.pem", "key")] {
            let target = self.lineage.join(link);
            if target.symlink_metadata().is_ok() {
                std::fs::remove_file(&target).unwrap();
            }
            symlink(self.archive.join(format!("{name}.{extension}")), target).unwrap();
        }
    }

    fn configure(&self) {
        configure_at(&self.layout, &self.lineage, COMMIT, &self.runner).unwrap();
    }

    fn requests(&self) -> serde_json::Value {
        let session_module = self.runner.module.parent().unwrap().join("lib/session.mjs");
        let result = HostRunner
            .run(&CommandRequest {
                program: "/usr/bin/node".into(),
                args: vec![
                    "-e".into(),
                    REQUESTS.into(),
                    self.runner.ca.clone().into_os_string(),
                    self.port.to_string().into(),
                    session_module.into_os_string(),
                ],
                environment: BTreeMap::new(),
                clear_environment: true,
            })
            .unwrap();
        assert!(
            result.success,
            "normal TLS and route acceptance requests must complete"
        );
        serde_json::from_str(&result.stdout).unwrap()
    }

    fn assert_routes(&self) -> String {
        let observed = self.requests();
        assert_eq!(observed["public_status"], 200);
        assert_eq!(observed["public_body_ok"], true);
        assert_eq!(observed["protected_anonymous_status"], 302);
        assert_eq!(observed["protected_owner_status"], 200);
        assert_eq!(observed["protected_body_ok"], true);
        assert_eq!(observed["healthy"], true);
        assert_eq!(observed["route_generation"], 42);
        assert!(
            std::fs::read(&self.routes).unwrap() == self.routes_before,
            "route and access-policy bytes must be unchanged"
        );
        observed["fingerprint"].as_str().unwrap().into()
    }

    fn assert_previous_files(&self) {
        assert!(
            std::fs::read(&self.layout.certificate).unwrap() == self.old_cert,
            "previous certificate must remain installed"
        );
        assert!(
            std::fs::read(&self.layout.key).unwrap() == self.old_key,
            "previous private key must remain installed"
        );
    }

    fn assert_private_recovery(&self) {
        fn walk(path: &Path, uid: u32) {
            let meta = path.symlink_metadata().unwrap();
            assert_eq!(meta.uid(), uid);
            assert_eq!(meta.mode() & 0o077, 0);
            assert!(!meta.file_type().is_symlink());
            if meta.is_dir() {
                for entry in std::fs::read_dir(path).unwrap() {
                    walk(&entry.unwrap().path(), uid);
                }
            }
        }
        walk(&self.layout.recovery, self.layout.owner.0);
    }
}

fn openssl(args: &[&str]) {
    let output = Command::new("/usr/bin/openssl")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "disposable certificate fixture generation must succeed"
    );
}

fn write_private(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    std::io::Write::write_all(&mut file, bytes).unwrap();
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .unwrap();
}

fn text_path(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn renewal_event_reaches_real_edge_and_preserves_public_and_protected_routes() {
    let world = World::new();
    let before = world.assert_routes();
    world.configure();
    world.assert_previous_files();
    assert_eq!(
        world.assert_routes(),
        before,
        "registration must not restart or alter the service"
    );
    assert_eq!(std::fs::read_to_string(&world.layout.hook).unwrap(), HOOK);
    let receipt = deploy_at(&world.layout, &world.lineage, COMMIT, &world.runner).unwrap();
    assert_eq!(receipt.status, "renewed");
    assert!(receipt.recovery_retained);
    assert_ne!(
        world.assert_routes(),
        before,
        "renewed certificate must be served through normal TLS"
    );
    assert_eq!(world.runner.restarts.load(Ordering::SeqCst), 1);
    world.assert_private_recovery();
    let public = serde_json::to_string(&receipt).unwrap();
    assert!(
        !public.contains("example.test")
            && !public.contains("BEGIN")
            && !public.contains(text_path(world._temporary.path()))
    );
    let unchanged = deploy_at(&world.layout, &world.lineage, COMMIT, &world.runner).unwrap();
    assert_eq!(unchanged.status, "unchanged");
    assert_eq!(world.runner.restarts.load(Ordering::SeqCst), 1);
}

#[test]
fn invalid_key_pair_does_not_change_working_service() {
    let world = World::new();
    world.configure();
    let before = world.assert_routes();
    write_private(&world.archive.join("renewed.key"), &world.old_key);
    let error = deploy_at(&world.layout, &world.lineage, COMMIT, &world.runner)
        .err()
        .expect("mismatched key must be rejected");
    assert!(error.contains("pair, validity, or hostname coverage"));
    world.assert_previous_files();
    assert_eq!(world.assert_routes(), before);
    assert_eq!(world.runner.restarts.load(Ordering::SeqCst), 0);
    world.assert_private_recovery();
}

#[test]
fn invalid_certificate_classes_are_rejected_before_activation() {
    let world = World::new();
    world.configure();
    let before = world.assert_routes();
    let cases = [
        (
            "expired",
            60,
            HOSTS,
            Some(("20000101000000Z", "20010101000000Z")),
        ),
        (
            "future",
            60,
            HOSTS,
            Some(("20900101000000Z", "20910101000000Z")),
        ),
        (
            "missing-wildcard",
            60,
            "DNS:example.test,DNS:console.example.test",
            None,
        ),
        ("wrong-host", 60, "DNS:other.test,DNS:*.other.test", None),
        ("older-expiry", 1, HOSTS, None),
    ];
    let mut failures = Vec::new();
    for (name, days, hosts, dates) in cases {
        world.issue(name, days, hosts, dates);
        world.select(name);
        if deploy_at(&world.layout, &world.lineage, COMMIT, &world.runner).is_ok() {
            failures.push(name);
        }
        world.assert_previous_files();
        assert_eq!(world.assert_routes(), before);
    }
    assert!(
        failures.is_empty(),
        "every invalid certificate fixture must be rejected: {failures:?}"
    );
    assert_eq!(world.runner.restarts.load(Ordering::SeqCst), 0);
    world.assert_private_recovery();
}

#[test]
fn failed_restart_and_tls_verification_restore_private_pair_and_working_edge() {
    for fail_restart in [true, false] {
        let world = World::new();
        world.configure();
        let before = world.assert_routes();
        if fail_restart {
            world.runner.fail_restart.store(true, Ordering::SeqCst);
        } else {
            world.runner.fail_probe.store(true, Ordering::SeqCst);
        }
        let error = deploy_at(&world.layout, &world.lineage, COMMIT, &world.runner)
            .err()
            .expect("failed activation must return failure");
        assert!(error.contains("previous edge was restored"));
        assert!(
            !error.contains("private failure detail")
                && !error.contains("sensitive output")
                && !error.contains("BEGIN")
        );
        world.assert_previous_files();
        assert_eq!(world.assert_routes(), before);
        assert_eq!(world.runner.restarts.load(Ordering::SeqCst), 2);
        world.assert_private_recovery();
        let receipts = std::fs::read_dir(&world.layout.recovery)
            .unwrap()
            .filter_map(|e| {
                let path = e.unwrap().path().join("receipt.json");
                std::fs::read_to_string(path).ok()
            })
            .collect::<Vec<_>>();
        assert!(receipts.iter().any(|r| r.contains("rolled_back")));
    }
}

#[test]
fn unrelated_renewal_and_unreviewed_source_or_service_never_activate() {
    let world = World::new();
    world.configure();
    let unrelated = world._temporary.path().join("unrelated-lineage");
    std::fs::create_dir(&unrelated).unwrap();
    let ignored = deploy_at(&world.layout, &unrelated, COMMIT, &world.runner).unwrap();
    assert_eq!(ignored.status, "ignored");
    world.runner.dirty.store(true, Ordering::SeqCst);
    assert!(deploy_at(&world.layout, &world.lineage, COMMIT, &world.runner).is_err());
    world.runner.dirty.store(false, Ordering::SeqCst);
    world.runner.wrong_unit.store(true, Ordering::SeqCst);
    assert!(deploy_at(&world.layout, &world.lineage, COMMIT, &world.runner).is_err());
    world.runner.wrong_unit.store(false, Ordering::SeqCst);
    assert!(
        deploy_at(
            &world.layout,
            &world.lineage,
            "2222222222222222222222222222222222222222",
            &world.runner
        )
        .is_err()
    );
    assert_eq!(world.runner.restarts.load(Ordering::SeqCst), 0);
    world.assert_previous_files();
    world.assert_routes();
}

#[test]
fn failed_rollback_keeps_recovery_and_reports_owner_repair_without_false_success() {
    let world = World::new();
    world.configure();
    let before = world.assert_routes();
    world.runner.fail_all_restarts.store(true, Ordering::SeqCst);
    let error = deploy_at(&world.layout, &world.lineage, COMMIT, &world.runner)
        .err()
        .expect("failed activation and rollback must remain failure");
    assert!(error.contains("recovery failed") && error.contains("owner repair"));
    assert!(!error.contains("private failure detail") && !error.contains("BEGIN"));
    world.assert_previous_files();
    world.assert_private_recovery();
    assert!(world.runner.service.lock().unwrap().is_none());
    let receipts = std::fs::read_dir(&world.layout.recovery)
        .unwrap()
        .filter_map(|entry| {
            std::fs::read_to_string(entry.unwrap().path().join("receipt.json")).ok()
        })
        .collect::<Vec<_>>();
    assert!(
        receipts
            .iter()
            .any(|receipt| receipt.contains("rollback_failed"))
    );
    // Simulate the explicit owner repair, then retest the original route surface.
    world
        .runner
        .fail_all_restarts
        .store(false, Ordering::SeqCst);
    systemctl(&world.runner, &["restart", UNIT]).unwrap();
    assert_eq!(world.assert_routes(), before);
}

#[test]
fn existing_other_hook_and_nonprivate_key_are_preserved_and_rejected() {
    let world = World::new();
    write_private(&world.layout.hook, b"#!/bin/sh\n# another workflow\n");
    assert!(configure_at(&world.layout, &world.lineage, COMMIT, &world.runner).is_err());
    assert!(std::fs::read(&world.layout.hook).unwrap() == b"#!/bin/sh\n# another workflow\n");
    std::fs::remove_file(&world.layout.hook).unwrap();
    world.configure();
    std::fs::set_permissions(
        world.archive.join("renewed.key"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(deploy_at(&world.layout, &world.lineage, COMMIT, &world.runner).is_err());
    world.assert_previous_files();
    world.assert_routes();
    assert_eq!(world.runner.restarts.load(Ordering::SeqCst), 0);
}
