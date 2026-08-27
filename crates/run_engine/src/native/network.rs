use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use run_protocol::{EngineError, InputPath};

use super::subprocess::{HelperOutput, InvocationSupervisor, run_helper_until};
use crate::CancellationToken;

pub(super) const HOST_ADDRESS: &str = "169.254.254.1";
const GUEST_ADDRESS_POOL: &str = "10.240.0.0/12";

#[derive(Clone)]
pub(super) struct EgressTools {
    ip: PathBuf,
    iptables: PathBuf,
    ip6tables: PathBuf,
    nsenter: PathBuf,
}

impl EgressTools {
    pub(super) fn preflight(
        supervisor: &InvocationSupervisor,
        deadline: Instant,
    ) -> Result<Self, EngineError> {
        let tools = Self {
            ip: find_executable("ip")?,
            iptables: find_executable("iptables")?,
            ip6tables: find_executable("ip6tables")?,
            nsenter: find_executable("nsenter")?,
        };
        let forwarding = fs::read_to_string("/proc/sys/net/ipv4/ip_forward")
            .map_err(|error| unsupported(format!("cannot inspect IPv4 forwarding: {error}")))?;
        if forwarding.trim() != "1" {
            return Err(unsupported(
                "host IPv4 forwarding is disabled; NativeEngine will not change this host-wide setting",
            ));
        }
        for command in [
            CommandSpec::new(&tools.ip, ["-Version"]),
            CommandSpec::new(&tools.iptables, ["-w", "2", "-t", "nat", "-S"]),
            CommandSpec::new(&tools.iptables, ["-w", "2", "-S"]),
            CommandSpec::new(&tools.ip6tables, ["-w", "2", "-S"]),
            CommandSpec::new(&tools.nsenter, ["--version"]),
        ] {
            run(&command, supervisor, deadline, None)
                .map_err(|error| unsupported(format!("egress capability probe failed: {error}")))?;
        }
        Ok(tools)
    }

    pub(super) fn plan(&self, process_id: u32, sequence: u64) -> EgressPlan {
        let slot = (u64::from(process_id)
            .wrapping_mul(1_048_583)
            .wrapping_add(sequence)
            % 1_048_574)
            + 1;
        let guest_address = format!(
            "10.{}.{}.{}",
            240 + ((slot >> 16) & 0x0f),
            (slot >> 8) & 0xff,
            slot & 0xff
        );
        let suffix = format!(
            "{:05x}{:07x}",
            process_id & 0x0f_ffff,
            sequence & 0x0fff_ffff
        );
        EgressPlan {
            tools: self.clone(),
            host_interface: format!("rle{suffix}"),
            peer_interface: format!("rlp{suffix}"),
            guest_address,
        }
    }
}

#[derive(Clone)]
pub(super) struct EgressPlan {
    tools: EgressTools,
    host_interface: String,
    peer_interface: String,
    guest_address: String,
}

pub(super) struct EgressNetwork {
    plan: EgressPlan,
    link_created: bool,
    cleanup: Vec<CommandSpec>,
}

impl EgressNetwork {
    pub(super) fn new(plan: EgressPlan) -> Self {
        Self {
            plan,
            link_created: false,
            cleanup: Vec::new(),
        }
    }

    pub(super) fn setup(
        &mut self,
        init_pid: u32,
        supervisor: &InvocationSupervisor,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<(), NetworkCommandError> {
        let host = self.plan.host_interface.clone();
        let peer = self.plan.peer_interface.clone();
        let guest = self.plan.guest_address.clone();
        let guest_cidr = format!("{guest}/32");
        let host_cidr = format!("{HOST_ADDRESS}/32");
        let pid = init_pid.to_string();

        run(
            &CommandSpec::new(
                &self.plan.tools.ip,
                ["link", "add", &host, "type", "veth", "peer", "name", &peer],
            ),
            supervisor,
            deadline,
            Some(cancellation),
        )?;
        self.link_created = true;

        for (apply, cleanup) in firewall_rules(&self.plan) {
            self.apply(&apply, cleanup, supervisor, deadline, cancellation)?;
        }

        for command in [
            CommandSpec::new(&self.plan.tools.ip, ["link", "set", &peer, "netns", &pid]),
            namespaced(&self.plan, &pid, ["link", "set", "lo", "up"]),
            namespaced(&self.plan, &pid, ["link", "set", &peer, "name", "eth0"]),
            namespaced(
                &self.plan,
                &pid,
                ["address", "add", &guest_cidr, "dev", "eth0"],
            ),
            CommandSpec::new(
                &self.plan.tools.ip,
                ["address", "add", &host_cidr, "dev", &host],
            ),
            CommandSpec::new(&self.plan.tools.ip, ["link", "set", &host, "up"]),
            CommandSpec::new(
                &self.plan.tools.ip,
                ["route", "add", &guest_cidr, "dev", &host],
            ),
            namespaced(&self.plan, &pid, ["link", "set", "eth0", "up"]),
            namespaced(
                &self.plan,
                &pid,
                ["route", "add", &host_cidr, "dev", "eth0"],
            ),
            namespaced(
                &self.plan,
                &pid,
                ["route", "add", "default", "via", HOST_ADDRESS],
            ),
        ] {
            run(&command, supervisor, deadline, Some(cancellation))?;
        }
        Ok(())
    }

    fn apply(
        &mut self,
        command: &CommandSpec,
        cleanup: CommandSpec,
        supervisor: &InvocationSupervisor,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<(), NetworkCommandError> {
        run(command, supervisor, deadline, Some(cancellation))?;
        self.cleanup.push(cleanup);
        Ok(())
    }

    pub(super) fn cleanup(
        &mut self,
        supervisor: &InvocationSupervisor,
        deadline: Instant,
    ) -> Vec<NetworkCleanupIssue> {
        let mut issues = Vec::new();
        while let Some(command) = self.cleanup.pop() {
            if let Err(error) = run(&command, supervisor, deadline, None) {
                issues.push(NetworkCleanupIssue {
                    message: format!("failed to remove egress network resource: {error}"),
                    supervisor_reaped: error.supervisor_reaped,
                });
            }
        }
        if self.link_created {
            let command = CommandSpec::new(
                &self.plan.tools.ip,
                ["link", "delete", self.plan.host_interface.as_str()],
            );
            if let Err(error) = run(&command, supervisor, deadline, None)
                && self.host_interface_exists()
            {
                issues.push(NetworkCleanupIssue {
                    message: format!("failed to remove egress network resource: {error}"),
                    supervisor_reaped: error.supervisor_reaped,
                });
            }
            self.link_created = self.host_interface_exists();
        }
        issues
    }

    fn host_interface_exists(&self) -> bool {
        Path::new("/sys/class/net")
            .join(&self.plan.host_interface)
            .exists()
    }
}

pub(super) struct NetworkCommandError {
    message: String,
    pub(super) supervisor_reaped: bool,
}

impl std::fmt::Display for NetworkCommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

pub(super) struct NetworkCleanupIssue {
    pub(super) message: String,
    pub(super) supervisor_reaped: bool,
}

#[derive(Clone)]
struct CommandSpec {
    program: PathBuf,
    arguments: Vec<OsString>,
}

impl CommandSpec {
    fn new<I, S>(program: impl AsRef<Path>, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self {
            program: program.as_ref().to_path_buf(),
            arguments: arguments
                .into_iter()
                .map(|argument| argument.as_ref().to_os_string())
                .collect(),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.arguments);
        command
    }
}

fn firewall_rules(plan: &EgressPlan) -> Vec<(CommandSpec, CommandSpec)> {
    let host = plan.host_interface.as_str();
    let guest_cidr = format!("{}/32", plan.guest_address);
    let mut rules = vec![rule(
        &plan.tools.iptables,
        Some("nat"),
        "POSTROUTING",
        1,
        ["-s", &guest_cidr, "-j", "MASQUERADE"],
    )];
    rules.extend(ipv4_filter_rules(&plan.tools.iptables, host));
    rules.extend(ipv6_filter_rules(&plan.tools.ip6tables, host));
    rules
}

fn ipv4_filter_rules(executable: &Path, host: &str) -> [(CommandSpec, CommandSpec); 6] {
    [
        rule(
            executable,
            None,
            "FORWARD",
            1,
            ["-i", host, "-d", GUEST_ADDRESS_POOL, "-j", "DROP"],
        ),
        rule(executable, None, "FORWARD", 2, ["-i", host, "-j", "ACCEPT"]),
        established_rule(executable, "FORWARD", 3, host),
        rule(executable, None, "FORWARD", 4, ["-o", host, "-j", "DROP"]),
        established_rule(executable, "OUTPUT", 1, host),
        rule(executable, None, "OUTPUT", 2, ["-o", host, "-j", "DROP"]),
    ]
}

fn ipv6_filter_rules(executable: &Path, host: &str) -> [(CommandSpec, CommandSpec); 5] {
    [
        rule(executable, None, "FORWARD", 1, ["-i", host, "-j", "ACCEPT"]),
        established_rule(executable, "FORWARD", 2, host),
        rule(executable, None, "FORWARD", 3, ["-o", host, "-j", "DROP"]),
        established_rule(executable, "OUTPUT", 1, host),
        rule(executable, None, "OUTPUT", 2, ["-o", host, "-j", "DROP"]),
    ]
}

fn established_rule(
    executable: &Path,
    chain: &str,
    position: usize,
    host: &str,
) -> (CommandSpec, CommandSpec) {
    rule(
        executable,
        None,
        chain,
        position,
        [
            "-o",
            host,
            "-m",
            "conntrack",
            "--ctstate",
            "ESTABLISHED,RELATED",
            "-j",
            "ACCEPT",
        ],
    )
}

fn rule<const N: usize>(
    executable: &Path,
    table: Option<&str>,
    chain: &str,
    position: usize,
    body: [&str; N],
) -> (CommandSpec, CommandSpec) {
    let mut prefix = vec![OsString::from("-w"), OsString::from("2")];
    if let Some(table) = table {
        prefix.extend([OsString::from("-t"), OsString::from(table)]);
    }
    let mut apply = prefix.clone();
    apply.extend([
        OsString::from("-I"),
        OsString::from(chain),
        OsString::from(position.to_string()),
    ]);
    apply.extend(body.iter().map(OsString::from));
    let mut cleanup = prefix;
    cleanup.extend([OsString::from("-D"), OsString::from(chain)]);
    cleanup.extend(body.iter().map(OsString::from));
    (
        CommandSpec::new(executable, apply),
        CommandSpec::new(executable, cleanup),
    )
}

fn namespaced<const N: usize>(plan: &EgressPlan, pid: &str, arguments: [&str; N]) -> CommandSpec {
    let mut values = vec![
        OsString::from("-t"),
        OsString::from(pid),
        OsString::from("-n"),
        OsString::from("--"),
        plan.tools.ip.as_os_str().to_os_string(),
    ];
    values.extend(arguments.iter().map(OsString::from));
    CommandSpec::new(&plan.tools.nsenter, values)
}

fn run(
    spec: &CommandSpec,
    supervisor: &InvocationSupervisor,
    deadline: Instant,
    cancellation: Option<&CancellationToken>,
) -> Result<HelperOutput, NetworkCommandError> {
    let output = run_helper_until(supervisor, &mut spec.command(), deadline, cancellation)
        .map_err(|error| NetworkCommandError {
            message: format!("{}: {error}", describe(spec)),
            supervisor_reaped: error.supervisor_reaped,
        })?;
    if output.status.success() {
        return Ok(output);
    }
    Err(NetworkCommandError {
        message: format!(
            "{} exited with {}; stdout: {}; stderr: {}",
            describe(spec),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
        supervisor_reaped: true,
    })
}

fn describe(spec: &CommandSpec) -> String {
    std::iter::once(spec.program.as_os_str())
        .chain(spec.arguments.iter().map(OsString::as_os_str))
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

fn find_executable(name: &str) -> Result<PathBuf, EngineError> {
    let path = env::var_os("PATH")
        .ok_or_else(|| unsupported(format!("PATH is unavailable while locating {name}")))?;
    for directory in env::split_paths(&path) {
        let candidate = directory.join(name);
        let Ok(metadata) = fs::metadata(&candidate) else {
            continue;
        };
        if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
            return if candidate.is_absolute() {
                Ok(candidate)
            } else {
                env::current_dir()
                    .map(|current| current.join(candidate))
                    .map_err(|error| unsupported(format!("cannot resolve {name}: {error}")))
            };
        }
    }
    Err(unsupported(format!(
        "required egress helper is unavailable in PATH: {name}"
    )))
}

fn unsupported(reason: impl Into<String>) -> EngineError {
    EngineError::unsupported(InputPath::field("network"), reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_use_unique_bounded_interface_names_and_guest_addresses() {
        let tools = EgressTools {
            ip: "/ip".into(),
            iptables: "/iptables".into(),
            ip6tables: "/ip6tables".into(),
            nsenter: "/nsenter".into(),
        };
        let first = tools.plan(42, 1);
        let second = tools.plan(42, 2);
        assert_ne!(first.host_interface, second.host_interface);
        assert_ne!(first.guest_address, second.guest_address);
        assert!(first.host_interface.len() <= 15);
        assert!(first.peer_interface.len() <= 15);
        assert!(first.guest_address.starts_with("10."));
    }

    #[test]
    fn every_applied_firewall_rule_has_an_exact_delete() {
        let tools = EgressTools {
            ip: "/ip".into(),
            iptables: "/iptables".into(),
            ip6tables: "/ip6tables".into(),
            nsenter: "/nsenter".into(),
        };
        for (apply, cleanup) in firewall_rules(&tools.plan(42, 1)) {
            let insert = apply
                .arguments
                .iter()
                .position(|argument| argument == "-I")
                .expect("insert operation");
            let delete = cleanup
                .arguments
                .iter()
                .position(|argument| argument == "-D")
                .expect("delete operation");
            assert_eq!(&apply.arguments[..insert], &cleanup.arguments[..delete]);
            assert_eq!(apply.arguments[insert + 1], cleanup.arguments[delete + 1]);
            assert_eq!(
                &apply.arguments[insert + 3..],
                &cleanup.arguments[delete + 2..]
            );
        }
    }

    #[test]
    fn ipv4_egress_cannot_target_another_run_internal_address() {
        let rules = ipv4_filter_rules(Path::new("/iptables"), "rle-test");
        let pool_drop = describe(&rules[0].0);
        let outbound_accept = describe(&rules[1].0);
        assert!(pool_drop.contains("-I FORWARD 1 -i rle-test -d 10.240.0.0/12 -j DROP"));
        assert!(outbound_accept.contains("-I FORWARD 2 -i rle-test -j ACCEPT"));
    }
}
