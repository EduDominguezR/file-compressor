//! Estabilidad: instancias simultáneas, bloqueos reales (incluida la muerte de un proceso),
//! permisos y destino existente.
mod common;
use common::*;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use megacompress::engine::cancel::CancelFlag;
use megacompress::engine::mode::Mode;
use megacompress::engine::orchestrator::{run_quiet, Job};
#[cfg(unix)]
use megacompress::error::Error;
use megacompress::fs::atomic::{cleanup_orphans, cleanup_orphans_with, OpLock};
use zip::CompressionMethod;

#[test]
fn two_simultaneous_instances_do_not_interfere() {
    for mode in [Mode::Fast, Mode::Max] {
        let d = tempfile::tempdir().unwrap();
        let z = zip_in(d.path(), "x.zip");
        let entries: Vec<_> = (0..200)
            .map(|i| (format!("f{i}.txt"), text(50 + i, "t")))
            .collect();
        make_zip(&z, &entries, CompressionMethod::Deflated, false);
        let handles: Vec<_> = (0..3)
            .map(|_| {
                let z = z.clone();
                thread::spawn(move || run_mode(&z, mode))
            })
            .collect();
        let mut paths = Vec::new();
        for h in handles {
            let rep = h
                .join()
                .unwrap()
                .unwrap_or_else(|e| panic!("{mode:?}: {e}"));
            assert_eq!(extract(&rep.path, mode), expected_map(&entries));
            paths.push(rep.path);
        }
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), 3, "cada instancia publica un nombre distinto");
        let names = listing(d.path());
        assert!(
            names.iter().all(|n| !n.contains("megacompress")),
            "sin temporales/locks: {names:?}"
        );
    }
}

#[test]
fn a_running_job_does_not_delete_another_active_operation() {
    let d = tempfile::tempdir().unwrap();
    let z = zip_in(d.path(), "x.zip");
    make_zip(&z, &three_texts(), CompressionMethod::Deflated, false);
    // "Otra instancia" activa: bloqueo + temporal en la misma carpeta.
    let live = OpLock::acquire(d.path(), "otra", "7z").unwrap();
    std::fs::write(live.tmp_path(), b"datos en curso").unwrap();
    run_mode(&z, Mode::Fast).unwrap();
    assert_eq!(std::fs::read(live.tmp_path()).unwrap(), b"datos en curso");
    assert!(live.lock_path().exists());
    let rep = cleanup_orphans_with(d.path(), Duration::ZERO);
    assert_eq!(rep.removed_tmp, 0);
}

/// Proceso "hijo": toma el bloqueo, escribe un temporal, avisa y espera a que lo maten.
#[test]
#[ignore = "solo lo lanza `abandoned_lock_after_process_kill_is_recovered`"]
fn helper_hold_lock() {
    let dir = match std::env::var("MC_HELPER_DIR") {
        Ok(d) => std::path::PathBuf::from(d),
        Err(_) => return,
    };
    let lock = OpLock::acquire(&dir, "hijo", "7z").unwrap();
    std::fs::write(lock.tmp_path(), b"parcial").unwrap();
    println!("MC_READY {}", lock.tmp_path().display());
    thread::sleep(Duration::from_secs(120));
}

#[test]
fn abandoned_lock_after_process_kill_is_recovered() {
    let d = tempfile::tempdir().unwrap();
    let exe = std::env::current_exe().unwrap();
    let mut child = Command::new(exe)
        .args([
            "--exact",
            "helper_hold_lock",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("MC_HELPER_DIR", d.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let out = child.stdout.take().unwrap();
    let mut tmp_path = None;
    for line in BufReader::new(out).lines().map_while(Result::ok) {
        // libtest imprime «test helper_hold_lock ... » delante de la salida del test, así que
        // el marcador puede no estar al principio de la línea.
        if let Some((_, p)) = line.split_once("MC_READY ") {
            tmp_path = Some(std::path::PathBuf::from(p.trim()));
            break;
        }
    }
    let tmp = tmp_path.expect("el proceso hijo no tomó el bloqueo");

    // Mientras el hijo vive, la limpieza NO puede tocar su temporal (aunque no haya margen).
    let rep = cleanup_orphans_with(d.path(), Duration::ZERO);
    assert_eq!(rep.removed_tmp, 0);
    assert!(tmp.exists());

    // Cierre inesperado: se mata al proceso; el SO libera el bloqueo y la limpieza lo recupera.
    child.kill().unwrap();
    child.wait().unwrap();
    let rep = cleanup_orphans_with(d.path(), Duration::ZERO);
    assert_eq!(rep.removed_tmp, 1, "{rep:?}");
    assert!(!tmp.exists());
    assert_eq!(cleanup_orphans(d.path()).removed_tmp, 0);
}

#[cfg(unix)]
#[test]
fn unwritable_destination_gives_permission_error_and_keeps_original() {
    use std::os::unix::fs::PermissionsExt;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let z = zip_in(src.path(), "x.zip");
    make_zip(&z, &three_texts(), CompressionMethod::Deflated, false);
    let before = file_bytes(&z);
    std::fs::set_permissions(dst.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    // Si somos root los permisos no se aplican: no hay nada que probar.
    if std::fs::File::create(dst.path().join("probe")).is_ok() {
        std::fs::set_permissions(dst.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("omitido: el usuario actual ignora los permisos de directorio");
        return;
    }
    let mut job = Job::new(z.clone(), Mode::Fast);
    job.out_dir = Some(dst.path().to_path_buf());
    let e = run_quiet(&job, &CancelFlag::new()).unwrap_err();
    std::fs::set_permissions(dst.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(e, Error::PermissionDenied(_)), "{e:?}");
    assert_eq!(file_bytes(&z), before);
    assert!(listing(dst.path()).is_empty());
}

#[test]
fn custom_destination_folder_is_used() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let z = zip_in(src.path(), "x.zip");
    make_zip(&z, &three_texts(), CompressionMethod::Deflated, false);
    let mut job = Job::new(z.clone(), Mode::Fast);
    job.out_dir = Some(dst.path().to_path_buf());
    let rep = run_quiet(&job, &CancelFlag::new()).unwrap();
    assert_eq!(rep.path.parent().unwrap(), dst.path());
    assert_eq!(listing(src.path()), vec!["x.zip".to_string()]);
}
