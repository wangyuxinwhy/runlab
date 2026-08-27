use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use oci_spec::image::ImageConfiguration;
use serde_json::{Value, json};

const DEFAULT_ENV: [&str; 2] = [
    "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    "TERM=xterm",
];

pub(crate) fn generate(image: &ImageConfiguration) -> Result<Vec<u8>> {
    let image_defaults = image.config().as_ref();
    let entrypoint = image_defaults
        .and_then(|config| config.entrypoint().as_ref())
        .into_iter()
        .flatten();
    let command = image_defaults
        .and_then(|config| config.cmd().as_ref())
        .into_iter()
        .flatten();
    let mut args = entrypoint.chain(command).cloned().collect::<Vec<_>>();
    if args.is_empty() {
        args.push("sh".to_owned());
    }

    let env = image_defaults
        .and_then(|config| config.env().clone())
        .unwrap_or_else(|| DEFAULT_ENV.iter().map(ToString::to_string).collect());
    let cwd = image_defaults
        .and_then(|config| config.working_dir().as_deref())
        .filter(|value| !value.is_empty())
        .unwrap_or("/");
    if !Path::new(cwd).is_absolute() {
        bail!("OCI Image Config WorkingDir must be absolute: {cwd}");
    }
    let user = process_user(
        image_defaults
            .and_then(|config| config.user().as_deref())
            .unwrap_or(""),
    )?;

    let mut mounts = standard_mounts();
    if let Some(resolver) = resolver_source() {
        mounts.push(json!({
            "destination": "/etc/resolv.conf",
            "type": "bind",
            "source": resolver,
            "options": ["rbind", "ro", "nosuid", "noexec", "nodev"]
        }));
    }

    let value = json!({
        "ociVersion": "1.3.0",
        "root": {"path": "rootfs", "readonly": false},
        "process": {
            "terminal": false,
            "user": user,
            "args": args,
            "env": env,
            "cwd": cwd,
            "noNewPrivileges": true,
            "capabilities": {
                "bounding": [],
                "effective": [],
                "inheritable": [],
                "permitted": [],
                "ambient": []
            }
        },
        "hostname": "runlab",
        "mounts": mounts,
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
    });
    let mut bytes = serde_json::to_vec(&value).context("failed to encode Runtime Configuration")?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn standard_mounts() -> Vec<Value> {
    vec![
        json!({"destination": "/proc", "type": "proc", "source": "proc"}),
        json!({
            "destination": "/dev",
            "type": "tmpfs",
            "source": "tmpfs",
            "options": ["nosuid", "strictatime", "mode=755", "size=65536k"]
        }),
        json!({
            "destination": "/dev/pts",
            "type": "devpts",
            "source": "devpts",
            "options": ["nosuid", "noexec", "newinstance", "ptmxmode=0666", "mode=0620", "gid=5"]
        }),
        json!({
            "destination": "/dev/shm",
            "type": "tmpfs",
            "source": "shm",
            "options": ["nosuid", "noexec", "nodev", "mode=1777", "size=65536k"]
        }),
        json!({
            "destination": "/dev/mqueue",
            "type": "mqueue",
            "source": "mqueue",
            "options": ["nosuid", "noexec", "nodev"]
        }),
        json!({
            "destination": "/sys",
            "type": "sysfs",
            "source": "sysfs",
            "options": ["nosuid", "noexec", "nodev", "ro"]
        }),
    ]
}

fn process_user(raw: &str) -> Result<Value> {
    if raw.is_empty() || raw == "root" || raw == "0" {
        return Ok(json!({"uid": 0, "gid": 0}));
    }
    if let Some((user, group)) = raw.split_once(':') {
        let uid = parse_id(user, "user", raw)?;
        let gid = parse_id(group, "group", raw)?;
        return Ok(json!({"uid": uid, "gid": gid}));
    }
    bail!(
        "OCI Image Config User must be root or numeric uid:gid; user and primary-group lookup is not available while generating config: {raw}"
    )
}

fn parse_id(value: &str, kind: &str, original: &str) -> Result<u32> {
    value
        .parse::<u32>()
        .with_context(|| format!("OCI Image Config User has a non-numeric {kind} in {original}"))
}

fn resolver_source() -> Option<PathBuf> {
    [
        Path::new("/run/systemd/resolve/resolv.conf"),
        Path::new("/etc/resolv.conf"),
    ]
    .into_iter()
    .find(|path| has_external_nameserver(path))
    .map(Path::to_path_buf)
}

fn has_external_nameserver(path: &Path) -> bool {
    let Ok(contents) = fs::read_to_string(path) else {
        return false;
    };
    contents.lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields.next() == Some("nameserver")
            && fields
                .next()
                .and_then(|address| address.parse::<IpAddr>().ok())
                .is_some_and(|address| !address.is_loopback())
    })
}

#[cfg(test)]
mod tests {
    use oci_spec::image::ImageConfiguration;
    use run_protocol::RuntimeConfig;
    use serde_json::{Value, json};

    use super::generate;

    #[test]
    fn image_defaults_are_merged_into_one_engine_compatible_config() {
        let image: ImageConfiguration = serde_json::from_value(json!({
            "architecture": "amd64",
            "os": "linux",
            "rootfs": {"type": "layers", "diff_ids": []},
            "config": {
                "User": "1000:1001",
                "Env": ["PATH=/agent/bin", "MODEL=example"],
                "Entrypoint": ["/agent/pi"],
                "Cmd": ["--mode", "run"],
                "WorkingDir": "/workspace"
            }
        }))
        .expect("Image Config");

        let bytes = generate(&image).expect("generated config");
        let runtime = RuntimeConfig::parse(bytes.clone()).expect("Run Protocol config");
        let value = runtime.as_json();
        assert_eq!(
            value.pointer("/process/args"),
            Some(&json!(["/agent/pi", "--mode", "run"]))
        );
        assert_eq!(
            value.pointer("/process/env"),
            Some(&json!(["PATH=/agent/bin", "MODEL=example"]))
        );
        assert_eq!(
            value.pointer("/process/cwd"),
            Some(&Value::String("/workspace".to_owned()))
        );
        assert_eq!(
            value.pointer("/process/user"),
            Some(&json!({"uid": 1000, "gid": 1001}))
        );
        assert_eq!(
            value.pointer("/linux/namespaces/1"),
            Some(&json!({"type": "network"}))
        );
        assert!(value.get("network").is_none());
        assert_eq!(bytes.last(), Some(&b'\n'));
    }

    #[test]
    fn fixed_defaults_produce_a_complete_config() {
        let image: ImageConfiguration = serde_json::from_value(json!({
            "architecture": "amd64",
            "os": "linux",
            "rootfs": {"type": "layers", "diff_ids": []},
            "config": {}
        }))
        .expect("Image Config");

        let bytes = generate(&image).expect("generated config");
        let runtime = RuntimeConfig::parse(bytes).expect("Run Protocol config");
        assert_eq!(
            runtime.as_json().pointer("/process/args"),
            Some(&json!(["sh"]))
        );
        assert_eq!(runtime.as_json().pointer("/process/cwd"), Some(&json!("/")));
        assert_eq!(
            runtime.as_json().pointer("/process/user"),
            Some(&json!({"uid": 0, "gid": 0}))
        );
    }
}
