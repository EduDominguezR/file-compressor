mod common;
use common::*;
use std::sync::mpsc;
use std::thread;

use megacompress::engine::cancel::CancelFlag;
use megacompress::engine::mode::{Config, Mode};
use megacompress::engine::orchestrator::{run, Job};
use megacompress::engine::Msg;
use megacompress::error::Error;
use zip::CompressionMethod;

#[test]
fn cancel_midway_removes_tmp_and_keeps_original() {
    let d = tempfile::tempdir().unwrap();
    let z = zip_in(d.path(), "grande.zip");
    // 6 entradas de 6 MB de ruido: con bloque de 1 MB cada una se transmite en streaming.
    let entries: Vec<_> = (0..6)
        .map(|i| (format!("r{i}.bin"), noise(6 * 1024 * 1024, i as u64)))
        .collect();
    make_zip(&z, &entries, CompressionMethod::Stored, false);
    let before = file_bytes(&z);

    let mut cfg = Config::for_system(Mode::Max, 2 * 1024 * 1024 * 1024, 1);
    cfg.block_bytes = 1024 * 1024;
    let mut job = Job::new(z.clone(), Mode::Max);
    job.config = Some(cfg);

    let cancel = CancelFlag::new();
    let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
    let c2 = cancel.clone();
    let h = thread::spawn(move || run(&job, &c2, tx, None));

    // Espera el primer progreso real y cancela a mitad de trabajo.
    let (done_tx, done_rx) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(m) = rx.recv() {
            if let Msg::Progress(s) = m {
                if s.done > 0 {
                    let _ = done_tx.send(());
                    break;
                }
            }
        }
    });
    done_rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("sin progreso");
    cancel.cancel();

    let res = h.join().unwrap();
    assert!(matches!(res, Err(Error::Cancelled)), "{res:?}");
    assert_eq!(listing(d.path()), vec!["grande.zip".to_string()]);
    assert_eq!(file_bytes(&z), before);
}

#[test]
fn cancel_before_start_is_clean() {
    let d = tempfile::tempdir().unwrap();
    let z = zip_in(d.path(), "x.zip");
    make_zip(&z, &three_texts(), CompressionMethod::Deflated, false);
    let cancel = CancelFlag::new();
    cancel.cancel();
    let e = megacompress::engine::orchestrator::run_quiet(&Job::new(z, Mode::Fast), &cancel)
        .unwrap_err();
    assert!(matches!(e, Error::Cancelled));
    assert_eq!(listing(d.path()), vec!["x.zip".to_string()]);
}
