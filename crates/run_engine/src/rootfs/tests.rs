use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Cursor, Read as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use anyhow::{Context, Result};
use oci_spec::image::{Descriptor, Digest, MediaType};
use rustix::fs::{Mode, OFlags, XattrFlags, fsetxattr, open};
use rustix::process::geteuid;
use tar::{Archive, Builder, EntryType, HeaderMode};

use super::super::{CapturedLayer, VerifiedLayer};
use super::capture::read_fd_xattrs;
use super::digest::sha256_digest;
use super::encode::{append_pax_metadata, tar_header};
use super::layer::scan_layer;
use super::mountinfo::{ensure_mountinfo_clear, mount_below};
use super::plan::LayerPlan;
use super::preflight::MaterializationBudget;
use super::{
    FsPath, Metadata, Rootfs, RootfsError, RootfsErrorKind, RootfsLimits, Timestamp, usize_to_u64,
};

struct TestLayer {
    descriptor: Descriptor,
    bytes: Vec<u8>,
    diff_id: Digest,
}

#[test]
fn rejects_path_traversal_and_normalizes_safe_components() {
    assert!(FsPath::from_relative(b"../../escape", 1024).is_err());
    assert!(FsPath::from_relative(b"/absolute", 1024).is_err());
    assert_eq!(
        FsPath::from_relative(b"safe/./path", 1024)
            .expect("path")
            .as_bytes(),
        b"safe/path"
    );
}

#[test]
fn ordered_layers_apply_whiteout_and_type_replacement() {
    if !geteuid().is_root() {
        return;
    }
    let lower = tar_layer(|builder| {
        append_test_directory(builder, b"dir")?;
        append_test_file(builder, b"dir/old", b"old")?;
        append_test_file(builder, b"value", b"lower")
    });
    let upper = tar_layer(|builder| {
        append_test_file(builder, b"dir/.wh..wh..opq", b"")?;
        append_test_file(builder, b"dir/new", b"new")?;
        append_test_file(builder, b".wh.value", b"")?;
        append_test_file(builder, b"value", b"upper")
    });
    let workspace = tempfile::tempdir().expect("workspace");
    let rootfs = materialize(workspace.path(), &[&lower, &upper]).expect("materialize");
    assert!(!rootfs.path().join("dir/old").exists());
    assert_eq!(
        std::fs::read(rootfs.path().join("dir/new")).unwrap(),
        b"new"
    );
    assert_eq!(
        std::fs::read(rootfs.path().join("value")).unwrap(),
        b"upper"
    );
}

#[test]
fn forward_hardlink_is_materialized_as_one_inode() {
    if !geteuid().is_root() {
        return;
    }
    let layer = tar_layer(|builder| {
        append_test_hardlink(builder, b"alias", b"target")?;
        append_test_file(builder, b"target", b"shared")
    });
    let workspace = tempfile::tempdir().expect("workspace");
    let rootfs = materialize(workspace.path(), &[&layer]).expect("materialize");
    let target = std::fs::metadata(rootfs.path().join("target")).unwrap();
    let alias = std::fs::metadata(rootfs.path().join("alias")).unwrap();
    assert_eq!(target.ino(), alias.ino());
    assert_eq!(target.nlink(), 2);
}

#[test]
fn mount_artifact_cleanup_is_dirfd_relative_and_fails_closed() {
    if !geteuid().is_root() {
        return;
    }
    let workspace = tempfile::tempdir().expect("workspace");
    let rootfs = materialize(workspace.path(), &[]).expect("materialize");
    std::fs::create_dir_all(rootfs.path().join("removed/nested")).expect("artifact directories");
    rootfs
        .remove_mount_artifact(Path::new("removed/nested"))
        .expect("remove leaf artifact");
    rootfs
        .remove_mount_artifact(Path::new("removed"))
        .expect("remove parent artifact");
    assert!(!rootfs.path().join("removed").exists());

    let outside = tempfile::tempdir().expect("outside");
    std::fs::create_dir(outside.path().join("victim")).expect("outside victim");
    std::os::unix::fs::symlink(outside.path(), rootfs.path().join("escape"))
        .expect("escape symlink");
    let error = rootfs
        .remove_mount_artifact(Path::new("escape/victim"))
        .expect_err("symlink ancestor must fail closed");
    assert!(
        error.to_string().contains("rootfs instability"),
        "{error:#}"
    );
    assert!(
        outside.path().join("victim").is_dir(),
        "cleanup escaped the retained rootfs descriptor"
    );

    std::fs::create_dir(rootfs.path().join("nonempty")).expect("nonempty artifact");
    std::fs::write(rootfs.path().join("nonempty/change"), b"preserve").expect("rootfs change");
    let error = rootfs
        .remove_mount_artifact(Path::new("nonempty"))
        .expect_err("nonempty artifact must make the rootfs unstable");
    assert!(
        error.to_string().contains("rootfs instability"),
        "{error:#}"
    );
    assert_eq!(
        std::fs::read(rootfs.path().join("nonempty/change")).expect("preserved change"),
        b"preserve"
    );
}

#[test]
fn stopped_capture_is_deterministic_and_preserves_raw_hardlinks() {
    if !geteuid().is_root() {
        return;
    }
    let layer = tar_layer(|builder| append_test_file(builder, b"base", b"base"));
    let workspace = tempfile::tempdir().expect("workspace");
    let rootfs = materialize(workspace.path(), &[&layer]).expect("materialize");
    let raw = rootfs.path().join(OsStr::from_bytes(b"raw-\xff"));
    std::fs::write(&raw, b"changed").unwrap();
    std::fs::hard_link(&raw, rootfs.path().join("hard")).unwrap();
    std::fs::remove_file(rootfs.path().join("base")).unwrap();

    let first = rootfs.capture().expect("first capture");
    let second = rootfs.capture().expect("second capture");
    assert_eq!(first.diff_id, second.diff_id);
    assert_eq!(first.size, second.size);
    let mut first_bytes = Vec::new();
    first.open().unwrap().read_to_end(&mut first_bytes).unwrap();
    let mut second_bytes = Vec::new();
    second
        .open()
        .unwrap()
        .read_to_end(&mut second_bytes)
        .unwrap();
    assert_eq!(first_bytes, second_bytes);

    let mut archive = Archive::new(Cursor::new(first_bytes));
    let mut observed = Vec::new();
    for entry in archive.entries().unwrap() {
        let entry = entry.unwrap();
        observed.push((
            entry.path_bytes().into_owned(),
            entry.header().entry_type(),
            entry.link_name_bytes().map(std::borrow::Cow::into_owned),
        ));
    }
    assert!(observed.iter().any(|entry| entry.0 == b".wh.base"));
    assert!(observed.iter().any(|entry| entry.0 == b"raw-\xff"));
    assert!(observed.iter().any(|entry| entry.1 == EntryType::Link));
}

#[test]
fn captured_binary_xattr_survives_layer_materialization() {
    if !geteuid().is_root() {
        return;
    }
    let base = tar_layer(|_builder| Ok(()));
    let first_workspace = tempfile::tempdir().expect("first workspace");
    let first = materialize(first_workspace.path(), &[&base]).expect("first rootfs");
    let path = first.path().join("value");
    std::fs::write(&path, b"content").expect("write value");
    let file = File::open(&path).expect("open value");
    fsetxattr(
        &file,
        b"user.percent%name".as_slice(),
        b"binary\0value",
        XattrFlags::empty(),
    )
    .expect("set binary xattr");

    let captured = first.capture().expect("capture xattr");
    let mut bytes = Vec::new();
    captured
        .open()
        .expect("open capture")
        .read_to_end(&mut bytes)
        .expect("read capture");
    let captured_layer = TestLayer {
        descriptor: Descriptor::new(captured.media_type, captured.size, captured.diff_id.clone()),
        bytes,
        diff_id: captured.diff_id,
    };

    let second_workspace = tempfile::tempdir().expect("second workspace");
    let second = materialize(second_workspace.path(), &[&captured_layer])
        .expect("materialize captured xattr");
    let fd = open(
        second.path().join("value"),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("open materialized value");
    let xattrs =
        read_fd_xattrs(&fd, RootfsLimits::default(), None).expect("read materialized xattrs");

    assert_eq!(
        xattrs
            .get(b"user.percent%name".as_slice())
            .map(AsRef::as_ref),
        Some(b"binary\0value".as_slice())
    );
}

#[test]
fn capture_keeps_mount_path_and_outside_hardlink_in_one_group() {
    if !geteuid().is_root() {
        return;
    }
    let base = tar_layer(|builder| {
        append_test_directory(builder, b"mount")?;
        append_test_file(builder, b"mount/value", b"old")?;
        append_test_hardlink(builder, b"outside", b"mount/value")
    });
    let workspace = tempfile::tempdir().expect("workspace");
    let rootfs = materialize(workspace.path(), &[&base]).expect("materialize");
    let mut outside = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(rootfs.path().join("outside"))
        .expect("open outside hardlink");
    std::io::Write::write_all(&mut outside, b"new").expect("write hardlink group");
    outside.sync_all().expect("sync hardlink group");

    let captured = rootfs.capture().expect("capture complete rootfs");
    let entries = captured_entries(&captured);
    let mount = entries
        .iter()
        .find(|entry| entry.0 == b"mount/value")
        .expect("mount path entry");
    let outside = entries
        .iter()
        .find(|entry| entry.0 == b"outside")
        .expect("outside path entry");
    assert!(
        [mount, outside]
            .iter()
            .any(|entry| entry.1 == EntryType::Regular && entry.3 == b"new")
    );
    assert!(
        [mount, outside]
            .iter()
            .any(|entry| entry.1 == EntryType::Link)
    );
}

#[test]
fn opaque_replaces_lower_non_directory_before_children_are_applied() {
    if !geteuid().is_root() {
        return;
    }
    for lower_is_symlink in [false, true] {
        let lower = tar_layer(|builder| {
            if lower_is_symlink {
                append_test_symlink(builder, b"a", b"target")
            } else {
                append_test_file(builder, b"a", b"file")
            }
        });
        let upper = tar_layer(|builder| {
            append_test_file(builder, b"a/.wh..wh..opq", b"")?;
            append_test_directory(builder, b"a")?;
            append_test_file(builder, b"a/child", b"child")
        });
        let workspace = tempfile::tempdir().expect("workspace");
        let rootfs =
            materialize(workspace.path(), &[&lower, &upper]).expect("opaque type replacement");
        assert_eq!(
            std::fs::read(rootfs.path().join("a/child")).expect("child"),
            b"child"
        );
    }
}

#[test]
fn normalized_duplicate_tar_paths_are_rejected() {
    let layer = tar_layer(|builder| {
        append_test_file(builder, b"a/b", b"first")?;
        append_test_file(builder, b"a//b", b"second")
    });
    let error = scan_test_layer(&layer, RootfsLimits::default()).expect_err("normalized duplicate");
    assert!(error.to_string().contains("duplicate OCI Layer path"));
    if geteuid().is_root() {
        let workspace = tempfile::tempdir().expect("workspace");
        let error = materialize(workspace.path(), &[&layer]).expect_err("normalized duplicate");
        assert_materialization_kind(&error, RootfsErrorKind::InvalidInput);
    }
}

#[test]
fn traversal_and_symlink_escape_fail_without_writing_outside() {
    if !geteuid().is_root() {
        return;
    }
    let mut traversal = tar_layer(|builder| append_test_file(builder, b"safe-name", b"x"));
    replace_first_tar_path(&mut traversal.bytes, b"../escape");
    refresh_test_layer(&mut traversal);
    let workspace = tempfile::tempdir().expect("workspace");
    let error = materialize(workspace.path(), &[&traversal]).expect_err("path traversal");
    assert_materialization_kind(&error, RootfsErrorKind::InvalidInput);

    let outside = tempfile::tempdir().expect("outside");
    let target = outside.path().as_os_str().as_bytes().to_vec();
    let lower = tar_layer(|builder| append_test_symlink(builder, b"link", &target));
    let upper = tar_layer(|builder| append_test_file(builder, b"link/escaped", b"bad"));
    let workspace = tempfile::tempdir().expect("workspace");
    assert!(materialize(workspace.path(), &[&lower, &upper]).is_err());
    assert!(!outside.path().join("escaped").exists());
}

#[test]
fn hardlink_cycle_and_explicit_budgets_fail_closed() {
    if !geteuid().is_root() {
        return;
    }
    let cycle = tar_layer(|builder| {
        append_test_hardlink(builder, b"first", b"second")?;
        append_test_hardlink(builder, b"second", b"first")
    });
    let workspace = tempfile::tempdir().expect("workspace");
    let error = materialize(workspace.path(), &[&cycle]).expect_err("hardlink cycle");
    assert_materialization_kind(&error, RootfsErrorKind::InvalidInput);

    let paths = tar_layer(|builder| {
        append_test_file(builder, b"aa", b"")?;
        append_test_file(builder, b"bb", b"")
    });
    let limits = RootfsLimits {
        total_path_bytes: 3,
        ..RootfsLimits::default()
    };
    let error = scan_test_layer(&paths, limits).expect_err("path byte budget");
    assert!(
        error
            .to_string()
            .contains("Layer raw path bytes limit exceeded")
    );

    let metadata = Metadata {
        mode: 0o644,
        uid: 0,
        gid: 0,
        mtime: Timestamp {
            seconds: 0,
            nanos: 0,
        },
        xattrs: BTreeMap::from([(
            b"user.budget".to_vec().into_boxed_slice(),
            b"value".to_vec().into_boxed_slice(),
        )]),
    };
    let xattrs = tar_layer(|builder| {
        append_pax_metadata(builder, &metadata)?;
        append_test_file(builder, b"value", b"")
    });
    let limits = RootfsLimits {
        total_xattr_bytes: 4,
        ..RootfsLimits::default()
    };
    let error = scan_test_layer(&xattrs, limits).expect_err("xattr byte budget");
    assert!(
        error
            .to_string()
            .contains("Layer xattr bytes limit exceeded")
    );

    let lower = tar_layer(|builder| {
        append_test_directory(builder, b"tree")?;
        append_test_file(builder, b"tree/a", b"")?;
        append_test_file(builder, b"tree/b", b"")
    });
    let upper = tar_layer(|builder| append_test_file(builder, b".wh.tree", b""));
    let limits = RootfsLimits {
        cleanup_entries: 1,
        ..RootfsLimits::default()
    };
    let workspace = tempfile::tempdir().expect("workspace");
    let error = materialize_with_limits(workspace.path(), &[&lower, &upper], limits)
        .expect_err("cleanup entry budget");
    assert_materialization_kind(&error, RootfsErrorKind::UnsupportedInput);
}

#[test]
fn pax_xattr_suffix_is_literal_and_raw_unrepresentable_name_fails() {
    let percent = b"user.percent%name".to_vec().into_boxed_slice();
    let metadata = Metadata {
        mode: 0o644,
        uid: 0,
        gid: 0,
        mtime: Timestamp {
            seconds: 0,
            nanos: 0,
        },
        xattrs: BTreeMap::from([(percent.clone(), b"value".to_vec().into_boxed_slice())]),
    };
    let mut builder = Builder::new(Vec::new());
    append_pax_metadata(&mut builder, &metadata).expect("literal percent xattr");
    builder.finish().expect("finish PAX archive");
    let bytes = builder.into_inner().expect("PAX bytes");
    assert!(
        bytes
            .windows(b"SCHILY.xattr.user.percent%name".len())
            .any(|window| { window == b"SCHILY.xattr.user.percent%name" })
    );

    let raw = Metadata {
        xattrs: BTreeMap::from([(
            b"user.raw-\xff".to_vec().into_boxed_slice(),
            Box::<[u8]>::default(),
        )]),
        ..metadata
    };
    let mut builder = Builder::new(Vec::new());
    let error = append_pax_metadata(&mut builder, &raw)
        .expect_err("raw PAX xattr name is not representable");
    assert!(
        error
            .to_string()
            .contains("cannot be represented literally")
    );
}

#[test]
fn mountinfo_descendant_is_a_positive_fail_closed_signal() {
    let mountinfo = b"36 29 0:32 / / rw,relatime - ext4 /dev/root rw\n\
                40 36 0:45 / /state/rootfs/runtime\\040mount rw - tmpfs tmpfs rw\n";
    assert_eq!(
        mount_below(b"/state/rootfs", mountinfo).expect("mountinfo"),
        Some(b"/state/rootfs/runtime mount".to_vec())
    );
    assert!(ensure_mountinfo_clear(b"/state/rootfs", mountinfo).is_err());
    assert_eq!(
        mount_below(b"/other/rootfs", mountinfo).expect("unrelated mountinfo"),
        None
    );
}

#[test]
fn materialization_budgets_are_shared_across_layers() {
    if !geteuid().is_root() {
        return;
    }
    let first = tar_layer(|builder| append_test_file(builder, b"first", b"1"));
    let second = tar_layer(|builder| append_test_file(builder, b"second", b"2"));

    let workspace = tempfile::tempdir().expect("workspace");
    let limits = RootfsLimits {
        total_uncompressed_bytes: usize_to_u64(first.bytes.len() + second.bytes.len() - 1),
        ..RootfsLimits::default()
    };
    let error = materialize_with_limits(workspace.path(), &[&first, &second], limits)
        .expect_err("second Layer must exceed the remaining uncompressed byte");
    assert!(
        error
            .to_string()
            .contains("uncompressed Layer limit exceeded")
    );

    let workspace = tempfile::tempdir().expect("workspace");
    let limits = RootfsLimits {
        entries: 1,
        ..RootfsLimits::default()
    };
    let error = materialize_with_limits(workspace.path(), &[&first, &second], limits)
        .expect_err("second Layer entry must exceed the shared entry budget");
    assert!(error.to_string().contains("Layer entries limit exceeded"));

    let workspace = tempfile::tempdir().expect("workspace");
    let limits = RootfsLimits {
        total_path_bytes: usize_to_u64(b"first".len()),
        ..RootfsLimits::default()
    };
    let error = materialize_with_limits(workspace.path(), &[&first, &second], limits)
        .expect_err("second Layer path must exceed the shared raw path budget");
    assert!(
        error
            .to_string()
            .contains("Layer raw path bytes limit exceeded")
    );

    let xattr_metadata = |name: &[u8]| Metadata {
        mode: 0o644,
        uid: 0,
        gid: 0,
        mtime: Timestamp {
            seconds: 0,
            nanos: 0,
        },
        xattrs: BTreeMap::from([(
            name.to_vec().into_boxed_slice(),
            b"v".to_vec().into_boxed_slice(),
        )]),
    };
    let first_xattr = tar_layer(|builder| {
        append_pax_metadata(builder, &xattr_metadata(b"user.one"))?;
        append_test_file(builder, b"x-one", b"")
    });
    let second_xattr = tar_layer(|builder| {
        append_pax_metadata(builder, &xattr_metadata(b"user.two"))?;
        append_test_file(builder, b"x-two", b"")
    });
    // SCHILY carries one raw byte and LIBARCHIVE carries four base64
    // bytes, so the first Layer consumes 2*8 name bytes plus 5 values.
    let workspace = tempfile::tempdir().expect("workspace");
    let limits = RootfsLimits {
        total_xattr_bytes: 21,
        ..RootfsLimits::default()
    };
    let error = materialize_with_limits(workspace.path(), &[&first_xattr, &second_xattr], limits)
        .expect_err("second Layer xattr must exceed the shared xattr budget");
    assert!(
        error
            .to_string()
            .contains("Layer xattr bytes limit exceeded")
    );
}

#[test]
fn raw_dot_components_cannot_bypass_path_budget() {
    if !geteuid().is_root() {
        return;
    }
    let mut layer = tar_layer(|builder| append_test_file(builder, b"value", b"x"));
    let raw = format!("{}value", "./".repeat(30));
    replace_first_tar_path(&mut layer.bytes, raw.as_bytes());
    refresh_test_layer(&mut layer);
    let limits = RootfsLimits {
        total_path_bytes: usize_to_u64(b"value".len()),
        ..RootfsLimits::default()
    };
    let workspace = tempfile::tempdir().expect("workspace");
    let error = materialize_with_limits(workspace.path(), &[&layer], limits)
        .expect_err("raw path bytes must be counted before normalization");
    assert!(
        error
            .to_string()
            .contains("Layer raw path bytes limit exceeded")
    );
}

#[test]
fn oversized_pax_and_gnu_longname_are_rejected_by_preflight() {
    if !geteuid().is_root() {
        return;
    }
    for entry_type in [EntryType::XHeader, EntryType::GNULongName] {
        let layer = tar_layer(|builder| {
            let payload = vec![b'x'; 33];
            let mut header = tar_header(usize_to_u64(payload.len()), 0o644, 0, 0, 0, entry_type)?;
            header.set_path("extension")?;
            header.set_cksum();
            builder.append(&header, payload.as_slice())?;
            append_test_file(builder, b"value", b"")
        });
        let workspace = tempfile::tempdir().expect("workspace");
        let limits = RootfsLimits {
            extension_bytes: 32,
            ..RootfsLimits::default()
        };
        let error = materialize_with_limits(workspace.path(), &[&layer], limits)
            .expect_err("advanced tar parser must not receive oversized extension");
        assert!(
            error
                .to_string()
                .contains("tar extension bytes limit exceeded")
        );
    }
}

#[test]
fn pax_size_polyglot_is_rejected_before_hidden_extension() {
    if !geteuid().is_root() {
        return;
    }
    let pax = pax_record(b"size", b"0");
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&raw_tar_header(
        usize_to_u64(pax.len()),
        EntryType::XHeader,
        b"PaxHeaders/size",
    ));
    bytes.extend_from_slice(&pax);
    bytes.resize(bytes.len().next_multiple_of(512), 0);

    // The raw carrier claims that the following GNU longname header is
    // data. tar::Archive would instead apply PAX size=0, expose that
    // hidden header, and allocate its following 512-byte payload.
    bytes.extend_from_slice(&raw_tar_header(512, EntryType::Regular, b"carrier"));
    bytes.extend_from_slice(&raw_tar_header(512, EntryType::GNULongName, b"hidden"));
    bytes.extend_from_slice(&raw_tar_header(0, EntryType::Regular, b"decoy"));
    bytes.extend_from_slice(&raw_tar_header(0, EntryType::Regular, b"visible"));
    bytes.extend_from_slice(&[0_u8; 1024]);
    let layer = test_layer_from_bytes(bytes);
    let workspace = tempfile::tempdir().expect("workspace");
    let error = materialize_with_limits(
        workspace.path(),
        &[&layer],
        RootfsLimits {
            extension_bytes: usize_to_u64(pax.len()),
            ..RootfsLimits::default()
        },
    )
    .expect_err("PAX size must fail before the hidden GNU extension is parsed");
    assert!(
        error
            .to_string()
            .contains("PAX size overrides are unsupported")
    );
}

#[test]
fn gnu_sparse_headers_and_pax_keys_are_rejected_by_preflight() {
    if !geteuid().is_root() {
        return;
    }
    let sparse_type = tar_layer(|builder| {
        let mut header = tar_header(0, 0o644, 0, 0, 0, EntryType::GNUSparse)?;
        header.set_path("sparse")?;
        header.set_cksum();
        builder.append(&header, std::io::empty())?;
        Ok(())
    });
    let workspace = tempfile::tempdir().expect("workspace");
    let error = materialize(workspace.path(), &[&sparse_type])
        .expect_err("GNU sparse type must fail in preflight");
    assert_materialization_kind(&error, RootfsErrorKind::UnsupportedInput);
    assert!(
        error
            .to_string()
            .contains("GNU sparse OCI Layer entries are unsupported")
    );

    let pax = pax_record(b"GNU.sparse.map", b"0,1");
    let sparse_pax = tar_layer(|builder| {
        let mut header = tar_header(usize_to_u64(pax.len()), 0o644, 0, 0, 0, EntryType::XHeader)?;
        header.set_path("PaxHeaders/sparse")?;
        header.set_cksum();
        builder.append(&header, pax.as_slice())?;
        append_test_file(builder, b"value", b"")
    });
    let workspace = tempfile::tempdir().expect("workspace");
    let error = materialize(workspace.path(), &[&sparse_pax])
        .expect_err("GNU sparse PAX key must fail in preflight");
    assert_materialization_kind(&error, RootfsErrorKind::UnsupportedInput);
    assert!(
        error
            .to_string()
            .contains("GNU sparse OCI Layer PAX metadata is unsupported")
    );
}

#[test]
fn corrupt_compressed_layer_is_invalid_not_internal() {
    if !geteuid().is_root() {
        return;
    }
    let bytes = b"not a gzip stream".to_vec();
    let layer = TestLayer {
        descriptor: Descriptor::new(
            MediaType::ImageLayerGzip,
            usize_to_u64(bytes.len()),
            sha256_digest(&bytes),
        ),
        bytes,
        diff_id: sha256_digest(b""),
    };
    let workspace = tempfile::tempdir().expect("workspace");
    let error = materialize(workspace.path(), &[&layer]).expect_err("corrupt gzip Layer");
    assert_materialization_kind(&error, RootfsErrorKind::InvalidInput);
}

#[test]
fn nonzero_tail_after_tar_end_marker_is_rejected() {
    if !geteuid().is_root() {
        return;
    }
    let mut layer = tar_layer(|builder| append_test_file(builder, b"value", b""));
    layer.bytes.extend_from_slice(&[0_u8; 512]);
    layer.bytes.push(1);
    refresh_test_layer(&mut layer);
    let workspace = tempfile::tempdir().expect("workspace");
    let error = materialize(workspace.path(), &[&layer])
        .expect_err("non-zero unvisited tar tail must fail");
    assert!(
        error
            .to_string()
            .contains("non-zero data after its end marker")
    );
}

fn materialize(workspace: &Path, layers: &[&TestLayer]) -> Result<Rootfs> {
    materialize_with_limits(workspace, layers, RootfsLimits::default())
}

fn assert_materialization_kind(error: &anyhow::Error, expected: RootfsErrorKind) {
    let classified = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<RootfsError>())
        .expect("structured RootfsError in causal chain");
    assert_eq!(classified.kind(), expected, "{error:#}");
}

fn materialize_with_limits(
    workspace: &Path,
    layers: &[&TestLayer],
    limits: RootfsLimits,
) -> Result<Rootfs> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(workspace, std::fs::Permissions::from_mode(0o700))?;
    let verified = layers
        .iter()
        .map(|layer| VerifiedLayer {
            descriptor: &layer.descriptor,
            expected_diff_id: &layer.diff_id,
        })
        .collect::<Vec<_>>();
    Ok(Rootfs::materialize_in(
        workspace,
        &verified,
        limits,
        |descriptor| {
            let layer = layers
                .iter()
                .find(|layer| &layer.descriptor == descriptor)
                .context("test Layer is absent")?;
            Ok(Cursor::new(layer.bytes.clone()))
        },
    )?)
}

fn scan_test_layer(layer: &TestLayer, limits: RootfsLimits) -> Result<LayerPlan> {
    use std::io::{Seek as _, Write as _};
    let workspace = tempfile::tempdir()?;
    let mut file = tempfile::NamedTempFile::new_in(workspace.path())?;
    file.write_all(&layer.bytes)?;
    file.rewind()?;
    let mut budget = MaterializationBudget::new(limits);
    scan_layer(file.as_file_mut(), workspace.path(), limits, &mut budget)
}

type CapturedEntry = (Vec<u8>, EntryType, Option<Vec<u8>>, Vec<u8>);

fn captured_entries(layer: &CapturedLayer) -> Vec<CapturedEntry> {
    let mut archive = Archive::new(layer.open().expect("open captured Layer"));
    archive
        .entries()
        .expect("captured entries")
        .map(|entry| {
            let mut entry = entry.expect("captured entry");
            let mut content = Vec::new();
            entry.read_to_end(&mut content).expect("captured content");
            (
                entry.path_bytes().into_owned(),
                entry.header().entry_type(),
                entry.link_name_bytes().map(std::borrow::Cow::into_owned),
                content,
            )
        })
        .collect()
}

fn tar_layer(mut write: impl FnMut(&mut Builder<Vec<u8>>) -> Result<()>) -> TestLayer {
    let mut builder = Builder::new(Vec::new());
    builder.mode(HeaderMode::Deterministic);
    write(&mut builder).expect("Layer entries");
    builder.finish().expect("finish Layer");
    let bytes = builder.into_inner().expect("Layer bytes");
    test_layer_from_bytes(bytes)
}

fn test_layer_from_bytes(bytes: Vec<u8>) -> TestLayer {
    let diff_id = sha256_digest(&bytes);
    let descriptor = Descriptor::new(
        MediaType::ImageLayer,
        usize_to_u64(bytes.len()),
        diff_id.clone(),
    );
    TestLayer {
        descriptor,
        bytes,
        diff_id,
    }
}

fn pax_record(key: &[u8], value: &[u8]) -> Vec<u8> {
    let remainder = key.len() + value.len() + 3;
    let mut digits = 1;
    loop {
        let length = remainder + digits;
        if length.to_string().len() == digits {
            let mut record = length.to_string().into_bytes();
            record.push(b' ');
            record.extend_from_slice(key);
            record.push(b'=');
            record.extend_from_slice(value);
            record.push(b'\n');
            return record;
        }
        digits = length.to_string().len();
    }
}

fn raw_tar_header(size: u64, entry_type: EntryType, path: &[u8]) -> [u8; 512] {
    let mut header = tar_header(size, 0o644, 0, 0, 0, entry_type).expect("raw header");
    header
        .set_path(Path::new(OsStr::from_bytes(path)))
        .expect("raw header path");
    header.set_cksum();
    *header.as_bytes()
}

fn refresh_test_layer(layer: &mut TestLayer) {
    layer.diff_id = sha256_digest(&layer.bytes);
    layer.descriptor = Descriptor::new(
        MediaType::ImageLayer,
        usize_to_u64(layer.bytes.len()),
        layer.diff_id.clone(),
    );
}

fn replace_first_tar_path(bytes: &mut [u8], path: &[u8]) {
    assert!(path.len() <= 100);
    bytes[..100].fill(0);
    bytes[..path.len()].copy_from_slice(path);
    bytes[148..156].fill(b' ');
    let checksum = bytes[..512]
        .iter()
        .map(|byte| u64::from(*byte))
        .sum::<u64>();
    let encoded = format!("{checksum:06o}\0 ");
    bytes[148..156].copy_from_slice(encoded.as_bytes());
}

fn append_test_file(builder: &mut Builder<Vec<u8>>, path: &[u8], bytes: &[u8]) -> Result<()> {
    let mut header = tar_header(
        usize_to_u64(bytes.len()),
        0o644,
        0,
        0,
        0,
        EntryType::Regular,
    )?;
    builder.append_data(&mut header, Path::new(OsStr::from_bytes(path)), bytes)?;
    Ok(())
}

fn append_test_directory(builder: &mut Builder<Vec<u8>>, path: &[u8]) -> Result<()> {
    let mut header = tar_header(0, 0o755, 0, 0, 0, EntryType::Directory)?;
    builder.append_data(
        &mut header,
        Path::new(OsStr::from_bytes(path)),
        std::io::empty(),
    )?;
    Ok(())
}

fn append_test_hardlink(builder: &mut Builder<Vec<u8>>, path: &[u8], target: &[u8]) -> Result<()> {
    let mut header = tar_header(0, 0o644, 0, 0, 0, EntryType::Link)?;
    builder.append_link(
        &mut header,
        Path::new(OsStr::from_bytes(path)),
        Path::new(OsStr::from_bytes(target)),
    )?;
    Ok(())
}

fn append_test_symlink(builder: &mut Builder<Vec<u8>>, path: &[u8], target: &[u8]) -> Result<()> {
    let mut header = tar_header(0, 0o777, 0, 0, 0, EntryType::Symlink)?;
    builder.append_link(
        &mut header,
        Path::new(OsStr::from_bytes(path)),
        Path::new(OsStr::from_bytes(target)),
    )?;
    Ok(())
}
