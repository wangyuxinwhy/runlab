#![cfg(target_os = "linux")]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use flate2::Compression;
use flate2::write::GzEncoder;
use rustix::process::{Pid, Signal, kill_process};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tar::{Builder, EntryType, Header};

const MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const CONFIG_MEDIA_TYPE: &str = "application/vnd.oci.image.config.v1+json";
const LAYER_MEDIA_TYPE: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

#[test]
#[ignore = "requires rootful Linux, a static C toolchain, OverlayFS, cgroup v2 and RUNLAB_TEST_RUNC"]
fn native_cli_execution_contract() {
    let fixture = NativeFixture::new();

    dry_run_does_not_initialize_state(&fixture);
    happy_path(&fixture);
    prepublication_staging_reconciliation(&fixture);
    read_only_sensitive_file_binding(&fixture);
    missing_executable(&fixture);
    timeout(&fixture);
    oom_kill(&fixture);
    stdout_capture_limit(&fixture);
    inherited_streams_do_not_block_cleanup(&fixture);
    managed_service_happy_path(&fixture);
    managed_service_readiness_cancellation(&fixture);
    cancellation(&fixture);
    supervisor_loss_reconciliation(&fixture);
}

#[test]
#[ignore = "requires rootful Linux, a static C toolchain, OverlayFS, cgroup v2 and RUNLAB_TEST_RUNC"]
fn native_managed_service_gc_contract() {
    let fixture = NativeFixture::new();
    let (run_id, primary_manifest, service_manifest) = managed_service_happy_path(&fixture);
    assert_managed_terminal_run_survives_gc(
        &fixture,
        &run_id,
        &primary_manifest,
        &service_manifest,
    );
}

fn prepublication_staging_reconciliation(fixture: &NativeFixture) {
    let run_id = format!("run-{}", uuid::Uuid::now_v7());
    let staging = fixture
        .state()
        .join("recovery/native")
        .join(format!(".prepare-{run_id}"));
    fs::create_dir(&staging).expect("prepublication staging");
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))
        .expect("prepublication staging permissions");

    let before = fixture.output(&["run", "get", &run_id]);
    assert_eq!(before.status.code(), Some(1));
    assert!(staging.exists(), "run get mutated staging");
    let before_dry_run = snapshot_state_tree(fixture.state());

    assert_gc_plan_rejected(
        fixture,
        "prepublication-gc-plan.json",
        "state GC requires recovery attempts to be reconciled: 1 entries",
    );

    let dry_run = run_json(
        &fixture.output(&["run", "reconcile", "--all", "--dry-run"]),
        Some(0),
    );
    assert_eq!(dry_run["failed"], 0);
    assert_eq!(dry_run["items"][0]["run_id"], run_id);
    assert_eq!(
        dry_run["items"][0]["outcome"]["result"]["status"],
        "planned"
    );
    assert!(staging.exists(), "reconcile dry-run mutated staging");
    assert_state_tree_unchanged(fixture.state(), &before_dry_run);

    let applied = run_json(&fixture.output(&["run", "reconcile", "--all"]), Some(0));
    assert_eq!(applied["failed"], 0);
    assert_eq!(applied["items"][0]["run_id"], run_id);
    assert_eq!(
        applied["items"][0]["outcome"]["result"]["status"],
        "discarded_prepublication"
    );
    assert!(!staging.exists());

    let repeated = run_json(&fixture.output(&["run", "reconcile", "--all"]), Some(0));
    assert_eq!(repeated["failed"], 0);
    assert_eq!(repeated["items"], json!([]));
    fixture.assert_cleanup();
}

#[test]
#[ignore = "requires rootful Linux, a static C toolchain, OverlayFS, cgroup v2, RUNLAB_TEST_RUNC, RUNLAB_TEST_IP, RUNLAB_TEST_NFT, and RUNLAB_TEST_CONNTRACK"]
fn native_egress_packet_contract() {
    let fixture = NativeFixture::new();
    egress_packet_path(&fixture);
    egress_supervisor_loss_reconciliation(&fixture);
    egress_cleanup_failure_terminalizes(&fixture);
}

#[test]
#[ignore = "requires rootful Linux, OverlayFS, cgroup v2, RUNLAB_TEST_RUNC, RUNLAB_TEST_IP, and registry access"]
fn postgres_managed_service_state_transition() {
    let fixture = NativeFixture::new();
    let remote = std::env::var("RUNLAB_TEST_POSTGRES_REMOTE")
        .expect("RUNLAB_TEST_POSTGRES_REMOTE is required");
    let pulled = fixture.output(&[
        "image",
        "pull",
        &remote,
        "--platform",
        host_platform_name(),
        "--name",
        "postgres-e2e",
    ]);
    let pulled = run_json(&pulled, Some(0));
    let postgres = pulled["selected_manifest"]["digest"]
        .as_str()
        .expect("PostgreSQL Manifest")
        .to_owned();
    let generated = fixture.state().join("postgres-generated.json");
    let created = fixture.output(&[
        "runtime-config",
        "create",
        &postgres,
        "--output",
        generated.to_str().expect("generated Runtime Config"),
    ]);
    run_json(&created, Some(0));
    let service_runtime = fixture.state().join("postgres-service.json");
    let primary_runtime = fixture.state().join("postgres-primary.json");
    write_postgres_runtime(&generated, &service_runtime, None);

    write_postgres_runtime(&generated, &primary_runtime, Some("/bin/true"));
    let initialized = execute_postgres_case(
        &fixture,
        &postgres,
        &postgres,
        &primary_runtime,
        &service_runtime,
        "initialize",
    );
    let database_zero = managed_final_manifest(&initialized);

    write_postgres_runtime(
        &generated,
        &primary_runtime,
        Some(
            r#"psql -h 127.0.0.1 -U postgres -d postgres -v ON_ERROR_STOP=1 -c "CREATE TABLE runlab_state (id integer PRIMARY KEY, value text NOT NULL); INSERT INTO runlab_state VALUES (1, 'initial');""#,
        ),
    );
    let mutated = execute_postgres_case(
        &fixture,
        &postgres,
        &database_zero,
        &primary_runtime,
        &service_runtime,
        "mutate",
    );
    assert_eq!(mutated["process"]["facts"]["exit_code"], 0);
    let database_one = managed_final_manifest(&mutated);
    assert_ne!(database_one, database_zero);

    write_postgres_runtime(
        &generated,
        &primary_runtime,
        Some(
            r#"value=$(psql -h 127.0.0.1 -U postgres -d postgres -Atc "SELECT value FROM runlab_state WHERE id=1"); printf "verified:%s\n" "$value""#,
        ),
    );
    let verified = execute_postgres_case(
        &fixture,
        &postgres,
        &database_one,
        &primary_runtime,
        &service_runtime,
        "verify",
    );
    assert_eq!(verified["process"]["facts"]["exit_code"], 0);
    assert_stream(
        &fixture,
        verified["run_id"].as_str().expect("Run ID"),
        "stdout",
        b"verified:initial\n",
    );
    fixture.assert_cleanup();
}

fn dry_run_does_not_initialize_state(fixture: &NativeFixture) {
    let absent = fixture.temp_dir.path().join("absent-state");
    let output = runlab(
        fixture.tool_dir.path(),
        fixture.temp_dir.path(),
        &absent,
        &[
            "run",
            "reconcile",
            "run-00000000-0000-0000-0000-000000000000",
            "--dry-run",
        ],
    );
    assert!(!output.status.success());
    assert!(!absent.exists(), "dry-run initialized an absent state");
}

struct NativeFixture {
    state_dir: tempfile::TempDir,
    tool_dir: tempfile::TempDir,
    temp_dir: tempfile::TempDir,
    base_manifest: String,
    base_image: Value,
}

impl NativeFixture {
    fn new() -> Self {
        let runc_source = required_path("RUNLAB_TEST_RUNC");
        let scratch_parent =
            std::env::var_os("RUNLAB_TEST_TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from);
        let state_dir = tempfile::Builder::new()
            .prefix("runlab-native-state-")
            .tempdir_in(&scratch_parent)
            .expect("state");
        let tool_dir = tempfile::tempdir().expect("tools");
        let temp_dir = tempfile::Builder::new()
            .prefix("runlab-native-e2e-")
            .tempdir_in(scratch_parent)
            .expect("native scratch");
        let runc = tool_dir.path().join("runc");
        fs::copy(runc_source, &runc).expect("copy runc");
        fs::set_permissions(&runc, fs::Permissions::from_mode(0o755)).expect("runc mode");
        for helper in ["unshare", "nsenter", "ip", "cat"] {
            copy_path_tool(tool_dir.path(), helper);
        }

        let executable = compile_static_fixture(state_dir.path());
        let base_manifest = write_oci_fixture(state_dir.path(), &executable);
        let base_image = inspect_image(
            tool_dir.path(),
            temp_dir.path(),
            state_dir.path(),
            &base_manifest,
        );
        Self {
            state_dir,
            tool_dir,
            temp_dir,
            base_manifest,
            base_image,
        }
    }

    fn state(&self) -> &Path {
        self.state_dir.path()
    }

    fn write_runtime(&self, name: &str, arguments: &[&str]) -> PathBuf {
        write_runtime(self.state(), name, arguments)
    }

    fn output(&self, arguments: &[&str]) -> Output {
        runlab(
            self.tool_dir.path(),
            self.temp_dir.path(),
            self.state(),
            arguments,
        )
    }

    fn install_tool(&self, name: &str) {
        copy_path_tool(self.tool_dir.path(), name);
    }

    fn execute(&self, runtime: &Path, options: &[&str], expected_code: i32) -> Value {
        let mut arguments = vec![
            "run",
            "start",
            self.base_manifest.as_str(),
            "--runtime-config",
            runtime.to_str().expect("runtime path"),
        ];
        arguments.extend_from_slice(options);
        let output = self.output(&arguments);
        run_json(&output, Some(expected_code))
    }

    fn execute_with_resolver(
        &self,
        runtime: &Path,
        resolver: &Path,
        options: &[&str],
        expected_code: i32,
    ) -> Value {
        let mut arguments = vec![
            "run",
            "start",
            self.base_manifest.as_str(),
            "--runtime-config",
            runtime.to_str().expect("runtime path"),
        ];
        arguments.extend_from_slice(options);
        let output = runlab_with_resolver(
            self.tool_dir.path(),
            self.temp_dir.path(),
            self.state(),
            resolver,
            &arguments,
        );
        run_json(&output, Some(expected_code))
    }

    fn spawn_execution(&self, runtime: &Path, options: &[&str]) -> Child {
        let mut arguments = vec![
            "run",
            "start",
            self.base_manifest.as_str(),
            "--runtime-config",
            runtime.to_str().expect("runtime path"),
        ];
        arguments.extend_from_slice(options);
        spawn_runlab(
            self.tool_dir.path(),
            self.temp_dir.path(),
            self.state(),
            &arguments,
        )
    }

    fn assert_cleanup(&self) {
        assert_native_cleanup(self.temp_dir.path(), self.state());
    }
}

fn copy_path_tool(directory: &Path, name: &str) {
    let override_name = format!("RUNLAB_TEST_{}", name.to_ascii_uppercase());
    let source = std::env::var_os(&override_name).map_or_else(
        || {
            std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
                .map(|entry| entry.join(name))
                .find(|candidate| candidate.is_file())
                .unwrap_or_else(|| panic!("required test tool is unavailable: {name}"))
        },
        PathBuf::from,
    );
    let target = directory.join(name);
    fs::copy(source, &target).unwrap_or_else(|error| panic!("copy {name}: {error}"));
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|error| panic!("set {name} mode: {error}"));
}

struct LocalForwardTarget {
    workspace: tempfile::TempDir,
    tools: PathBuf,
    fixture_executable: PathBuf,
    host_interface: String,
    holder: Child,
    dns_server: Option<Child>,
    dns_query_marker: PathBuf,
    resolver_path: PathBuf,
    allowed_server: Option<Child>,
    pool_server: Option<Child>,
}

impl LocalForwardTarget {
    const ALLOWED_ADDRESS: Ipv4Addr = Ipv4Addr::new(198, 18, 0, 2);
    const ALLOWED_PORT: u16 = 18_443;
    const FIRST_POOL_ADDRESS: Ipv4Addr = Ipv4Addr::new(10, 240, 0, 2);
    const SECOND_POOL_ADDRESS: Ipv4Addr = Ipv4Addr::new(10, 240, 255, 254);
    const POOL_PORT: u16 = 18_444;

    fn start(fixture: &NativeFixture) -> Self {
        let workspace = tempfile::Builder::new()
            .prefix("runlab-egress-target-")
            .tempdir_in(fixture.temp_dir.path())
            .expect("egress target workspace");
        let fixture_executable = fixture.state().join("fixture");
        let mut holder = Command::new(fixture.tool_dir.path().join("unshare"))
            .args(["--net", "--"])
            .arg(&fixture_executable)
            .arg("wait")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("outside namespace holder");
        wait_for_network_namespace(&mut holder);
        let suffix = format!("{:08x}", std::process::id());
        let resolver_path = workspace.path().join("resolv.conf");
        fs::write(
            &resolver_path,
            format!("nameserver {}\n", Self::ALLOWED_ADDRESS),
        )
        .expect("controlled resolver configuration");
        let mut target = Self {
            dns_query_marker: workspace.path().join("dns-query-observed"),
            resolver_path,
            workspace,
            tools: fixture.tool_dir.path().to_path_buf(),
            fixture_executable,
            host_interface: format!("rle{suffix}"),
            holder,
            dns_server: None,
            allowed_server: None,
            pool_server: None,
        };
        target.configure();
        target.start_servers();
        target
    }

    fn configure(&self) {
        let peer = format!("rlp{:08x}", std::process::id());
        let holder_pid = self.holder.id().to_string();
        self.ip(&[
            "link",
            "add",
            "name",
            &self.host_interface,
            "type",
            "veth",
            "peer",
            "name",
            &peer,
        ]);
        self.ip(&[
            "address",
            "add",
            "198.18.0.1/30",
            "dev",
            &self.host_interface,
        ]);
        self.ip(&["link", "set", "dev", &self.host_interface, "up"]);
        self.ip(&["link", "set", "dev", &peer, "netns", &holder_pid]);
        self.ip_in_target(&["link", "set", "dev", "lo", "up"]);
        self.ip_in_target(&["address", "add", "198.18.0.2/30", "dev", &peer]);
        self.ip_in_target(&["address", "add", "10.240.0.2/32", "dev", &peer]);
        self.ip_in_target(&["address", "add", "10.240.255.254/32", "dev", &peer]);
        self.ip_in_target(&["link", "set", "dev", &peer, "up"]);
        self.ip(&["route", "add", "10.240.0.2/32", "dev", &self.host_interface]);
        self.ip(&[
            "route",
            "add",
            "10.240.255.254/32",
            "dev",
            &self.host_interface,
        ]);
    }

    fn start_servers(&mut self) {
        let dns_marker = self.workspace.path().join("dns-ready");
        let address = Self::ALLOWED_ADDRESS.to_string();
        let mut dns = self.in_target_command();
        dns.arg(&self.fixture_executable)
            .args([
                "dns-server",
                &address,
                "53",
                dns_marker.to_str().expect("DNS ready marker path"),
                self.dns_query_marker
                    .to_str()
                    .expect("DNS query marker path"),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        self.dns_server = Some(dns.spawn().expect("spawn DNS server"));
        wait_for_server_marker(self.dns_server.as_mut().expect("DNS server"), &dns_marker);

        let allowed_marker = self.workspace.path().join("allowed-ready");
        self.allowed_server = Some(self.spawn_server(
            "packet-server",
            Self::ALLOWED_ADDRESS,
            Self::ALLOWED_PORT,
            &allowed_marker,
            true,
        ));
        wait_for_server_marker(
            self.allowed_server.as_mut().expect("allowed server"),
            &allowed_marker,
        );

        let pool_marker = self.workspace.path().join("pool-ready");
        self.pool_server = Some(self.spawn_server(
            "packet-server-loop",
            Ipv4Addr::UNSPECIFIED,
            Self::POOL_PORT,
            &pool_marker,
            false,
        ));
        wait_for_server_marker(
            self.pool_server.as_mut().expect("pool server"),
            &pool_marker,
        );
    }

    fn spawn_server(
        &self,
        mode: &str,
        address: Ipv4Addr,
        port: u16,
        marker: &Path,
        capture_stdout: bool,
    ) -> Child {
        let address = address.to_string();
        let port = port.to_string();
        let mut command = self.in_target_command();
        command
            .arg(&self.fixture_executable)
            .args([
                mode,
                &address,
                &port,
                marker.to_str().expect("server marker path"),
            ])
            .stdin(Stdio::null())
            .stdout(if capture_stdout {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("spawn {mode}: {error}"))
    }

    fn assert_pool_targets_reachable_from_host() {
        for address in [Self::FIRST_POOL_ADDRESS, Self::SECOND_POOL_ADDRESS] {
            let mut connection = TcpStream::connect_timeout(
                &SocketAddrV4::new(address, Self::POOL_PORT).into(),
                Duration::from_secs(2),
            )
            .unwrap_or_else(|error| panic!("pool oracle {address} is unreachable: {error}"));
            connection
                .write_all(b"host-proof")
                .expect("pool proof write");
            let mut response = [0_u8; 2];
            connection
                .read_exact(&mut response)
                .expect("pool proof response");
            assert_eq!(&response, b"ok");
        }
    }

    fn resolver_path(&self) -> &Path {
        &self.resolver_path
    }

    fn assert_allowed_source_is_nat_address(&mut self) {
        let server = self.allowed_server.take().expect("allowed server");
        let output = server.wait_with_output().expect("allowed server output");
        assert!(
            output.status.success(),
            "allowed target failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"198.18.0.1\n", "NAT source address");
    }

    fn assert_dns_query_observed(&mut self) {
        let server = self.dns_server.take().expect("DNS server");
        let output = server.wait_with_output().expect("DNS server output");
        assert!(
            output.status.success(),
            "DNS server failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"agent.runlab.test.\n");
        assert!(
            self.dns_query_marker.is_file(),
            "container did not resolve agent.runlab.test through the projected resolver"
        );
    }

    fn assert_pool_listener_remains_unreached(&mut self) {
        assert!(
            self.pool_server
                .as_mut()
                .expect("pool server")
                .try_wait()
                .expect("poll pool server")
                .is_none(),
            "egress guest reached another Run pool address"
        );
    }

    fn ip(&self, arguments: &[&str]) {
        let mut command = Command::new(self.tools.join("ip"));
        command.args(arguments);
        require_command_success(&mut command, "outside target ip");
    }

    fn ip_in_target(&self, arguments: &[&str]) {
        let mut command = self.in_target_command();
        command.arg(self.tools.join("ip")).args(arguments);
        require_command_success(&mut command, "outside target namespace ip");
    }

    fn in_target_command(&self) -> Command {
        let mut command = Command::new(self.tools.join("nsenter"));
        command
            .arg(format!("--net=/proc/{}/ns/net", self.holder.id()))
            .arg("--");
        command
    }
}

impl Drop for LocalForwardTarget {
    fn drop(&mut self) {
        stop_test_child(self.dns_server.as_mut());
        stop_test_child(self.allowed_server.as_mut());
        stop_test_child(self.pool_server.as_mut());
        let mut command = Command::new(self.tools.join("ip"));
        command.args(["link", "delete", "dev", &self.host_interface]);
        let _ = command.output();
        stop_test_child(Some(&mut self.holder));
    }
}

struct RunNetworkArtifacts {
    interfaces: BTreeSet<String>,
    nft_tables: BTreeSet<String>,
}

impl RunNetworkArtifacts {
    fn capture(fixture: &NativeFixture) -> Self {
        let ip = Command::new(fixture.tool_dir.path().join("ip"))
            .args(["-details", "-json", "link", "show"])
            .output()
            .expect("inspect host links");
        assert!(
            ip.status.success(),
            "ip link inspection failed: {}",
            String::from_utf8_lossy(&ip.stderr)
        );
        let interfaces = serde_json::from_slice::<Value>(&ip.stdout)
            .expect("ip link JSON")
            .as_array()
            .expect("ip link array")
            .iter()
            .filter_map(|link| {
                let name = link["ifname"].as_str()?;
                let alias = link["ifalias"].as_str().unwrap_or_default();
                (name.starts_with("rlh") || alias.starts_with("runlab:")).then(|| name.to_owned())
            })
            .collect();
        let nft = Command::new(fixture.tool_dir.path().join("nft"))
            .args(["list", "tables"])
            .output()
            .expect("inspect nft tables");
        assert!(
            nft.status.success(),
            "nft table inspection failed: {}",
            String::from_utf8_lossy(&nft.stderr)
        );
        let nft_tables = String::from_utf8(nft.stdout)
            .expect("nft table output")
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("table ip runlab_"))
            .map(str::to_owned)
            .collect();
        Self {
            interfaces,
            nft_tables,
        }
    }

    fn assert_restored(&self, fixture: &NativeFixture) {
        let current = Self::capture(fixture);
        assert_eq!(
            current.interfaces, self.interfaces,
            "Run egress veth leaked"
        );
        assert_eq!(current.nft_tables, self.nft_tables, "Run nft table leaked");
    }
}

fn wait_for_network_namespace(holder: &mut Child) {
    let namespace = PathBuf::from(format!("/proc/{}/ns/net", holder.id()));
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if namespace.exists() {
            return;
        }
        if let Some(status) = holder.try_wait().expect("poll outside holder") {
            panic!("outside namespace holder exited during startup: {status}");
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("outside network namespace was not ready within 5 seconds");
}

fn wait_for_server_marker(server: &mut Child, marker: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if marker.is_file() {
            return;
        }
        if let Some(status) = server.try_wait().expect("poll packet server") {
            let mut stderr = Vec::new();
            server
                .stderr
                .as_mut()
                .expect("packet server stderr")
                .read_to_end(&mut stderr)
                .expect("packet server stderr bytes");
            panic!(
                "packet server exited before readiness: {status}; stderr: {}",
                String::from_utf8_lossy(&stderr)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("packet server was not ready within 5 seconds");
}

fn require_command_success(command: &mut Command, operation: &str) {
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("{operation}: {error}"));
    assert!(
        output.status.success(),
        "{operation} failed with {}: stdout: {}; stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stop_test_child(child: Option<&mut Child>) {
    let Some(child) = child else {
        return;
    };
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn assert_no_network_holder(run_id: &str) {
    let leaked = fs::read_dir("/proc")
        .expect("/proc")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .bytes()
                .all(|byte| byte.is_ascii_digit())
        })
        .filter_map(|entry| fs::read(entry.path().join("cmdline")).ok())
        .any(|command| {
            command
                .windows(b"__internal-network-holder".len())
                .any(|window| window == b"__internal-network-holder")
                && command
                    .windows(run_id.len())
                    .any(|window| window == run_id.as_bytes())
        });
    assert!(!leaked, "Run network holder remains for {run_id}");
}

fn assert_no_conntrack_entry(fixture: &NativeFixture, guest_address: &str) {
    let output = Command::new(fixture.tool_dir.path().join("conntrack"))
        .env_clear()
        .env("LC_ALL", "C")
        .args(["-L", "--orig-src", guest_address, "--output", "xml"])
        .output()
        .expect("inspect Run conntrack entries");
    assert!(
        output.status.success(),
        "conntrack inspection for {guest_address} failed with {}: stdout: {}; stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.iter().all(u8::is_ascii_whitespace),
        "conntrack entries remain for {guest_address}: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

fn happy_path(fixture: &NativeFixture) {
    let stdin = fixture.state().join("stdin");
    let input = b"input\0bytes\n";
    fs::write(&stdin, input).expect("stdin");

    let runtime = fixture.write_runtime("happy.json", &["/agent"]);
    let result = fixture.execute(
        &runtime,
        &["--stdin", stdin.to_str().expect("stdin path")],
        0,
    );
    assert_eq!(result["process"]["availability"], "available");
    assert_eq!(
        result["process"]["facts"]["terminal_outcome"],
        "process_exited"
    );
    assert_eq!(result["process"]["facts"]["exit_code"], 7);
    assert_eq!(result["process"]["facts"]["oom_killed"], false);
    assert_eq!(result["operation_errors"], json!([]));
    let run_id = result["run_id"].as_str().expect("run id");
    let terminal = run_json(&fixture.output(&["run", "get", run_id]), Some(0));
    assert_eq!(terminal["backend"]["details"]["runtime_name"], "runc");
    assert_eq!(terminal["backend"]["details"]["runtime_version"], "1.5.1");
    assert_eq!(
        terminal["backend"]["details"]["runtime_commit"],
        "v1.5.1-0-g8f2685a47"
    );
    assert_eq!(terminal["backend"]["details"]["runtime_spec"], "1.3.0");
    assert_stream(fixture, run_id, "stdout", input);
    assert_stream(fixture, run_id, "stderr", b"diagnostic\n");
    let final_manifest = assert_final_child(fixture, &result);
    let final_file = fixture.state().join("result");
    let extracted = fixture.output(&[
        "image",
        "file",
        "get",
        &final_manifest,
        "/workspace/result",
        "--output",
        final_file.to_str().expect("Final file path"),
    ]);
    assert!(extracted.status.success());
    assert_eq!(fs::read(final_file).expect("Final file"), b"changed\n");
    assert_terminal_run_survives_gc(fixture, run_id, &final_manifest);
    fixture.assert_cleanup();
}

fn assert_terminal_run_survives_gc(fixture: &NativeFixture, run_id: &str, final_manifest: &str) {
    let blob_directory = fixture.state().join("oci/blobs/sha256");
    let orphan = put_blob(
        &blob_directory,
        b"unreachable native E2E blob\n",
        "application/octet-stream",
    );
    let orphan_digest = orphan["digest"].as_str().expect("orphan digest");
    let orphan_path = blob_directory.join(&orphan_digest["sha256:".len()..]);
    let plan_path = fixture.state().join("terminal-run-gc-plan.json");
    let planned = run_json(
        &fixture.output(&[
            "state",
            "gc",
            "plan",
            "--output",
            plan_path.to_str().expect("GC plan path"),
        ]),
        Some(0),
    );
    assert_eq!(planned["roots"], 2);
    assert_eq!(planned["delete_oci_blobs"], 1);

    let plan: Value = serde_json::from_slice(&fs::read(&plan_path).expect("GC plan bytes"))
        .expect("GC plan JSON");
    let roots = plan["roots"].as_array().expect("GC roots");
    assert_eq!(roots.len(), 2);
    assert!(
        roots
            .iter()
            .all(|root| { root["owner"]["kind"] == "run" && root["owner"]["run_id"] == run_id })
    );
    let root_digests = roots
        .iter()
        .map(|root| {
            root["manifest"]["digest"]
                .as_str()
                .expect("root Manifest digest")
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        root_digests,
        BTreeSet::from([fixture.base_manifest.as_str(), final_manifest])
    );
    let root_slots = roots
        .iter()
        .map(|root| root["owner"]["slot"].as_str().expect("Run Image slot"))
        .collect::<BTreeSet<_>>();
    assert_eq!(root_slots, BTreeSet::from(["initial", "final"]));
    assert_eq!(plan["delete"][0]["digest"], orphan_digest);

    let applied = run_json(
        &fixture.output(&[
            "state",
            "gc",
            "apply",
            plan_path.to_str().expect("GC plan path"),
        ]),
        Some(0),
    );
    assert_eq!(applied["deleted_oci_blobs"], 1);
    assert_eq!(applied["skipped_reachable_oci_blobs"], 0);
    assert_eq!(applied["failed"], 0);
    assert!(!orphan_path.exists());

    let run = run_json(&fixture.output(&["run", "verify", run_id]), Some(0));
    assert_eq!(run["valid"], true);
    assert_eq!(run["lifecycle"], "terminal");
    assert_eq!(run["image_roots"], 2);
    assert_eq!(run["verified_oci_blobs"], 6);

    let initial = inspect_image(
        fixture.tool_dir.path(),
        fixture.temp_dir.path(),
        fixture.state(),
        &fixture.base_manifest,
    );
    assert_eq!(initial["manifest"]["digest"], fixture.base_manifest);
    let final_image = inspect_image(
        fixture.tool_dir.path(),
        fixture.temp_dir.path(),
        fixture.state(),
        final_manifest,
    );
    assert_eq!(final_image["manifest"]["digest"], final_manifest);

    let state = run_json(&fixture.output(&["state", "verify"]), Some(0));
    assert_eq!(state["valid"], true);
    assert_eq!(state["runs"], 1);
    assert_eq!(state["accepted_runs"], 0);
    assert_eq!(state["image_roots"], 2);
    assert_eq!(state["orphan_oci_blobs"], 0);
    fs::remove_file(plan_path).expect("remove applied GC plan");
}

fn assert_managed_terminal_run_survives_gc(
    fixture: &NativeFixture,
    run_id: &str,
    primary_final: &str,
    service_final: &str,
) {
    let blob_directory = fixture.state().join("oci/blobs/sha256");
    let orphan = put_blob(
        &blob_directory,
        b"unreachable managed-service E2E blob\n",
        "application/octet-stream",
    );
    let orphan_digest = orphan["digest"].as_str().expect("orphan digest");
    let orphan_path = blob_directory.join(&orphan_digest["sha256:".len()..]);
    let plan_path = fixture.state().join("managed-run-gc-plan.json");
    let planned = run_json(
        &fixture.output(&[
            "state",
            "gc",
            "plan",
            "--output",
            plan_path.to_str().expect("GC plan path"),
        ]),
        Some(0),
    );
    assert_eq!(planned["roots"], 4);
    assert_eq!(planned["delete_oci_blobs"], 1);

    let plan: Value = serde_json::from_slice(&fs::read(&plan_path).expect("GC plan bytes"))
        .expect("GC plan JSON");
    let roots = plan["roots"].as_array().expect("GC roots");
    assert_eq!(roots.len(), 4);
    assert!(
        roots
            .iter()
            .all(|root| { root["owner"]["kind"] == "run" && root["owner"]["run_id"] == run_id })
    );
    let participants = roots
        .iter()
        .map(|root| {
            root["owner"]["participant"]["kind"]
                .as_str()
                .expect("participant")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        participants
            .iter()
            .filter(|kind| **kind == "primary")
            .count(),
        2
    );
    assert_eq!(
        participants
            .iter()
            .filter(|kind| **kind == "managed_service")
            .count(),
        2
    );
    let slots = roots
        .iter()
        .map(|root| root["owner"]["slot"].as_str().expect("slot"))
        .collect::<Vec<_>>();
    assert_eq!(slots.iter().filter(|slot| **slot == "initial").count(), 2);
    assert_eq!(slots.iter().filter(|slot| **slot == "final").count(), 2);
    assert_eq!(plan["delete"][0]["digest"], orphan_digest);

    let applied = run_json(
        &fixture.output(&[
            "state",
            "gc",
            "apply",
            plan_path.to_str().expect("GC plan path"),
        ]),
        Some(0),
    );
    assert_eq!(applied["deleted_oci_blobs"], 1);
    assert_eq!(applied["failed"], 0);
    assert!(!orphan_path.exists());

    let run = run_json(&fixture.output(&["run", "verify", run_id]), Some(0));
    assert_eq!(run["valid"], true);
    assert_eq!(run["image_roots"], 4);
    for manifest in [fixture.base_manifest.as_str(), primary_final, service_final] {
        let inspected = inspect_image(
            fixture.tool_dir.path(),
            fixture.temp_dir.path(),
            fixture.state(),
            manifest,
        );
        assert_eq!(inspected["manifest"]["digest"], manifest);
    }
    let state = run_json(&fixture.output(&["state", "verify"]), Some(0));
    assert_eq!(state["valid"], true);
    assert_eq!(state["runs"], 1);
    assert_eq!(state["image_roots"], 4);
    assert_eq!(state["orphan_oci_blobs"], 0);
    fs::remove_file(plan_path).expect("remove applied GC plan");
}

fn assert_gc_plan_rejected(fixture: &NativeFixture, name: &str, expected_error: &str) {
    let plan = fixture.state().join(name);
    let output = fixture.output(&[
        "state",
        "gc",
        "plan",
        "--output",
        plan.to_str().expect("GC plan path"),
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains(expected_error));
    assert!(!plan.exists());
}

fn read_only_sensitive_file_binding(fixture: &NativeFixture) {
    let source = fixture.temp_dir.path().join("credential");
    let sentinel = format!("runlab-sensitive-{}\n", std::process::id()).into_bytes();
    fs::write(&source, &sentinel).expect("sensitive source");
    fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).expect("sensitive mode");
    let source = source.canonicalize().expect("canonical sensitive source");

    let trusted_runtime = fixture.state().join("sensitive-trusted.json");
    fs::write(
        &trusted_runtime,
        runtime_config_with_file_mount(&["/agent", "secret-read"], &source),
    )
    .expect("trusted sensitive Runtime Config");
    let trusted = fixture.execute(&trusted_runtime, &[], 0);
    assert_eq!(trusted["process"]["facts"]["exit_code"], 0);
    let trusted_run = trusted["run_id"].as_str().expect("trusted Run ID");
    assert_stream(fixture, trusted_run, "stdout", b"");
    assert_stream(fixture, trusted_run, "stderr", b"");
    let trusted_final = assert_final_child(fixture, &trusted);
    assert_image_file(
        fixture,
        &trusted_final,
        "/run/credential",
        b"",
        "sensitive-placeholder",
    );
    assert_last_layer_lacks_path(fixture, &trusted, "run/credential");
    assert_bytes_absent_below(fixture.state(), &sentinel);

    let copying_runtime = fixture.state().join("sensitive-copying.json");
    fs::write(
        &copying_runtime,
        runtime_config_with_file_mount(&["/agent", "secret-copy"], &source),
    )
    .expect("copying sensitive Runtime Config");
    let copying = fixture.execute(&copying_runtime, &[], 0);
    assert_eq!(copying["process"]["facts"]["exit_code"], 0);
    let copying_final = assert_final_child(fixture, &copying);
    assert_image_file(
        fixture,
        &copying_final,
        "/workspace/copied-credential",
        &sentinel,
        "copied-sensitive-value",
    );
    fs::remove_file(source).expect("remove caller-owned sensitive source");
    fixture.assert_cleanup();
}

fn missing_executable(fixture: &NativeFixture) {
    let runtime = fixture.write_runtime("missing.json", &["/does-not-exist"]);
    let result = fixture.execute(&runtime, &[], 1);
    assert_eq!(
        result["process"]["facts"]["terminal_outcome"],
        "not_started"
    );
    assert_eq!(result["process"]["facts"]["exit_code"], Value::Null);
    assert_eq!(result["process"]["facts"]["started_at"], Value::Null);
    assert_eq!(result["final_image"]["availability"], "unavailable");
    assert!(
        !result["operation_errors"]
            .as_array()
            .expect("operation errors")
            .is_empty()
    );
    fixture.assert_cleanup();
}

fn timeout(fixture: &NativeFixture) {
    let runtime = fixture.write_runtime("timeout.json", &["/agent", "wait"]);
    let result = fixture.execute(&runtime, &["--timeout-seconds", "1"], 0);
    assert_eq!(result["process"]["facts"]["terminal_outcome"], "timed_out");
    assert_eq!(result["process"]["facts"]["oom_killed"], false);
    assert_eq!(result["operation_errors"], json!([]));
    assert_final_child(fixture, &result);
    fixture.assert_cleanup();
}

fn oom_kill(fixture: &NativeFixture) {
    let runtime = fixture.state().join("oom.json");
    fs::write(
        &runtime,
        runtime_config_with_memory_limit(&["/agent", "oom"], 67_108_864),
    )
    .expect("OOM Runtime Config");
    let result = fixture.execute(&runtime, &["--timeout-seconds", "20"], 0);
    assert_eq!(
        result["process"]["facts"]["terminal_outcome"],
        "process_exited"
    );
    assert_eq!(result["process"]["facts"]["exit_code"], 137);
    assert_eq!(result["process"]["facts"]["oom_killed"], true);
    assert_eq!(result["operation_errors"], json!([]));
    assert_final_child(fixture, &result);
    fixture.assert_cleanup();
}

fn stdout_capture_limit(fixture: &NativeFixture) {
    let runtime = fixture.write_runtime("limit.json", &["/agent", "stdout"]);
    let result = fixture.execute(&runtime, &["--stdout-limit-bytes", "1024"], 0);
    assert_eq!(
        result["process"]["facts"]["terminal_outcome"],
        "capture_limit_exceeded"
    );
    assert_eq!(result["stdout"]["availability"], "partial");
    assert_eq!(result["stdout"]["size"], 1024);
    assert_eq!(result["operation_errors"], json!([]));
    assert_stream(
        fixture,
        result["run_id"].as_str().expect("run id"),
        "stdout",
        &vec![b'x'; 1024],
    );
    assert_final_child(fixture, &result);
    fixture.assert_cleanup();
}

fn inherited_streams_do_not_block_cleanup(fixture: &NativeFixture) {
    let runtime = fixture.write_runtime("descendant.json", &["/agent", "descendant"]);
    let result = fixture.execute(&runtime, &[], 0);
    assert_eq!(
        result["process"]["facts"]["terminal_outcome"],
        "process_exited"
    );
    assert_eq!(result["process"]["facts"]["exit_code"], 0);
    assert_eq!(result["operation_errors"], json!([]));
    assert_final_child(fixture, &result);
    fixture.assert_cleanup();
}

fn egress_packet_path(fixture: &NativeFixture) {
    fixture.install_tool("nft");
    fixture.install_tool("conntrack");
    let baseline = RunNetworkArtifacts::capture(fixture);
    let host_listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("host test listener");
    host_listener
        .set_nonblocking(true)
        .expect("host listener nonblocking mode");
    let host_port = host_listener
        .local_addr()
        .expect("host listener address")
        .port();

    {
        let mut target = LocalForwardTarget::start(fixture);
        LocalForwardTarget::assert_pool_targets_reachable_from_host();
        let runtime = fixture.state().join("egress.json");
        let allowed_address = LocalForwardTarget::ALLOWED_ADDRESS.to_string();
        let allowed_port = LocalForwardTarget::ALLOWED_PORT.to_string();
        let host_port = host_port.to_string();
        let first_pool_address = LocalForwardTarget::FIRST_POOL_ADDRESS.to_string();
        let second_pool_address = LocalForwardTarget::SECOND_POOL_ADDRESS.to_string();
        let pool_port = LocalForwardTarget::POOL_PORT.to_string();
        fs::write(
            &runtime,
            runtime_config_managed(&[
                "/agent",
                "egress-client",
                &allowed_address,
                &allowed_port,
                &host_port,
                &first_pool_address,
                &second_pool_address,
                &pool_port,
            ]),
        )
        .expect("egress Runtime Config");
        let result = fixture.execute_with_resolver(
            &runtime,
            target.resolver_path(),
            &["--network", "egress", "--timeout-seconds", "15"],
            0,
        );
        assert_eq!(result["process"]["facts"]["exit_code"], 0, "{result}");
        let run_id = result["run_id"].as_str().expect("egress Run ID");
        assert_stream(fixture, run_id, "stdout", b"egress-ok\n");
        assert_stream(fixture, run_id, "stderr", b"");
        let final_manifest = assert_final_child(fixture, &result);
        assert_image_file(
            fixture,
            &final_manifest,
            "/etc/resolv.conf",
            b"",
            "egress-final-resolver",
        );
        assert_last_layer_lacks_path(fixture, &result, "etc/resolv.conf");

        let terminal = run_json(&fixture.output(&["run", "get", run_id]), Some(0));
        assert_eq!(terminal["controls"]["network"], "egress");
        let network = &terminal["backend"]["run_network"];
        assert!(network["namespace_device"].as_u64().is_some());
        assert!(network["namespace_inode"].as_u64().is_some());
        assert_eq!(
            network["realization"]["kind"], "ipv4_nat_egress",
            "{terminal}"
        );
        assert_eq!(network["realization"]["prefix_length"], 30);
        assert_eq!(
            network["realization"]["resolver"]["source"],
            "etc_resolv_conf"
        );
        assert_eq!(
            network["realization"]["resolver"]["nameservers"],
            json!([LocalForwardTarget::ALLOWED_ADDRESS.to_string()])
        );
        let guest_address = network["realization"]["guest_address"]
            .as_str()
            .expect("egress guest address");

        target.assert_dns_query_observed();
        target.assert_allowed_source_is_nat_address();
        target.assert_pool_listener_remains_unreached();
        assert_eq!(
            host_listener.accept().map(|_| ()).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "egress guest reached a host INPUT listener"
        );
        assert_no_network_holder(run_id);
        assert_no_conntrack_entry(fixture, guest_address);
    }

    baseline.assert_restored(fixture);
    fixture.assert_cleanup();
}

fn egress_supervisor_loss_reconciliation(fixture: &NativeFixture) {
    fixture.install_tool("nft");
    fixture.install_tool("conntrack");
    let baseline = RunNetworkArtifacts::capture(fixture);

    {
        let mut target = LocalForwardTarget::start(fixture);
        let runtime = fixture.state().join("egress-supervisor-loss.json");
        let allowed_address = LocalForwardTarget::ALLOWED_ADDRESS.to_string();
        let allowed_port = LocalForwardTarget::ALLOWED_PORT.to_string();
        fs::write(
            &runtime,
            runtime_config_managed(&[
                "/agent",
                "egress-connect-wait",
                &allowed_address,
                &allowed_port,
            ]),
        )
        .expect("egress supervisor-loss Runtime Config");

        let mut child = fixture.spawn_execution(
            &runtime,
            &["--network", "egress", "--timeout-seconds", "30"],
        );
        let run_id = wait_for_init(&mut child, fixture.state());
        let result_path = fixture
            .state()
            .join("recovery/native")
            .join(&run_id)
            .join("workspace/bundle/rootfs/workspace/result");
        wait_for_path(&mut child, &result_path);
        let journal_path = fixture
            .state()
            .join("recovery/native")
            .join(&run_id)
            .join("journal.json");
        let journal: Value =
            serde_json::from_slice(&fs::read(&journal_path).expect("resolver recovery journal"))
                .expect("resolver recovery journal JSON");
        assert_eq!(journal["resolver"]["primary"]["phase"], "mounted");
        assert_egress_ipv6_disabled(fixture, &run_id);
        target.assert_allowed_source_is_nat_address();

        let accepted = run_json(&fixture.output(&["run", "get", &run_id]), Some(0));
        assert_eq!(accepted["lifecycle"], "accepted");
        signal(&child, Signal::KILL);
        let killed = child.wait_with_output().expect("reap egress supervisor");
        assert_eq!(killed.status.code(), None);

        let reconcile = run_json(&fixture.output(&["run", "reconcile", &run_id]), Some(0));
        assert_eq!(reconcile["status"], "reconciled");
        assert_eq!(reconcile["terminalized"], true);
        assert_eq!(reconcile["resources_absent"], true);
        assert!(
            reconcile["actions"]
                .as_array()
                .expect("reconcile actions")
                .iter()
                .any(|action| action == "resolver_projection_removed"),
            "resolver projection cleanup was not reported: {reconcile}"
        );

        let terminal = run_json(&fixture.output(&["run", "get", &run_id]), Some(0));
        assert_eq!(terminal["lifecycle"], "terminal");
        assert_eq!(terminal["process"]["availability"], "unavailable");
        let final_manifest = assert_final_child(fixture, &terminal);
        assert_image_file(
            fixture,
            &final_manifest,
            "/etc/resolv.conf",
            b"",
            "egress-reconciled-final-resolver",
        );
        assert_last_layer_lacks_path(fixture, &terminal, "etc/resolv.conf");
        let guest_address = terminal["backend"]["run_network"]["realization"]["guest_address"]
            .as_str()
            .expect("egress guest address");
        assert_no_network_holder(&run_id);
        assert_no_conntrack_entry(fixture, guest_address);
    }

    baseline.assert_restored(fixture);
    fixture.assert_cleanup();
}

fn assert_egress_ipv6_disabled(fixture: &NativeFixture, run_id: &str) {
    let journal_path = fixture
        .state()
        .join("recovery/native")
        .join(run_id)
        .join("journal.json");
    let journal: Value = serde_json::from_slice(&fs::read(journal_path).expect("network journal"))
        .expect("network journal JSON");
    let network = &journal["shared_network"];
    let holder_pid = network["holder_pid"].as_u64().expect("network holder PID");
    let host_interface = network["plan"]["egress"]["host_interface"]
        .as_str()
        .expect("host egress interface");
    assert_eq!(
        fs::read_to_string(format!(
            "/proc/sys/net/ipv6/conf/{host_interface}/disable_ipv6"
        ))
        .expect("host egress IPv6 state")
        .trim(),
        "1"
    );
    let output = Command::new(fixture.tool_dir.path().join("nsenter"))
        .arg(format!("--net=/proc/{holder_pid}/ns/net"))
        .arg("--")
        .arg(fixture.tool_dir.path().join("ip"))
        .args(["-6", "-json", "address", "show", "dev", "eth0"])
        .output()
        .expect("inspect guest IPv6 addresses");
    assert!(
        output.status.success(),
        "guest IPv6 inspection failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let links: Value = serde_json::from_slice(&output.stdout).expect("guest IPv6 JSON");
    assert!(
        links
            .as_array()
            .expect("guest IPv6 array")
            .iter()
            .all(|link| link["addr_info"].as_array().is_none_or(Vec::is_empty)),
        "egress guest retained an IPv6 address: {links}"
    );
}

fn egress_cleanup_failure_terminalizes(fixture: &NativeFixture) {
    fixture.install_tool("nft");
    fixture.install_tool("conntrack");
    let baseline = RunNetworkArtifacts::capture(fixture);
    let runtime = fixture.state().join("egress-cleanup-failure.json");
    fs::write(&runtime, runtime_config_managed(&["/agent", "write-wait"]))
        .expect("egress cleanup-failure Runtime Config");

    let mut child = fixture.spawn_execution(
        &runtime,
        &["--network", "egress", "--timeout-seconds", "30"],
    );
    let run_id = wait_for_init(&mut child, fixture.state());
    let attempt = fixture.state().join("recovery/native").join(&run_id);
    wait_for_path(
        &mut child,
        &attempt.join("workspace/bundle/rootfs/workspace/result"),
    );
    let journal: Value =
        serde_json::from_slice(&fs::read(attempt.join("journal.json")).expect("network journal"))
            .expect("network journal JSON");
    let table = journal["shared_network"]["plan"]["egress"]["nft_table"]
        .as_str()
        .expect("Run nft table");
    replace_with_foreign_nft_table(fixture, table);

    interrupt(&child);
    let output = child
        .wait_with_output()
        .expect("cancel cleanup-failure Run");
    let start = run_json(&output, Some(130));
    assert!(
        start["operation_errors"]
            .as_array()
            .expect("operation errors")
            .iter()
            .any(|error| error["phase"] == "resource_cleanup"),
        "Run result omitted network cleanup failure: {start}"
    );
    let terminal = run_json(&fixture.output(&["run", "get", &run_id]), Some(0));
    assert_eq!(terminal["lifecycle"], "terminal");
    assert!(
        terminal["operation_errors"]
            .as_array()
            .expect("terminal operation errors")
            .iter()
            .any(|error| error["phase"] == "resource_cleanup"),
        "terminal record omitted network cleanup failure: {terminal}"
    );
    assert!(
        attempt.is_dir(),
        "failed cleanup must retain recovery state"
    );

    delete_nft_table(fixture, table);
    let reconciled = run_json(&fixture.output(&["run", "reconcile", &run_id]), Some(0));
    assert_eq!(reconciled["status"], "cleaned_terminal_attempt");
    assert_eq!(reconciled["resources_absent"], true);
    assert!(!attempt.exists(), "reconcile retained the recovery attempt");
    baseline.assert_restored(fixture);
    fixture.assert_cleanup();
}

fn replace_with_foreign_nft_table(fixture: &NativeFixture, table: &str) {
    delete_nft_table(fixture, table);
    let mut command = Command::new(fixture.tool_dir.path().join("nft"));
    command.arg(format!(
        "add table ip {table} {{ comment \"runlab-test:foreign\"; }}"
    ));
    require_command_success(&mut command, "install foreign nft table");
}

fn delete_nft_table(fixture: &NativeFixture, table: &str) {
    let mut command = Command::new(fixture.tool_dir.path().join("nft"));
    command.arg(format!("delete table ip {table}"));
    require_command_success(&mut command, "delete nft table");
}

fn managed_service_happy_path(fixture: &NativeFixture) -> (String, String, String) {
    let primary_runtime = fixture.state().join("managed-primary.json");
    let service_runtime = fixture.state().join("managed-service.json");
    fs::write(
        &primary_runtime,
        runtime_config_managed(&["/agent", "client"]),
    )
    .expect("Primary managed Runtime Config");
    fs::write(
        &service_runtime,
        runtime_config_managed(&["/agent", "service"]),
    )
    .expect("service Runtime Config");
    let declaration = fixture.state().join("service.json");
    fs::write(
        &declaration,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "name": "fixture-service",
            "initial_manifest": fixture.base_manifest,
            "runtime_config_file": service_runtime,
            "readiness": {"kind": "tcp", "port": 15432, "timeout_seconds": 10}
        }))
        .expect("Managed Service declaration"),
    )
    .expect("Managed Service file");
    let result = fixture.execute(
        &primary_runtime,
        &[
            "--managed-service",
            declaration.to_str().expect("declaration path"),
        ],
        0,
    );
    assert_eq!(result["process"]["facts"]["exit_code"], 0);
    assert_eq!(result["managed_service"]["readiness"]["outcome"], "ready");
    let primary_manifest = assert_final_child(fixture, &result);
    let service_manifest = result["managed_service"]["final_image"]["manifest"]["digest"]
        .as_str()
        .expect("Managed Service Final Manifest")
        .to_owned();
    assert_image_file(
        fixture,
        &primary_manifest,
        "/workspace/result",
        b"service-ok\n",
        "managed-primary-result",
    );
    assert_image_file(
        fixture,
        &service_manifest,
        "/service/value",
        b"updated\n",
        "managed-service-result",
    );
    fixture.assert_cleanup();
    (
        result["run_id"].as_str().expect("Run ID").to_owned(),
        primary_manifest,
        service_manifest,
    )
}

fn managed_service_readiness_cancellation(fixture: &NativeFixture) {
    let primary_runtime = fixture.state().join("managed-cancel-primary.json");
    let service_runtime = fixture.state().join("managed-cancel-service.json");
    fs::write(
        &primary_runtime,
        runtime_config_managed(&["/agent", "wait"]),
    )
    .expect("Primary cancellation Runtime Config");
    fs::write(
        &service_runtime,
        runtime_config_managed(&["/agent", "wait"]),
    )
    .expect("service cancellation Runtime Config");
    let declaration = fixture.state().join("service-cancel.json");
    fs::write(
        &declaration,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "name": "fixture-service",
            "initial_manifest": fixture.base_manifest,
            "runtime_config_file": service_runtime,
            "readiness": {"kind": "tcp", "port": 15433, "timeout_seconds": 30}
        }))
        .expect("Managed Service cancellation declaration"),
    )
    .expect("Managed Service cancellation file");
    let mut child = fixture.spawn_execution(
        &primary_runtime,
        &[
            "--managed-service",
            declaration.to_str().expect("declaration path"),
        ],
    );
    let run_id = wait_for_managed_init(&mut child, fixture.state());
    interrupt(&child);
    let output = child.wait_with_output().expect("cancelled managed output");
    let result = run_json(&output, Some(130));
    assert_eq!(result["run_id"], run_id);
    assert_eq!(result["process"]["facts"]["terminal_outcome"], "cancelled");
    assert_eq!(result["process"]["facts"]["started_at"], Value::Null);
    assert_eq!(result["operation_errors"], json!([]));
    assert_eq!(
        result["managed_service"]["readiness"]["outcome"],
        "cancelled"
    );
    assert_eq!(
        result["managed_service"]["process"]["facts"]["terminal_outcome"],
        "cancelled"
    );
    assert_eq!(result["managed_service"]["operation_errors"], json!([]));
    let stored = run_json(&fixture.output(&["run", "get", &run_id]), Some(0));
    assert_eq!(stored["process"], result["process"]);
    assert_eq!(
        stored["managed_service"]["readiness"],
        result["managed_service"]["readiness"]
    );
    assert_eq!(
        stored["managed_service"]["process"],
        result["managed_service"]["process"]
    );
    fixture.assert_cleanup();
}

fn cancellation(fixture: &NativeFixture) {
    let runtime = fixture.write_runtime("cancel.json", &["/agent", "wait"]);
    let mut child = fixture.spawn_execution(&runtime, &[]);
    let _run_id = wait_for_init(&mut child, fixture.state());
    interrupt(&child);
    let output = child.wait_with_output().expect("cancelled output");
    let result = run_json(&output, Some(130));
    assert_eq!(result["process"]["facts"]["terminal_outcome"], "cancelled");
    assert_eq!(result["process"]["facts"]["oom_killed"], false);
    assert_eq!(result["operation_errors"], json!([]));
    assert_final_child(fixture, &result);
    fixture.assert_cleanup();
}

fn supervisor_loss_reconciliation(fixture: &NativeFixture) {
    let gc = prepare_preaccepted_gc_plan(fixture);
    let (child, run_id, journal) = start_observable_supervisor_loss_run(fixture);
    assert_accepted_get_is_read_only(fixture, &run_id, &journal);
    signal(&child, Signal::KILL);
    let killed = child.wait_with_output().expect("reap killed supervisor");
    assert_eq!(killed.status.code(), None);
    assert_gc_apply_blocked_by_accepted_run(fixture, &gc);
    assert_gc_plan_rejected(
        fixture,
        "accepted-run-gc-plan.json",
        "state GC requires every accepted Run to become terminal or be reconciled",
    );
    reconcile_lost_supervisor(fixture, &run_id, &journal);
    assert_reconciled_terminal(fixture, &run_id);
    apply_preaccepted_gc_plan(fixture, gc);
    fixture.assert_cleanup();
}

struct PreacceptedGcPlan {
    path: PathBuf,
    orphan_path: PathBuf,
}

fn prepare_preaccepted_gc_plan(fixture: &NativeFixture) -> PreacceptedGcPlan {
    let blob_directory = fixture.state().join("oci/blobs/sha256");
    let orphan = put_blob(
        &blob_directory,
        b"apply-time accepted Run guard\n",
        "application/octet-stream",
    );
    let orphan_digest = orphan["digest"].as_str().expect("orphan digest");
    let orphan_path = blob_directory.join(&orphan_digest["sha256:".len()..]);
    let preaccepted_plan = fixture.state().join("preaccepted-gc-plan.json");
    run_json(
        &fixture.output(&[
            "state",
            "gc",
            "plan",
            "--output",
            preaccepted_plan.to_str().expect("GC plan path"),
        ]),
        Some(0),
    );
    PreacceptedGcPlan {
        path: preaccepted_plan,
        orphan_path,
    }
}

fn start_observable_supervisor_loss_run(fixture: &NativeFixture) -> (Child, String, PathBuf) {
    let runtime = fixture.write_runtime("supervisor-loss.json", &["/agent", "write-wait"]);
    let mut child = fixture.spawn_execution(&runtime, &[]);
    let run_id = wait_for_init(&mut child, fixture.state());
    let result_path = fixture
        .state()
        .join("recovery/native")
        .join(&run_id)
        .join("workspace/bundle/rootfs/workspace/result");
    wait_for_path(&mut child, &result_path);
    let journal = fixture
        .state()
        .join("recovery/native")
        .join(&run_id)
        .join("journal.json");
    (child, run_id, journal)
}

fn assert_accepted_get_is_read_only(fixture: &NativeFixture, run_id: &str, journal: &Path) {
    let journal_before_get = fs::read(journal).expect("recovery journal");
    let accepted_output = fixture.output(&["run", "get", run_id]);
    let accepted = run_json(&accepted_output, Some(0));
    assert_eq!(accepted["lifecycle"], "accepted");
    assert_eq!(
        fs::read(journal).expect("recovery journal after get"),
        journal_before_get,
        "run get must not reconcile an active Run"
    );
}

fn assert_gc_apply_blocked_by_accepted_run(fixture: &NativeFixture, plan: &PreacceptedGcPlan) {
    let rejected_apply = fixture.output(&[
        "state",
        "gc",
        "apply",
        plan.path.to_str().expect("GC plan path"),
    ]);
    assert_eq!(rejected_apply.status.code(), Some(1));
    assert!(rejected_apply.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&rejected_apply.stderr)
            .contains("state GC requires every accepted Run to become terminal or be reconciled")
    );
    assert!(
        plan.orphan_path.exists(),
        "rejected GC apply deleted its candidate"
    );
}

fn reconcile_lost_supervisor(fixture: &NativeFixture, run_id: &str, journal: &Path) {
    let journal_before_plan = fs::read(journal).expect("orphan journal");
    let plan_output = fixture.output(&["run", "reconcile", "--all", "--limit", "1", "--dry-run"]);
    let plan = run_json(&plan_output, Some(0));
    assert_eq!(plan["failed"], 0);
    assert_eq!(plan["next_after"], Value::Null);
    assert_eq!(plan["items"][0]["run_id"], run_id);
    assert_eq!(plan["items"][0]["outcome"]["kind"], "completed");
    assert_eq!(plan["items"][0]["outcome"]["result"]["status"], "planned");
    assert_eq!(plan["items"][0]["outcome"]["result"]["terminalized"], false);
    assert_eq!(
        plan["items"][0]["outcome"]["result"]["resources_absent"],
        false
    );
    assert_eq!(
        fs::read(journal).expect("journal after dry-run"),
        journal_before_plan,
        "dry-run must not advance the recovery journal"
    );

    let reconcile_output = fixture.output(&["run", "reconcile", "--all", "--limit", "1"]);
    let reconciled = run_json(&reconcile_output, Some(0));
    assert_eq!(reconciled["failed"], 0);
    assert_eq!(reconciled["items"][0]["run_id"], run_id);
    assert_eq!(
        reconciled["items"][0]["outcome"]["result"]["status"],
        "reconciled"
    );
    assert_eq!(
        reconciled["items"][0]["outcome"]["result"]["terminalized"],
        true
    );
    assert_eq!(
        reconciled["items"][0]["outcome"]["result"]["resources_absent"],
        true
    );
}

fn assert_reconciled_terminal(fixture: &NativeFixture, run_id: &str) {
    let terminal_output = fixture.output(&["run", "get", run_id]);
    let terminal = run_json(&terminal_output, Some(0));
    assert_eq!(terminal["lifecycle"], "terminal");
    assert_eq!(terminal["process"]["availability"], "unavailable");
    assert_eq!(
        terminal["process"]["error"],
        "process facts were not durably observed before recovery"
    );
    assert_eq!(terminal["stdout"]["availability"], "unavailable");
    assert_eq!(terminal["stderr"]["availability"], "unavailable");
    let final_manifest = assert_final_child(fixture, &terminal);
    let final_file = fixture.state().join("supervisor-loss-result");
    let extracted = fixture.output(&[
        "image",
        "file",
        "get",
        &final_manifest,
        "/workspace/result",
        "--output",
        final_file.to_str().expect("Final file path"),
    ]);
    assert!(extracted.status.success());
    assert_eq!(fs::read(final_file).expect("Final file"), b"changed\n");

    let repeated_output = fixture.output(&["run", "reconcile", run_id]);
    let repeated = run_json(&repeated_output, Some(0));
    assert_eq!(repeated["status"], "already_terminal");
    assert_eq!(repeated["terminalized"], false);
    assert_eq!(repeated["actions"], json!([]));
}

fn apply_preaccepted_gc_plan(fixture: &NativeFixture, plan: PreacceptedGcPlan) {
    let applied = run_json(
        &fixture.output(&[
            "state",
            "gc",
            "apply",
            plan.path.to_str().expect("GC plan path"),
        ]),
        Some(0),
    );
    assert_eq!(applied["deleted_oci_blobs"], 1);
    assert_eq!(applied["failed"], 0);
    assert!(!plan.orphan_path.exists());
    fs::remove_file(plan.path).expect("remove GC plan");
}

fn runlab(tools: &Path, scratch: &Path, state: &Path, arguments: &[&str]) -> Output {
    runlab_command(tools, scratch, state, arguments)
        .output()
        .expect("execute runlab")
}

fn spawn_runlab(tools: &Path, scratch: &Path, state: &Path, arguments: &[&str]) -> Child {
    runlab_command(tools, scratch, state, arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn runlab")
}

fn runlab_command(tools: &Path, scratch: &Path, state: &Path, arguments: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_runlab"));
    command
        .env_clear()
        .env("PATH", tools)
        .env("TMPDIR", scratch)
        .args(["--state", state.to_str().expect("state path")])
        .args(arguments);
    command
}

fn runlab_with_resolver(
    tools: &Path,
    scratch: &Path,
    state: &Path,
    resolver: &Path,
    arguments: &[&str],
) -> Output {
    Command::new(tools.join("unshare"))
        .args(["--mount", "--propagation", "private", "--"])
        .arg(state.join("fixture"))
        .arg("resolver-exec")
        .arg(resolver)
        .arg(env!("CARGO_BIN_EXE_runlab"))
        .env_clear()
        .env("PATH", tools)
        .env("TMPDIR", scratch)
        .args(["--state", state.to_str().expect("state path")])
        .args(arguments)
        .output()
        .expect("execute runlab with controlled resolver")
}

const STATIC_FIXTURE_SOURCE: &[u8] = br#"
#include <errno.h>
#include <fcntl.h>
#include <arpa/inet.h>
#include <netdb.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/mount.h>
#include <unistd.h>

static int timed_connect(const char *text, int port, int timeout_ms) {
    int socket_fd = socket(AF_INET, SOCK_STREAM, 0);
    if (socket_fd < 0) return -1;
    int flags = fcntl(socket_fd, F_GETFL, 0);
    if (flags < 0 || fcntl(socket_fd, F_SETFL, flags | O_NONBLOCK) < 0) {
        close(socket_fd);
        return -1;
    }
    struct sockaddr_in address = {0};
    address.sin_family = AF_INET;
    address.sin_port = htons((uint16_t)port);
    if (inet_pton(AF_INET, text, &address.sin_addr) != 1) {
        close(socket_fd);
        return -1;
    }
    if (connect(socket_fd, (struct sockaddr *)&address, sizeof(address)) < 0 && errno != EINPROGRESS) {
        close(socket_fd);
        return -1;
    }
    struct pollfd descriptor = {.fd = socket_fd, .events = POLLOUT};
    if (poll(&descriptor, 1, timeout_ms) != 1) {
        close(socket_fd);
        return -1;
    }
    int socket_error = 0;
    socklen_t error_size = sizeof(socket_error);
    if (getsockopt(socket_fd, SOL_SOCKET, SO_ERROR, &socket_error, &error_size) < 0 || socket_error != 0) {
        close(socket_fd);
        return -1;
    }
    if (fcntl(socket_fd, F_SETFL, flags) < 0) {
        close(socket_fd);
        return -1;
    }
    return socket_fd;
}

static int packet_server(const char *text, int port, const char *marker, int loop) {
    int server = socket(AF_INET, SOCK_STREAM, 0);
    if (server < 0) return 100;
    int enabled = 1;
    if (setsockopt(server, SOL_SOCKET, SO_REUSEADDR, &enabled, sizeof(enabled)) < 0) return 101;
    struct sockaddr_in address = {0};
    address.sin_family = AF_INET;
    address.sin_port = htons((uint16_t)port);
    if (inet_pton(AF_INET, text, &address.sin_addr) != 1) return 102;
    if (bind(server, (struct sockaddr *)&address, sizeof(address)) < 0) return 103;
    if (listen(server, 8) < 0) return 104;
    int ready = open(marker, O_CREAT | O_EXCL | O_WRONLY, 0600);
    if (ready < 0 || close(ready) < 0) return 105;
    for (;;) {
        struct sockaddr_in peer = {0};
        socklen_t peer_size = sizeof(peer);
        int client = accept(server, (struct sockaddr *)&peer, &peer_size);
        if (client < 0) return 106;
        char request[32];
        if (read(client, request, sizeof(request)) <= 0) return 107;
        if (write(client, "ok", 2) != 2) return 108;
        if (!loop) {
            char peer_text[INET_ADDRSTRLEN];
            if (inet_ntop(AF_INET, &peer.sin_addr, peer_text, sizeof(peer_text)) == 0) return 109;
            if (printf("%s\n", peer_text) < 0) return 110;
        }
        if (close(client) < 0) return 111;
        if (!loop) return close(server) == 0 ? 0 : 112;
    }
}

static int dns_server(const char *text, int port, const char *ready_marker, const char *query_marker) {
    int server = socket(AF_INET, SOCK_DGRAM, 0);
    if (server < 0) return 120;
    struct sockaddr_in address = {0};
    address.sin_family = AF_INET;
    address.sin_port = htons((uint16_t)port);
    if (inet_pton(AF_INET, text, &address.sin_addr) != 1) return 121;
    if (bind(server, (struct sockaddr *)&address, sizeof(address)) < 0) return 122;
    int ready = open(ready_marker, O_CREAT | O_EXCL | O_WRONLY, 0600);
    if (ready < 0 || close(ready) < 0) return 123;

    static const unsigned char expected_name[] = {
        5, 'a', 'g', 'e', 'n', 't',
        6, 'r', 'u', 'n', 'l', 'a', 'b',
        4, 't', 'e', 's', 't', 0
    };
    for (;;) {
        unsigned char request[512];
        struct sockaddr_in peer = {0};
        socklen_t peer_size = sizeof(peer);
        ssize_t count = recvfrom(
            server,
            request,
            sizeof(request),
            0,
            (struct sockaddr *)&peer,
            &peer_size
        );
        if (count < 12) continue;
        size_t request_size = (size_t)count;
        if (request[4] != 0 || request[5] != 1) continue;
        if (request_size < 12 + sizeof(expected_name) + 4) continue;
        if (memcmp(request + 12, expected_name, sizeof(expected_name)) != 0) continue;
        size_t question_end = 12 + sizeof(expected_name) + 4;
        uint16_t query_type = ((uint16_t)request[question_end - 4] << 8)
            | request[question_end - 3];

        unsigned char response[512];
        memcpy(response, request, question_end);
        response[2] = 0x81;
        response[3] = 0x80;
        response[6] = 0;
        response[7] = query_type == 1 ? 1 : 0;
        response[8] = 0;
        response[9] = 0;
        response[10] = 0;
        response[11] = 0;
        size_t response_size = question_end;
        if (query_type == 1) {
            static const unsigned char answer_prefix[] = {
                0xc0, 0x0c, 0x00, 0x01, 0x00, 0x01,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x04
            };
            memcpy(response + response_size, answer_prefix, sizeof(answer_prefix));
            response_size += sizeof(answer_prefix);
            memcpy(response + response_size, &address.sin_addr.s_addr, 4);
            response_size += 4;
        }
        if (sendto(
                server,
                response,
                response_size,
                0,
                (struct sockaddr *)&peer,
                peer_size
            ) != (ssize_t)response_size) return 124;
        if (query_type == 1) {
            int observed = open(query_marker, O_CREAT | O_EXCL | O_WRONLY, 0600);
            if (observed < 0 || close(observed) < 0) return 125;
            if (printf("agent.runlab.test.\n") < 0) return 126;
            return close(server) == 0 ? 0 : 127;
        }
    }
}

static int verify_runtime_identity(const char *expected_address) {
    char hostname[65] = {0};
    if (gethostname(hostname, sizeof(hostname) - 1) < 0) return 128;
    if (strcmp(hostname, "runlab-native-test") != 0) return 129;

    struct addrinfo hints = {0};
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_STREAM;
    struct addrinfo *resolved = 0;
    int status = getaddrinfo("agent.runlab.test.", 0, &hints, &resolved);
    if (status != 0 || resolved == 0) return 130;
    struct sockaddr_in *address = (struct sockaddr_in *)resolved->ai_addr;
    char text[INET_ADDRSTRLEN];
    int valid = inet_ntop(AF_INET, &address->sin_addr, text, sizeof(text)) != 0
        && strcmp(text, expected_address) == 0;
    freeaddrinfo(resolved);
    return valid ? 0 : 131;
}

int main(int argc, char **argv) {
    if (argc >= 5 && strcmp(argv[1], "resolver-exec") == 0) {
        if (mount(argv[2], "/etc/resolv.conf", 0, MS_BIND, 0) < 0) return 132;
        if (mount(
                0,
                "/etc/resolv.conf",
                0,
                MS_BIND | MS_REMOUNT | MS_RDONLY | MS_NOSUID | MS_NODEV | MS_NOEXEC,
                0
            ) < 0) return 133;
        execv(argv[3], &argv[3]);
        return 134;
    }
    if (argc == 6 && strcmp(argv[1], "dns-server") == 0) {
        return dns_server(argv[2], atoi(argv[3]), argv[4], argv[5]);
    }
    if (argc == 5 && strcmp(argv[1], "packet-server") == 0) {
        return packet_server(argv[2], atoi(argv[3]), argv[4], 0);
    }
    if (argc == 5 && strcmp(argv[1], "packet-server-loop") == 0) {
        return packet_server(argv[2], atoi(argv[3]), argv[4], 1);
    }
    if (argc == 8 && strcmp(argv[1], "egress-client") == 0) {
        int identity = verify_runtime_identity(argv[2]);
        if (identity != 0) return identity;
        int allowed = timed_connect(argv[2], atoi(argv[3]), 2000);
        if (allowed < 0) return 90;
        if (write(allowed, "egress", 6) != 6) return 91;
        char response[2];
        if (read(allowed, response, sizeof(response)) != 2 || memcmp(response, "ok", 2) != 0) return 92;
        struct sockaddr_in local = {0};
        socklen_t local_size = sizeof(local);
        if (getsockname(allowed, (struct sockaddr *)&local, &local_size) < 0) return 93;
        close(allowed);

        uint32_t local_address = ntohl(local.sin_addr.s_addr);
        struct in_addr gateway = {.s_addr = htonl(local_address - 1)};
        char gateway_text[INET_ADDRSTRLEN];
        if (inet_ntop(AF_INET, &gateway, gateway_text, sizeof(gateway_text)) == 0) return 94;
        int host = timed_connect(gateway_text, atoi(argv[4]), 700);
        if (host >= 0) {
            close(host);
            return 95;
        }

        struct in_addr first_pool;
        struct in_addr second_pool;
        if (inet_pton(AF_INET, argv[5], &first_pool) != 1) return 96;
        if (inet_pton(AF_INET, argv[6], &second_pool) != 1) return 97;
        uint32_t local_network = local_address & 0xfffffffcU;
        const char *pool_target = (ntohl(first_pool.s_addr) & 0xfffffffcU) == local_network
            ? argv[6]
            : argv[5];
        int pool = timed_connect(pool_target, atoi(argv[7]), 700);
        if (pool >= 0) {
            close(pool);
            return 98;
        }
        if (write(1, "egress-ok\n", 10) != 10) return 99;
        return 0;
    }
    if (argc == 4 && strcmp(argv[1], "egress-connect-wait") == 0) {
        int connection = timed_connect(argv[2], atoi(argv[3]), 2000);
        if (connection < 0) return 113;
        if (write(connection, "egress", 6) != 6) return 114;
        char response[2];
        if (read(connection, response, sizeof(response)) != 2 || memcmp(response, "ok", 2) != 0) return 115;
        if (mkdir("/workspace", 0755) < 0) return 116;
        int file = open("/workspace/result", O_CREAT | O_TRUNC | O_WRONLY, 0644);
        if (file < 0) return 117;
        if (write(file, "changed\n", 8) != 8) return 118;
        if (close(file) < 0) return 119;
        for (;;) pause();
    }
    if (argc == 2 && (strcmp(argv[1], "secret-read") == 0 || strcmp(argv[1], "secret-copy") == 0)) {
        int input = open("/run/credential", O_RDONLY);
        if (input < 0) return 80;
        char secret[4096];
        ssize_t count = read(input, secret, sizeof(secret));
        if (count <= 0) return 81;
        if (close(input) < 0) return 82;
        int forbidden = open("/run/credential", O_WRONLY);
        if (forbidden >= 0) {
            close(forbidden);
            return 83;
        }
        if (strcmp(argv[1], "secret-read") == 0) return 0;
        if (mkdir("/workspace", 0755) < 0) return 84;
        int output = open("/workspace/copied-credential", O_CREAT | O_TRUNC | O_WRONLY, 0600);
        if (output < 0) return 85;
        if (write(output, secret, (size_t)count) != count) return 86;
        if (close(output) < 0) return 87;
        return 0;
    }
    if (argc == 2 && strcmp(argv[1], "service") == 0) {
        int server = socket(AF_INET, SOCK_STREAM, 0);
        if (server < 0) return 60;
        struct sockaddr_in address = {0};
        address.sin_family = AF_INET;
        address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        address.sin_port = htons(15432);
        if (bind(server, (struct sockaddr *)&address, sizeof(address)) < 0) return 61;
        if (listen(server, 8) < 0) return 62;
        for (;;) {
            int client = accept(server, 0, 0);
            if (client < 0) return 63;
            char request[16] = {0};
            ssize_t count = read(client, request, sizeof(request));
            if (count >= 6 && memcmp(request, "mutate", 6) == 0) {
                if (mkdir("/service", 0755) < 0) return 64;
                int file = open("/service/value", O_CREAT | O_TRUNC | O_WRONLY, 0644);
                if (file < 0) return 65;
                if (write(file, "updated\n", 8) != 8) return 66;
                if (close(file) < 0) return 67;
                if (write(client, "ok", 2) != 2) return 68;
            }
            close(client);
        }
    }
    if (argc == 2 && strcmp(argv[1], "client") == 0) {
        int client = socket(AF_INET, SOCK_STREAM, 0);
        if (client < 0) return 70;
        struct sockaddr_in address = {0};
        address.sin_family = AF_INET;
        address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        address.sin_port = htons(15432);
        if (connect(client, (struct sockaddr *)&address, sizeof(address)) < 0) return 71;
        if (write(client, "mutate", 6) != 6) return 72;
        char response[2];
        if (read(client, response, sizeof(response)) != 2) return 73;
        if (memcmp(response, "ok", 2) != 0) return 74;
        if (mkdir("/workspace", 0755) < 0) return 75;
        int file = open("/workspace/result", O_CREAT | O_TRUNC | O_WRONLY, 0644);
        if (file < 0) return 76;
        if (write(file, "service-ok\n", 11) != 11) return 77;
        if (close(file) < 0) return 78;
        return 0;
    }
    if (argc == 2 && strcmp(argv[1], "wait") == 0) {
        for (;;) pause();
    }
    if (argc == 2 && strcmp(argv[1], "stdout") == 0) {
        char output[4096];
        memset(output, 'x', sizeof(output));
        for (;;) {
            if (write(1, output, sizeof(output)) < 0) return 30;
        }
    }
    if (argc == 2 && strcmp(argv[1], "oom") == 0) {
        for (;;) {
            volatile unsigned char *allocation = malloc(1024 * 1024);
            if (allocation == 0) return 31;
            for (size_t offset = 0; offset < 1024 * 1024; offset += 4096) {
                allocation[offset] = 1;
            }
        }
    }
    if (argc == 2 && strcmp(argv[1], "write-wait") == 0) {
        if (mkdir("/workspace", 0755) < 0) return 40;
        int file = open("/workspace/result", O_CREAT | O_TRUNC | O_WRONLY, 0644);
        if (file < 0) return 41;
        if (write(file, "changed\n", 8) != 8) return 42;
        if (close(file) < 0) return 43;
        for (;;) pause();
    }
    if (argc == 2 && strcmp(argv[1], "descendant") == 0) {
        pid_t child = fork();
        if (child < 0) return 50;
        if (child == 0) for (;;) pause();
        return 0;
    }
    char buffer[4096];
    ssize_t count;
    while ((count = read(0, buffer, sizeof(buffer))) > 0) {
        if (write(1, buffer, (size_t)count) != count) return 20;
    }
    if (count < 0) return 21;
    if (write(2, "diagnostic\n", 11) != 11) return 22;
    if (mkdir("/workspace", 0755) < 0) return 23;
    int file = open("/workspace/result", O_CREAT | O_TRUNC | O_WRONLY, 0644);
    if (file < 0) return 24;
    if (write(file, "changed\n", 8) != 8) return 25;
    if (close(file) < 0) return 26;
    return 7;
}
"#;

fn compile_static_fixture(directory: &Path) -> PathBuf {
    let source = directory.join("fixture.c");
    let executable = directory.join("fixture");
    fs::write(&source, STATIC_FIXTURE_SOURCE).expect("fixture source");
    let output = Command::new("cc")
        .args(["-static", "-O2", "-o"])
        .arg(&executable)
        .arg(&source)
        .output()
        .expect("execute static C compiler");
    assert!(
        output.status.success(),
        "static fixture compile failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    executable
}

fn write_oci_fixture(state: &Path, executable: &Path) -> String {
    let layout = state.join("oci");
    let blobs = layout.join("blobs/sha256");
    fs::create_dir_all(&blobs).expect("blob directory");
    fs::write(
        layout.join("oci-layout"),
        b"{\"imageLayoutVersion\":\"1.0.0\"}\n",
    )
    .expect("oci-layout");
    fs::write(
        layout.join("index.json"),
        b"{\"schemaVersion\":2,\"mediaType\":\"application/vnd.oci.image.index.v1+json\",\"manifests\":[]}\n",
    )
    .expect("index");

    let uncompressed = fixture_layer(executable);
    let diff_id = digest(&uncompressed);
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(6));
    encoder.write_all(&uncompressed).expect("compress Layer");
    let compressed = encoder.finish().expect("finish compression");
    let layer = put_blob(&blobs, &compressed, LAYER_MEDIA_TYPE);

    let architecture = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => panic!("unsupported fixture architecture: {other}"),
    };
    let config_bytes = serde_json::to_vec(&json!({
        "architecture": architecture,
        "os": "linux",
        "rootfs": {"type": "layers", "diff_ids": [diff_id]},
        "config": {"Entrypoint": ["/agent"], "Env": [], "WorkingDir": "/"},
        "history": []
    }))
    .expect("config JSON");
    let config = put_blob(&blobs, &config_bytes, CONFIG_MEDIA_TYPE);
    let manifest_bytes = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": MANIFEST_MEDIA_TYPE,
        "config": config,
        "layers": [layer]
    }))
    .expect("Manifest JSON");
    let manifest = put_blob(&blobs, &manifest_bytes, MANIFEST_MEDIA_TYPE);
    manifest["digest"]
        .as_str()
        .expect("Manifest digest")
        .to_owned()
}

fn fixture_layer(executable: &Path) -> Vec<u8> {
    let mut tar = Builder::new(Vec::new());
    let executable = fs::read(executable).expect("fixture executable");
    append_fixture_entry(&mut tar, "agent", EntryType::Regular, 0o755, &executable);
    append_fixture_entry(&mut tar, "etc", EntryType::Directory, 0o755, b"");
    append_fixture_entry(&mut tar, "etc/resolv.conf", EntryType::Regular, 0o644, b"");
    append_fixture_entry(
        &mut tar,
        "etc/nsswitch.conf",
        EntryType::Regular,
        0o644,
        b"hosts: dns\n",
    );
    append_fixture_entry(&mut tar, "run/credential", EntryType::Regular, 0o600, b"");
    tar.finish().expect("finish Layer tar");
    tar.into_inner().expect("Layer tar bytes")
}

fn append_fixture_entry(
    tar: &mut Builder<Vec<u8>>,
    path: &str,
    entry_type: EntryType,
    mode: u32,
    bytes: &[u8],
) {
    let mut header = Header::new_gnu();
    header.set_entry_type(entry_type);
    header.set_mode(mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_size(bytes.len() as u64);
    header.set_cksum();
    tar.append_data(&mut header, path, bytes)
        .unwrap_or_else(|error| panic!("append fixture Layer entry {path}: {error}"));
}

fn put_blob(directory: &Path, bytes: &[u8], media_type: &str) -> Value {
    let digest = digest(bytes);
    fs::write(directory.join(&digest[7..]), bytes).expect("blob");
    json!({"mediaType": media_type, "digest": digest, "size": bytes.len()})
}

fn digest(bytes: &[u8]) -> String {
    let mut value = String::from("sha256:");
    for byte in Sha256::digest(bytes) {
        use std::fmt::Write as _;
        write!(&mut value, "{byte:02x}").expect("write digest");
    }
    value
}

fn runtime_config(arguments: &[&str]) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "ociVersion": "1.2.0",
        "root": {"path": "rootfs", "readonly": false},
        "process": {
            "terminal": false,
            "user": {"uid": 0, "gid": 0},
            "args": arguments,
            "env": [],
            "cwd": "/",
            "noNewPrivileges": true
        },
        "hostname": "runlab-native-test",
        "mounts": [
            {
                "destination": "/proc",
                "type": "proc",
                "source": "proc",
                "options": ["nosuid", "noexec", "nodev"]
            },
            {
                "destination": "/dev",
                "type": "tmpfs",
                "source": "tmpfs",
                "options": ["nosuid", "strictatime", "mode=755", "size=65536k"]
            },
            {
                "destination": "/dev/pts",
                "type": "devpts",
                "source": "devpts",
                "options": ["nosuid", "noexec", "newinstance", "ptmxmode=0666", "mode=0620", "gid=5"]
            },
            {
                "destination": "/dev/shm",
                "type": "tmpfs",
                "source": "shm",
                "options": ["nosuid", "noexec", "nodev", "mode=1777", "size=65536k"]
            },
            {
                "destination": "/dev/mqueue",
                "type": "mqueue",
                "source": "mqueue",
                "options": ["nosuid", "noexec", "nodev"]
            }
        ],
        "linux": {
            "namespaces": [
                {"type": "pid"},
                {"type": "network"},
                {"type": "ipc"},
                {"type": "uts"},
                {"type": "mount"},
                {"type": "cgroup"}
            ]
        }
    }))
    .expect("runtime JSON")
}

fn runtime_config_with_file_mount(arguments: &[&str], source: &Path) -> Vec<u8> {
    let mut config: Value =
        serde_json::from_slice(&runtime_config(arguments)).expect("Runtime Config JSON");
    config["mounts"]
        .as_array_mut()
        .expect("standard mounts")
        .push(json!({
            "destination": "/run/credential",
            "type": "bind",
            "source": source,
            "options": ["bind", "ro", "nosuid", "nodev", "noexec"]
        }));
    serde_json::to_vec(&config).expect("Runtime Config bytes")
}

fn runtime_config_with_memory_limit(arguments: &[&str], limit: i64) -> Vec<u8> {
    let mut config: Value =
        serde_json::from_slice(&runtime_config(arguments)).expect("Runtime Config JSON");
    config["linux"]["resources"] = json!({
        "memory": {"limit": limit, "swap": limit}
    });
    serde_json::to_vec(&config).expect("Runtime Config bytes")
}

fn runtime_config_managed(arguments: &[&str]) -> Vec<u8> {
    let mut config: Value =
        serde_json::from_slice(&runtime_config(arguments)).expect("Runtime Config JSON");
    config["linux"]["namespaces"]
        .as_array_mut()
        .expect("namespace array")
        .retain(|namespace| namespace["type"] != "network");
    serde_json::to_vec(&config).expect("managed Runtime Config JSON")
}

fn write_postgres_runtime(source: &Path, target: &Path, command: Option<&str>) {
    let mut config: Value =
        serde_json::from_slice(&fs::read(source).expect("generated Runtime Config"))
            .expect("Runtime Config JSON");
    config["linux"]["namespaces"]
        .as_array_mut()
        .expect("namespace array")
        .retain(|namespace| namespace["type"] != "network");
    let environment = config["process"]["env"]
        .as_array_mut()
        .expect("process environment");
    environment.push(Value::String("POSTGRES_HOST_AUTH_METHOD=trust".to_owned()));
    if let Some(command) = command {
        config["process"]["args"] = if command == "/bin/true" {
            json!(["/bin/true"])
        } else {
            json!(["/bin/sh", "-ec", command])
        };
    }
    fs::write(
        target,
        serde_json::to_vec(&config).expect("Runtime Config bytes"),
    )
    .expect("write PostgreSQL Runtime Config");
}

fn execute_postgres_case(
    fixture: &NativeFixture,
    primary_manifest: &str,
    service_manifest: &str,
    primary_runtime: &Path,
    service_runtime: &Path,
    label: &str,
) -> Value {
    let declaration = fixture.state().join(format!("postgres-{label}.json"));
    fs::write(
        &declaration,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "name": "postgres",
            "initial_manifest": service_manifest,
            "runtime_config_file": service_runtime,
            "readiness": {"kind": "tcp", "port": 5432, "timeout_seconds": 30}
        }))
        .expect("PostgreSQL declaration"),
    )
    .expect("write PostgreSQL declaration");
    let output = fixture.output(&[
        "run",
        "start",
        primary_manifest,
        "--runtime-config",
        primary_runtime.to_str().expect("Primary Runtime Config"),
        "--managed-service",
        declaration.to_str().expect("Managed Service declaration"),
        "--timeout-seconds",
        "60",
    ]);
    let result = run_json(&output, Some(0));
    assert_eq!(
        result["managed_service"]["readiness"]["outcome"], "ready",
        "PostgreSQL did not become ready: {result}"
    );
    assert_eq!(
        result["managed_service"]["final_image"]["availability"],
        "available"
    );
    result
}

fn managed_final_manifest(result: &Value) -> String {
    result["managed_service"]["final_image"]["manifest"]["digest"]
        .as_str()
        .expect("Managed Service Final Manifest")
        .to_owned()
}

fn host_platform_name() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "linux/arm64",
        "x86_64" => "linux/amd64",
        _ => panic!("unsupported test architecture"),
    }
}

fn write_runtime(directory: &Path, name: &str, arguments: &[&str]) -> PathBuf {
    let path = directory.join(name);
    fs::write(&path, runtime_config(arguments)).expect("runtime config");
    path
}

fn run_json(output: &Output, expected_code: Option<i32>) -> Value {
    assert_eq!(
        output.status.code(),
        expected_code,
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("run result JSON")
}

fn inspect_image(tools: &Path, scratch: &Path, state: &Path, manifest: &str) -> Value {
    let output = runlab(tools, scratch, state, &["image", "inspect", manifest]);
    run_json(&output, Some(0))
}

fn assert_final_child(fixture: &NativeFixture, result: &Value) -> String {
    assert_eq!(result["final_image"]["availability"], "available");
    let manifest = result["final_image"]["manifest"]["digest"]
        .as_str()
        .expect("Final Manifest");
    let final_image = inspect_image(
        fixture.tool_dir.path(),
        fixture.temp_dir.path(),
        fixture.state(),
        manifest,
    );
    let initial_layers = fixture.base_image["layers"]
        .as_array()
        .expect("Initial Layers");
    let final_layers = final_image["layers"].as_array().expect("Final Layers");
    assert_eq!(final_layers.len(), initial_layers.len() + 1);
    assert_eq!(
        &final_layers[..initial_layers.len()],
        initial_layers,
        "Initial Layers must be an exact Final prefix"
    );
    manifest.to_owned()
}

fn assert_image_file(
    fixture: &NativeFixture,
    manifest: &str,
    source: &str,
    expected: &[u8],
    output_name: &str,
) {
    let output_path = fixture.state().join(output_name);
    let output = fixture.output(&[
        "image",
        "file",
        "get",
        manifest,
        source,
        "--output",
        output_path.to_str().expect("image output path"),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(output_path).expect("image file"), expected);
}

fn assert_stream(fixture: &NativeFixture, run_id: &str, name: &str, expected: &[u8]) {
    let output_path = fixture.state().join(format!("{run_id}-{name}"));
    let output = fixture.output(&[
        "run",
        name,
        "get",
        run_id,
        "--output",
        output_path.to_str().expect("stream output path"),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(output_path).expect("stream bytes"), expected);
}

fn assert_last_layer_lacks_path(fixture: &NativeFixture, result: &Value, unexpected: &str) {
    let layers = result["final_image"]["manifest"]["digest"]
        .as_str()
        .map(|manifest| {
            inspect_image(
                fixture.tool_dir.path(),
                fixture.temp_dir.path(),
                fixture.state(),
                manifest,
            )
        })
        .expect("Final Manifest");
    let layer = layers["layers"]
        .as_array()
        .and_then(|layers| layers.last())
        .and_then(|layer| layer["digest"].as_str())
        .expect("Final Layer digest");
    let bytes = fs::read(fixture.state().join("oci/blobs/sha256").join(&layer[7..]))
        .expect("Final Layer blob");
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()));
    let paths = archive
        .entries()
        .expect("Final Layer entries")
        .map(|entry| {
            entry
                .expect("Final Layer entry")
                .path()
                .expect("Final Layer path")
                .into_owned()
        })
        .collect::<Vec<_>>();
    let unexpected = Path::new(unexpected);
    let whiteout = unexpected
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(format!(
            ".wh.{}",
            unexpected
                .file_name()
                .expect("temporary execution path basename")
                .to_string_lossy()
        ));
    assert!(
        !paths
            .iter()
            .any(|path| path == unexpected || path == &whiteout),
        "temporary execution path leaked into Final Layer: {paths:?}"
    );
}

fn assert_bytes_absent_below(root: &Path, unexpected: &[u8]) {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(&path).expect("state directory") {
            let entry = entry.expect("state entry");
            let file_type = entry.file_type().expect("state entry type");
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                let bytes = fs::read(entry.path()).expect("state file");
                assert!(
                    !bytes
                        .windows(unexpected.len())
                        .any(|window| window == unexpected),
                    "sensitive bytes leaked into {}",
                    entry.path().display()
                );
            }
        }
    }
}

fn wait_for_init(child: &mut Child, state: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let attempts = state.join("recovery/native");
        let run_id = fs::read_dir(attempts).ok().and_then(|entries| {
            entries.filter_map(Result::ok).find_map(|entry| {
                entry
                    .path()
                    .join("workspace/runtime/init.pid")
                    .is_file()
                    .then(|| entry.file_name().to_string_lossy().into_owned())
            })
        });
        if let Some(run_id) = run_id {
            return run_id;
        }
        if let Some(status) = child.try_wait().expect("poll runlab") {
            panic!("runlab exited before runc init was observable: {status}");
        }
        thread::sleep(Duration::from_millis(20));
    }
    child.kill().expect("kill stuck runlab");
    child.wait().expect("reap stuck runlab");
    panic!("runc init was not observable within 30 seconds");
}

fn wait_for_managed_init(child: &mut Child, state: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let attempts = state.join("recovery/native");
        let run_id = fs::read_dir(attempts).ok().and_then(|entries| {
            entries.filter_map(Result::ok).find_map(|entry| {
                entry
                    .path()
                    .join("workspace/managed-service/runtime/init.pid")
                    .is_file()
                    .then(|| entry.file_name().to_string_lossy().into_owned())
            })
        });
        if let Some(run_id) = run_id {
            return run_id;
        }
        if let Some(status) = child.try_wait().expect("poll runlab") {
            panic!("runlab exited before Managed Service init was observable: {status}");
        }
        thread::sleep(Duration::from_millis(20));
    }
    child.kill().expect("kill stuck runlab");
    child.wait().expect("reap stuck runlab");
    panic!("Managed Service init was not observable within 30 seconds");
}

fn wait_for_path(child: &mut Child, path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if path.is_file() {
            return;
        }
        if let Some(status) = child.try_wait().expect("poll runlab") {
            panic!(
                "runlab exited before {} was observable: {status}",
                path.display()
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
    child.kill().expect("kill stuck runlab");
    child.wait().expect("reap stuck runlab");
    panic!("{} was not observable within 30 seconds", path.display());
}

fn interrupt(child: &Child) {
    signal(child, Signal::INT);
}

fn signal(child: &Child, signal: Signal) {
    let raw_pid = i32::try_from(child.id()).expect("runlab pid fits i32");
    let pid = Pid::from_raw(raw_pid).expect("runlab pid is positive");
    kill_process(pid, signal).expect("signal runlab");
}

#[derive(Debug, Eq, PartialEq)]
enum StateTreeEntry {
    Directory(u32),
    File(u32, Vec<u8>),
    Symlink(Vec<u8>),
}

fn snapshot_state_tree(root: &Path) -> BTreeMap<PathBuf, StateTreeEntry> {
    fn visit(root: &Path, path: &Path, snapshot: &mut BTreeMap<PathBuf, StateTreeEntry>) {
        let metadata = fs::symlink_metadata(path).expect("state metadata");
        let relative = path.strip_prefix(root).expect("state-relative path");
        let mode = metadata.permissions().mode() & 0o7777;
        if metadata.is_dir() {
            snapshot.insert(relative.to_path_buf(), StateTreeEntry::Directory(mode));
            let mut children = fs::read_dir(path)
                .expect("state directory")
                .map(|entry| entry.expect("state entry").path())
                .collect::<Vec<_>>();
            children.sort();
            for child in children {
                visit(root, &child, snapshot);
            }
        } else if metadata.is_file() {
            snapshot.insert(
                relative.to_path_buf(),
                StateTreeEntry::File(mode, fs::read(path).expect("state file")),
            );
        } else if metadata.file_type().is_symlink() {
            snapshot.insert(
                relative.to_path_buf(),
                StateTreeEntry::Symlink(
                    fs::read_link(path)
                        .expect("state symlink")
                        .into_os_string()
                        .into_vec(),
                ),
            );
        } else {
            panic!("unsupported state entry: {}", path.display());
        }
    }

    let mut snapshot = BTreeMap::new();
    visit(root, root, &mut snapshot);
    snapshot
}

fn assert_state_tree_unchanged(root: &Path, before: &BTreeMap<PathBuf, StateTreeEntry>) {
    let after = snapshot_state_tree(root);
    let before_paths = before.keys().collect::<Vec<_>>();
    let after_paths = after.keys().collect::<Vec<_>>();
    assert_eq!(after_paths, before_paths, "dry-run changed state paths");
    for (path, expected) in before {
        let actual = after
            .get(path)
            .expect("state path exists in both snapshots");
        assert!(
            actual == expected,
            "dry-run changed {}: {} -> {}",
            path.display(),
            state_tree_entry_summary(expected),
            state_tree_entry_summary(actual)
        );
    }
}

fn state_tree_entry_summary(entry: &StateTreeEntry) -> String {
    match entry {
        StateTreeEntry::Directory(mode) => format!("directory mode={mode:o}"),
        StateTreeEntry::File(mode, bytes) => {
            format!("file mode={mode:o} size={} {}", bytes.len(), digest(bytes))
        }
        StateTreeEntry::Symlink(target) => {
            format!("symlink target-bytes-{}", digest(target))
        }
    }
}

fn assert_native_cleanup(scratch: &Path, state: &Path) {
    let entries = fs::read_dir(scratch)
        .expect("scratch entries")
        .map(|entry| entry.expect("scratch entry").path())
        .collect::<Vec<_>>();
    assert!(
        entries.is_empty(),
        "native temporary resources remain: {entries:?}"
    );

    let mounts = fs::read_to_string("/proc/self/mountinfo")
        .expect("mountinfo")
        .lines()
        .filter_map(|line| line.split_whitespace().nth(4))
        .map(decode_mount_path)
        .filter(|path| path.starts_with(scratch) || path.starts_with(state))
        .collect::<Vec<_>>();
    assert!(mounts.is_empty(), "native mounts remain: {mounts:?}");

    let attempts = fs::read_dir(state.join("recovery/native"))
        .expect("native recovery directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    assert!(attempts.is_empty(), "native attempts remain: {attempts:?}");

    let own_cgroup = fs::read_to_string("/proc/self/cgroup")
        .expect("self cgroup")
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .expect("unified cgroup path")
        .to_owned();
    let parent = Path::new(&own_cgroup).parent().unwrap_or(Path::new("/"));
    let cgroup_parent = Path::new("/sys/fs/cgroup").join(
        parent
            .strip_prefix("/")
            .expect("absolute unified cgroup path"),
    );
    let cgroups = fs::read_dir(cgroup_parent)
        .expect("runc cgroup parent")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().as_encoded_bytes().starts_with(b"runlab-"))
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    assert!(cgroups.is_empty(), "native cgroups remain: {cgroups:?}");
}

fn decode_mount_path(value: &str) -> PathBuf {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() {
            let digits = &bytes[index + 1..index + 4];
            if digits.iter().all(|digit| matches!(*digit, b'0'..=b'7')) {
                decoded.push((digits[0] - b'0') * 64 + (digits[1] - b'0') * 8 + digits[2] - b'0');
                index += 4;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    PathBuf::from(std::ffi::OsString::from_vec(decoded))
}

fn required_path(name: &str) -> PathBuf {
    let path =
        PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("{name} is required")));
    assert!(path.is_absolute(), "{name} must be absolute");
    path
}
