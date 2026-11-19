//! Packaging contract for the MariaDB adapter and its system-account setup.
//!
//! The asset tables are parsed in their native syntaxes (generate-rpm inline
//! tables, deb arrays) and compared as exact multisets, so an empty or partial
//! parse fails. The real-package test builds both packages with the pinned
//! packagers and reads them back with the distribution tools, so it checks what
//! ships rather than what the manifest says. It needs `cargo-deb` 3.8.0 and
//! `cargo-generate-rpm` 0.21.0 on `PATH`, and `target/release/gruezi`.

use anyhow::{Result, bail, ensure};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

const MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");
const ADAPTER_SOURCE: &str = "contrib/mariadb/gruezi-mariadb-adapter";
const ADAPTER_DEST: &str = "/usr/libexec/gruezi/gruezi-mariadb-adapter";

/// (source, installed path, mode) for every asset both tables must ship.
const EXPECTED: &[(&str, &str, u32)] = &[
    ("target/release/gruezi", "/usr/bin/gruezi", 0o755),
    (
        "contrib/systemd/gruezi.service",
        "/etc/systemd/system/gruezi.service",
        0o644,
    ),
    (
        "contrib/systemd/gruezi.env.example",
        "/etc/gruezi/gruezi.env",
        0o640,
    ),
    (ADAPTER_SOURCE, ADAPTER_DEST, 0o755),
    (
        "contrib/mariadb/mariadb-adapter.conf.example",
        "/usr/share/doc/gruezi/examples/mariadb-adapter.conf.example",
        0o644,
    ),
    (
        "contrib/systemd/gruezi-sysusers.conf",
        "/usr/lib/sysusers.d/gruezi.conf",
        0o644,
    ),
    (
        "contrib/systemd/gruezi-tmpfiles.conf",
        "/usr/lib/tmpfiles.d/gruezi.conf",
        0o644,
    ),
];

/// Run a program in `dir` and return its stdout, failing on a non-zero exit.
fn run(program: &str, args: &[&str], dir: &Path) -> Result<String> {
    let output = Command::new(program).args(args).current_dir(dir).output()?;
    ensure!(
        output.status.success(),
        "{program} {args:?} exited {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

/// `(source, dest, mode)` for each asset line of one manifest table. Both
/// syntaxes quote their fields, so the odd pieces of a split on `"` are the
/// values; lines without exactly three values (such as scriptlet paths) are not
/// assets and are skipped.
fn asset_entries(manifest: &str, table: &str) -> Result<Vec<(String, String, u32)>> {
    let header = format!("[{table}]");
    let mut in_table = false;
    let mut entries = Vec::new();
    for line in manifest.lines() {
        if line.starts_with('[') {
            in_table = line.trim() == header;
            continue;
        }
        if !in_table {
            continue;
        }
        let values: Vec<&str> = line.split('"').skip(1).step_by(2).collect();
        if let [source, dest, mode] = values.as_slice() {
            entries.push((
                (*source).to_owned(),
                (*dest).to_owned(),
                u32::from_str_radix(mode, 8)?,
            ));
        }
    }
    Ok(entries)
}

fn expected_entries() -> Vec<(String, String, u32)> {
    let mut expected: Vec<_> = EXPECTED
        .iter()
        .map(|(source, dest, mode)| ((*source).to_owned(), (*dest).to_owned(), *mode))
        .collect();
    expected.sort();
    expected
}

/// `ls -l`-style permission string, the form both `dpkg-deb -c` and rpm report.
fn permission_string(mode: u32) -> String {
    let mut out = String::from("-");
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 0o7;
        out.push(if bits & 4 != 0 { 'r' } else { '-' });
        out.push(if bits & 2 != 0 { 'w' } else { '-' });
        out.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    out
}

/// The single file in `dir` whose name has the given prefix and suffix.
fn only_file(dir: &Path, prefix: &str, suffix: &str) -> Result<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if name.starts_with(prefix) && name.ends_with(suffix) {
            found.push(path);
        }
    }
    match found.as_slice() {
        [path] => Ok(path.clone()),
        _ => bail!(
            "expected one {prefix}*{suffix} in {}, found {found:?}",
            dir.display()
        ),
    }
}

#[test]
fn release_packages_asset_tables_ship_adapter_and_confs() -> Result<()> {
    let root = Path::new(MANIFEST_DIR);
    let manifest = fs::read_to_string(root.join("Cargo.toml"))?;
    let expected = expected_entries();

    for table in ["package.metadata.generate-rpm", "package.metadata.deb"] {
        let mut got = asset_entries(&manifest, table)?;
        got.sort();
        ensure!(got == expected, "[{table}] assets differ: {got:?}");
    }

    for (source, _, _) in EXPECTED {
        if !source.starts_with("target/") {
            ensure!(root.join(source).is_file(), "{source} is missing");
        }
    }
    let mode = fs::metadata(root.join(ADAPTER_SOURCE))?
        .permissions()
        .mode();
    ensure!(mode & 0o111 != 0, "{ADAPTER_SOURCE} is not executable");
    Ok(())
}

/// Fail unless the pinned packagers are the ones on `PATH`.
fn check_pinned_tools(root: &Path) -> Result<()> {
    ensure!(
        root.join("target/release/gruezi").is_file(),
        "run `cargo build --release --locked` first"
    );
    ensure!(
        run("cargo-deb", &["--version"], root)?.trim() == "cargo-deb 3.8.0",
        "cargo-deb must be the pinned 3.8.0"
    );
    ensure!(
        run("cargo-generate-rpm", &["--version"], root)?.trim() == "cargo-generate-rpm 0.21.0",
        "cargo-generate-rpm must be the pinned 0.21.0"
    );
    Ok(())
}

/// Build both packages from scratch and return their paths as `(deb, rpm)`.
/// The output directories start empty so the single-file lookups cannot pick
/// up a stale package from an earlier run.
fn build_packages(root: &Path) -> Result<(String, String)> {
    let debian = root.join("target/debian");
    let generated = root.join("target/generate-rpm");
    for dir in [&debian, &generated] {
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
    }
    run("cargo-deb", &["--no-build"], root)?;
    run("cargo-generate-rpm", &[], root)?;
    let deb = only_file(&debian, "gruezi_", ".deb")?;
    let rpm = only_file(&generated, "gruezi-", ".rpm")?;
    Ok((
        deb.to_string_lossy().into_owned(),
        rpm.to_string_lossy().into_owned(),
    ))
}

/// Each package's own listing must carry every expected asset with its mode.
fn check_listings(root: &Path, deb: &str, rpm: &str) -> Result<()> {
    let deb_text = run("dpkg-deb", &["-c", deb], root)?;
    println!("== dpkg-deb -c\n{deb_text}");
    let rpm_names = run("rpm", &["-qlp", rpm], root)?;
    println!("== rpm -qlp\n{rpm_names}");

    // `(permissions, path)` for every entry, from each package's own listing.
    let deb_listing: Vec<(String, String)> = deb_text
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (perms, path) = (fields.first()?, fields.last()?);
            Some(((*perms).to_owned(), path.trim_start_matches('.').to_owned()))
        })
        .collect();
    let rpm_listing: Vec<(String, String)> = run(
        "rpm",
        &[
            "-qp",
            "--queryformat",
            "[%{FILEMODES:perms} %{FILENAMES}\\n]",
            rpm,
        ],
        root,
    )?
    .lines()
    .filter_map(|line| {
        let (perms, path) = line.split_once(' ')?;
        Some((perms.to_owned(), path.to_owned()))
    })
    .collect();

    for (label, listing) in [("deb", &deb_listing), ("rpm", &rpm_listing)] {
        for (_, dest, mode) in expected_entries() {
            let perms = permission_string(mode);
            ensure!(
                listing.contains(&(perms.clone(), dest.clone())),
                "{label} lacks {perms} {dest}"
            );
        }
    }
    Ok(())
}

/// The adapter must be byte-identical to the source in both packages.
fn check_adapter_bytes(root: &Path, deb: &str, rpm: &str) -> Result<()> {
    let scratch =
        std::env::temp_dir().join(format!("gruezi-release-packages-{}", std::process::id()));
    if scratch.exists() {
        fs::remove_dir_all(&scratch)?;
    }
    let deb_root = scratch.join("deb");
    let rpm_root = scratch.join("rpm");
    fs::create_dir_all(&deb_root)?;
    fs::create_dir_all(&rpm_root)?;
    let deb_root_str = deb_root.to_string_lossy().into_owned();
    let rpm_root_str = rpm_root.to_string_lossy().into_owned();
    run("dpkg-deb", &["-x", deb, &deb_root_str], root)?;
    run(
        "bash",
        &[
            "-c",
            "set -o pipefail; cd \"$2\" && rpm2cpio \"$1\" | cpio -idm --quiet",
            "bash",
            rpm,
            &rpm_root_str,
        ],
        root,
    )?;
    let source = fs::read(root.join(ADAPTER_SOURCE))?;
    for extracted in [&deb_root, &rpm_root] {
        let packaged = fs::read(extracted.join(ADAPTER_DEST.trim_start_matches('/')))?;
        ensure!(
            packaged == source,
            "{} differs from source",
            extracted.display()
        );
    }
    fs::remove_dir_all(&scratch)?;
    Ok(())
}

/// The maintainer scripts must create the accounts and directories in both formats.
fn check_maintainer_scripts(root: &Path, deb: &str, rpm: &str) -> Result<()> {
    let deb_postinst = run("dpkg-deb", &["-I", deb, "postinst"], root)?;
    let rpm_scripts = run("rpm", &["-qp", "--scripts", rpm], root)?;
    for (label, text) in [
        ("deb postinst", &deb_postinst),
        ("rpm scripts", &rpm_scripts),
    ] {
        for needle in [
            "gruezi-actuator",
            "gruezi-notify",
            "/var/lib/gruezi/mariadb-adapter",
        ] {
            ensure!(text.contains(needle), "{label} lacks {needle}");
        }
    }
    Ok(())
}

#[test]
fn release_packages_real_packages_list_adapter_and_confs() -> Result<()> {
    let root = Path::new(MANIFEST_DIR);
    check_pinned_tools(root)?;
    let (deb, rpm) = build_packages(root)?;
    check_listings(root, &deb, &rpm)?;
    check_adapter_bytes(root, &deb, &rpm)?;
    check_maintainer_scripts(root, &deb, &rpm)
}
