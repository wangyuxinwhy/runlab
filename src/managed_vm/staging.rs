use super::*;

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};

use rustix::fs::{Mode, OFlags, open};

use crate::runtime::RuntimeConfig;
use crate::topology::rewrite_runtime_config_reference;

const MAX_RUNTIME_CONFIG_INPUTS: usize = 2;
const MAX_RUNTIME_CONFIG_BYTES: u64 = 16 * 1024 * 1024;
const MAX_MANAGED_SERVICE_BYTES: u64 = 1024 * 1024;
const MAX_MOUNT_SOURCE_BYTES: u64 = 64 * 1024;
const MAX_NATIVE_FILE_MOUNTS: usize = 8;

pub(super) fn validate_runtime_config_inputs(
    inputs: &[usize],
    input_count: usize,
) -> Result<BTreeSet<usize>> {
    ensure!(
        inputs.len() <= MAX_RUNTIME_CONFIG_INPUTS,
        "VM execution supports at most {MAX_RUNTIME_CONFIG_INPUTS} OCI Runtime Config inputs"
    );
    let unique = inputs.iter().copied().collect::<BTreeSet<_>>();
    ensure!(
        unique.len() == inputs.len(),
        "OCI Runtime Config input slots must be distinct"
    );
    ensure!(
        unique.iter().all(|index| *index < input_count),
        "OCI Runtime Config input slot is out of range"
    );
    Ok(unique)
}

pub(super) fn seal_runtime_config_inputs(operation_id: Uuid) -> Result<()> {
    ensure_guest_linux()?;
    ensure!(
        rustix::process::geteuid().is_root(),
        "VM input sealing requires root"
    );
    let operation = load_guest_operation(operation_id)?;
    ensure!(
        operation.input_identities.len() == operation.input_count,
        "guest operation input identity count mismatch"
    );
    let config_slots =
        validate_runtime_config_inputs(&operation.runtime_config_inputs, operation.input_count)?;
    if config_slots.is_empty() {
        return Ok(());
    }

    let declaration_slot = managed_service_declaration_slot(&operation.argv)?;
    let mut source_inputs = BTreeMap::<usize, Vec<u8>>::new();
    let mut derived = BTreeMap::<String, Vec<u8>>::new();
    let mut mount_count = 0_usize;

    for index in &config_slots {
        let bytes = read_verified_input(
            operation_id,
            *index,
            &operation.input_identities[*index],
            MAX_RUNTIME_CONFIG_BYTES,
        )?;
        let runtime = RuntimeConfig::load_rewriting_mount_sources(&bytes, |source| {
            let Some(source_index) = parse_slot(source, "@input/")? else {
                return Ok(None);
            };
            ensure!(
                source_index < operation.input_count,
                "OCI mount source input slot {source_index} was not declared"
            );
            ensure!(
                !config_slots.contains(&source_index),
                "OCI Runtime Config input cannot also be a mount source input"
            );
            if let std::collections::btree_map::Entry::Vacant(entry) =
                source_inputs.entry(source_index)
            {
                entry.insert(read_verified_input(
                    operation_id,
                    source_index,
                    &operation.input_identities[source_index],
                    MAX_MOUNT_SOURCE_BYTES,
                )?);
            }
            Ok(Some(sealed_source_path(operation_id, source_index)))
        })?;
        mount_count = mount_count
            .checked_add(runtime.native_file_mount_count()?)
            .context("native file mount count overflow")?;
        derived.insert(format!("runtime-config-{index}.json"), runtime.encoded()?);
    }
    ensure!(
        mount_count <= MAX_NATIVE_FILE_MOUNTS,
        "a native Run accepts at most {MAX_NATIVE_FILE_MOUNTS} read-only file mounts across all participants"
    );

    let mut referenced_configs = direct_runtime_config_slots(&operation.argv)?;
    if let Some(declaration_index) = declaration_slot {
        ensure!(
            !config_slots.contains(&declaration_index),
            "Managed Service declaration and OCI Runtime Config must use distinct input slots"
        );
        let bytes = read_verified_input(
            operation_id,
            declaration_index,
            &operation.input_identities[declaration_index],
            MAX_MANAGED_SERVICE_BYTES,
        )?;
        let declaration = rewrite_runtime_config_reference(&bytes, |reference| {
            let runtime_index = parse_slot(reference, "@input/")?
                .context("VM Managed Service runtime_config_file must be an @input/N token")?;
            ensure!(
                config_slots.contains(&runtime_index),
                "Managed Service Runtime Config input slot {runtime_index} was not marked with --runtime-config-input"
            );
            referenced_configs.insert(runtime_index);
            Ok(sealed_runtime_config_path(operation_id, runtime_index))
        })?;
        derived.insert(
            format!("managed-service-{declaration_index}.json"),
            declaration,
        );
    }
    ensure!(
        referenced_configs == config_slots,
        "each --runtime-config-input slot must be used by --runtime-config or a Managed Service declaration"
    );

    for (index, bytes) in source_inputs {
        derived.insert(format!("source-{index}"), bytes);
    }
    publish_sealed_directory(operation_id, &derived)
}

pub(super) fn derived_input_path(
    operation_id: Uuid,
    operation: &GuestOperation,
    index: usize,
) -> Result<String> {
    let configs =
        validate_runtime_config_inputs(&operation.runtime_config_inputs, operation.input_count)?;
    if configs.contains(&index) {
        return path_string(&sealed_runtime_config_path(operation_id, index));
    }
    if managed_service_declaration_slot(&operation.argv)? == Some(index) && !configs.is_empty() {
        return path_string(
            &sealed_operation_path(operation_id).join(format!("managed-service-{index}.json")),
        );
    }
    Ok(operation_file(operation_id, "input", index))
}

pub(super) fn sealed_operation_path(operation_id: Uuid) -> PathBuf {
    Path::new(GUEST_SEALED_INPUT_ROOT).join(operation_id.to_string())
}

fn sealed_source_path(operation_id: Uuid, index: usize) -> PathBuf {
    sealed_operation_path(operation_id).join(format!("source-{index}"))
}

fn sealed_runtime_config_path(operation_id: Uuid, index: usize) -> PathBuf {
    sealed_operation_path(operation_id).join(format!("runtime-config-{index}.json"))
}

fn managed_service_declaration_slot(argv: &[String]) -> Result<Option<usize>> {
    option_input_slot(argv, "--managed-service")
}

fn direct_runtime_config_slots(argv: &[String]) -> Result<BTreeSet<usize>> {
    Ok(option_input_slot(argv, "--runtime-config")?
        .into_iter()
        .collect())
}

fn option_input_slot(argv: &[String], option: &str) -> Result<Option<usize>> {
    let mut found = None;
    let mut arguments = argv.iter();
    while let Some(argument) = arguments.next() {
        let value = if argument == option {
            arguments.next().map(String::as_str)
        } else {
            argument
                .strip_prefix(option)
                .and_then(|value| value.strip_prefix('='))
        };
        let Some(value) = value else {
            continue;
        };
        let Some(index) = parse_slot(value, "@input/")? else {
            continue;
        };
        ensure!(
            found.replace(index).is_none(),
            "{option} is specified more than once"
        );
    }
    Ok(found)
}

fn read_verified_input(
    operation_id: Uuid,
    index: usize,
    expected: &FileIdentity,
    limit: u64,
) -> Result<Vec<u8>> {
    let path = operation_file(operation_id, "input", index);
    let file = File::from(
        open(
            &path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| format!("cannot open input slot {index}"))?,
    );
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot inspect input slot {index}"))?;
    ensure!(
        metadata.is_file(),
        "input slot {index} is not a regular file"
    );
    ensure!(
        metadata.len() <= limit,
        "input slot {index} exceeds {limit} bytes"
    );
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len())?);
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "input slot {index} exceeds {limit} bytes"
    );
    let observed = FileIdentity {
        schema_version: 1,
        digest: crate::integrity::digest_bytes(&bytes),
        size: bytes.len() as u64,
    };
    ensure!(
        &observed == expected,
        "input slot {index} changed after host verification"
    );
    Ok(bytes)
}

fn publish_sealed_directory(operation_id: Uuid, files: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    fs::create_dir_all(GUEST_SEALED_INPUT_ROOT)?;
    fs::set_permissions(GUEST_SEALED_INPUT_ROOT, fs::Permissions::from_mode(0o700))?;
    verify_private_root_directory(Path::new(GUEST_SEALED_INPUT_ROOT))?;
    let destination = sealed_operation_path(operation_id);
    if destination.exists() {
        return verify_published_files(&destination, files);
    }
    let temporary = Path::new(GUEST_SEALED_INPUT_ROOT).join(format!(".{operation_id}.tmp"));
    fs::create_dir(&temporary)?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o700))?;
    let result: Result<()> = (|| {
        for (name, bytes) in files {
            let path = temporary.join(name);
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        File::open(&temporary)?.sync_all()?;
        fs::rename(&temporary, &destination)?;
        File::open(GUEST_SEALED_INPUT_ROOT)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&temporary);
    }
    result?;
    verify_published_files(&destination, files)
}

fn verify_published_files(directory: &Path, expected: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    verify_private_root_directory(directory)?;
    let actual = fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<BTreeSet<_>>>()?;
    let names = expected
        .keys()
        .map(std::ffi::OsString::from)
        .collect::<BTreeSet<_>>();
    ensure!(
        actual == names,
        "sealed VM input set does not match the operation"
    );
    for (name, bytes) in expected {
        let metadata = fs::symlink_metadata(directory.join(name))?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == 0
                && metadata.gid() == 0
                && metadata.mode() & 0o777 == 0o600,
            "sealed VM input {name} is not a root-owned 0600 regular file"
        );
        ensure!(
            fs::read(directory.join(name))? == *bytes,
            "sealed VM input {name} does not match the operation"
        );
    }
    Ok(())
}

fn verify_private_root_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == 0
            && metadata.gid() == 0
            && metadata.mode() & 0o777 == 0o700,
        "sealed VM input directory is not root-owned mode 0700: {}",
        path.display()
    );
    ensure!(
        path.canonicalize()? == path,
        "sealed VM input directory is not canonical: {}",
        path.display()
    );
    Ok(())
}

fn path_string(path: &Path) -> Result<String> {
    Ok(path
        .to_str()
        .context("sealed VM input path is not valid Unicode")?
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_config_slots_are_bounded_distinct_and_declared() {
        assert_eq!(
            validate_runtime_config_inputs(&[0, 2], 3).unwrap(),
            BTreeSet::from([0, 2])
        );
        assert!(validate_runtime_config_inputs(&[0, 0], 1).is_err());
        assert!(validate_runtime_config_inputs(&[0, 1, 2], 3).is_err());
        assert!(validate_runtime_config_inputs(&[1], 1).is_err());
    }

    #[test]
    fn only_typed_option_values_identify_runtime_and_service_inputs() {
        let argv = vec![
            "run".to_owned(),
            "start".to_owned(),
            "image".to_owned(),
            "--runtime-config=@input/0".to_owned(),
            "--managed-service".to_owned(),
            "@input/2".to_owned(),
            "--stdin".to_owned(),
            "@input/3".to_owned(),
        ];
        assert_eq!(
            direct_runtime_config_slots(&argv).unwrap(),
            BTreeSet::from([0])
        );
        assert_eq!(managed_service_declaration_slot(&argv).unwrap(), Some(2));
    }
}
