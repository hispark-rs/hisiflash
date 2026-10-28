use super::*;
use crate::target::ws53::{
    ApplicationSector,
    tests::{MockPort, d2_offsets, replacement_wire, response, upload_wire},
};
use std::io::{ErrorKind, Read};

fn loader_wire() -> Vec<Vec<u8>> {
    vec![
        [response(0x5a, 0).to_vec(), vec![b'C']].concat(),
        vec![6],
        vec![6],
        vec![6, b'C'],
        vec![6],
        response(0x5a, 0).to_vec(),
    ]
}

fn connected(chunks: Vec<Vec<u8>>) -> Ws53Session<MockPort> {
    let mut wire = loader_wire();
    wire.extend(chunks);
    let mut session = Ws53Session::new(
        MockPort::new(wire, ErrorKind::BrokenPipe),
        CancelContext::none(),
    );
    session
        .connect(Duration::from_secs(1))
        .unwrap();
    assert_eq!(session.phase(), SessionPhase::Rom);
    session
        .download_loader(&[0x42; 100])
        .unwrap();
    assert_eq!(session.phase(), SessionPhase::Loader);
    session
}

fn assert_reset_refused(session: &mut Ws53Session<MockPort>) {
    let writes = session
        .port
        .writes
        .len();
    assert!(matches!(
        session.reset_after_verify(),
        Err(SessionFailure::Phase { .. })
    ));
    assert_eq!(
        session
            .port
            .writes
            .len(),
        writes
    );
}

#[test]
fn session_apply_and_recover_require_full_verify_and_preserve_monitor_bytes() {
    let body = [0x11; 4096];
    let mut new_body = body;
    new_body[0x300..].fill(0x33);
    let header = [0x22; 4096];
    let mut new_header = header;
    new_header[0x128..0x148].fill(0x44);
    let sectors = [
        ApplicationSector {
            offset: 0x5d000,
            original: &body,
            replacement: &new_body,
        },
        ApplicationSector {
            offset: 0x30000,
            original: &header,
            replacement: &new_header,
        },
    ];
    let plan = ApplicationUpdate::new(&sectors).unwrap();
    for recovery in [false, true] {
        let erased = [0xff; 4096];
        let (before_body, before_header, after_body, after_header) = if recovery {
            (&erased, &erased, &body, &header)
        } else {
            (&body, &header, &new_body, &new_header)
        };
        let mut chunks = vec![upload_wire(before_body), upload_wire(before_header)];
        chunks.extend(replacement_wire(before_body, after_body));
        chunks.extend(replacement_wire(before_header, after_header));
        let mut expected = vec![0x65; 0x400000];
        expected[0x5d000..0x5e000].copy_from_slice(after_body);
        expected[0x30000..0x31000].copy_from_slice(after_header);
        chunks.extend(
            expected
                .chunks(0x40000)
                .map(upload_wire),
        );
        let mut session = connected(chunks);
        assert_reset_refused(&mut session);
        if recovery {
            session.recover(&plan)
        } else {
            session.apply(&plan)
        }
        .unwrap();
        assert_eq!(session.phase(), SessionPhase::RegionsVerified);
        assert_reset_refused(&mut session);
        let mut progress = vec![];
        session
            .verify_full_image(&expected, |bytes| progress.push(bytes))
            .unwrap();
        assert_eq!(
            progress,
            (1..=16)
                .map(|i| i * 0x40000)
                .collect::<Vec<_>>()
        );
        assert_eq!(session.phase(), SessionPhase::FullVerified);
        assert_eq!(
            d2_offsets(
                &session
                    .port
                    .writes
            ),
            [0x5d000, 0x30000]
        );
        session
            .reset_after_verify()
            .unwrap();
        assert_eq!(session.phase(), SessionPhase::ResetRequested);
        assert_reset_refused(&mut session);
        session
            .port
            .chunks
            .push_back(b"early boot\r\n".to_vec());
        let mut monitor = session
            .into_monitor_port()
            .unwrap();
        assert_eq!(
            &monitor.writes[monitor
                .writes
                .len()
                - 12..][..10],
            &[0xef, 0xbe, 0xad, 0xde, 12, 0, 0x87, 0x78, 0, 0]
        );
        let mut boot = [0; 12];
        monitor
            .read_exact(&mut boot)
            .unwrap();
        assert_eq!(&boot, b"early boot\r\n");
        assert_eq!(monitor.timeout, Duration::from_secs(3));
    }
}

#[test]
fn session_invalid_arguments_and_phases_do_not_touch_transport() {
    let mut session = Ws53Session::new(
        MockPort::new(vec![], ErrorKind::BrokenPipe),
        CancelContext::none(),
    );
    assert_reset_refused(&mut session);
    assert!(
        session
            .download_loader(&[1])
            .is_err()
    );
    assert!(
        session
            .read_flash(0, 4)
            .is_err()
    );
    assert!(
        session
            .verify_full_image(&[], |_| panic!("no progress"))
            .is_err()
    );
    assert!(
        session
            .connect(Duration::ZERO)
            .is_err()
    );
    assert!(
        session
            .connect(Duration::from_secs(31))
            .is_err()
    );
    assert_eq!(session.phase(), SessionPhase::Fresh);
    assert!(
        session
            .port
            .writes
            .is_empty()
    );
    assert!(matches!(
        session.into_monitor_port(),
        Err(SessionFailure::Phase { .. })
    ));

    let mut session = Ws53Session::new(
        MockPort::new(loader_wire(), ErrorKind::BrokenPipe),
        CancelContext::none(),
    );
    session
        .connect(Duration::from_secs(1))
        .unwrap();
    let writes = session
        .port
        .writes
        .len();
    for package in [vec![], vec![0; 0x10001]] {
        assert!(
            session
                .download_loader(&package)
                .is_err()
        );
        assert_eq!(session.phase(), SessionPhase::Rom);
    }
    assert_reset_refused(&mut session);
    assert_eq!(
        session
            .port
            .writes
            .len(),
        writes
    );
    session
        .download_loader(&[0; 100])
        .unwrap();
    let writes = session
        .port
        .writes
        .len();
    for (offset, length) in [
        (0, 0),
        (1, 4),
        (0, 0x40001),
        (0x400000, 4),
        (u32::MAX - 3, 8),
    ] {
        assert!(
            session
                .read_flash(offset, length)
                .is_err()
        );
        assert_eq!(session.phase(), SessionPhase::Loader);
    }
    assert!(
        session
            .connect(Duration::from_secs(1))
            .is_err()
    );
    assert!(
        session
            .download_loader(&[1])
            .is_err()
    );
    assert_eq!(
        session
            .port
            .writes
            .len(),
        writes
    );
}

#[test]
fn session_full_verify_mismatch_disconnect_and_cancel_forbid_reset() {
    for fault in 0..3 {
        let expected = vec![0x55; 0x400000];
        let mut chunks: Vec<_> = expected
            .chunks(0x40000)
            .take(15)
            .map(upload_wire)
            .collect();
        if fault == 0 {
            chunks.push(upload_wire(&vec![0x54; 0x40000]));
        }
        let mut session = connected(chunks);
        // Isolate verification faults; the successful update transition is
        // exercised above using real protocol frames and public operations.
        session.phase = SessionPhase::RegionsVerified;
        if fault == 2 {
            session.cancel = CancelContext::new(|| true);
        }
        let mut progress = vec![];
        assert!(
            session
                .verify_full_image(&expected, |n| progress.push(n))
                .is_err()
        );
        assert_eq!(progress.len(), if fault == 2 { 0 } else { 15 });
        assert_eq!(session.phase(), SessionPhase::Failed);
        assert_reset_refused(&mut session);
        let writes = session
            .port
            .writes
            .len();
        assert!(
            session
                .read_flash(0, 4)
                .is_err()
        );
        assert!(
            session
                .connect(Duration::from_secs(1))
                .is_err()
        );
        assert_eq!(
            session
                .port
                .writes
                .len(),
            writes
        );
    }
}

#[test]
fn session_protocol_failures_poison_connection_without_implicit_reset() {
    let mut session = Ws53Session::new(
        MockPort::new(vec![], ErrorKind::BrokenPipe),
        CancelContext::none(),
    );
    assert!(
        session
            .connect(Duration::from_secs(1))
            .is_err()
    );
    assert_eq!(session.phase(), SessionPhase::Failed);
    assert_reset_refused(&mut session);

    let mut session = Ws53Session::new(
        MockPort::new(vec![response(0x5a, 0).to_vec()], ErrorKind::BrokenPipe),
        CancelContext::none(),
    );
    session
        .connect(Duration::from_secs(1))
        .unwrap();
    assert!(
        session
            .download_loader(&[1])
            .is_err()
    );
    assert_eq!(session.phase(), SessionPhase::Failed);
    assert_reset_refused(&mut session);

    let mut session = connected(vec![upload_wire(&[1, 2, 3, 4])]);
    assert_eq!(
        session
            .read_flash(0, 4)
            .unwrap(),
        [1, 2, 3, 4]
    );
    assert_eq!(session.phase(), SessionPhase::Loader);
    assert!(
        session
            .read_flash(0, 4)
            .is_err()
    );
    assert_eq!(session.phase(), SessionPhase::Failed);
    assert_reset_refused(&mut session);
}

#[test]
fn session_later_write_failure_retains_partial_progress_and_requires_reconnect() {
    let body = [0x11; 4096];
    let mut replacement = body;
    replacement[0x300..].fill(0x33);
    let header = [0x22; 4096];
    let sectors = [
        ApplicationSector {
            offset: 0x5d000,
            original: &body,
            replacement: &replacement,
        },
        ApplicationSector {
            offset: 0x30000,
            original: &header,
            replacement: &header,
        },
    ];
    let plan = ApplicationUpdate::new(&sectors).unwrap();
    let mut chunks = vec![upload_wire(&body), upload_wire(&header)];
    chunks.extend(replacement_wire(&body, &replacement));
    let mut session = connected(chunks);
    let Err(SessionFailure::Application(failure)) = session.apply(&plan) else {
        panic!("must preserve application progress on a later disconnect");
    };
    assert_eq!(
        (failure.index, failure.offset, failure.verified_sectors),
        (1, 0x30000, 1)
    );
    assert!(failure.flash_may_have_changed);
    assert_eq!(session.phase(), SessionPhase::Failed);
    let writes = session
        .port
        .writes
        .len();
    assert_reset_refused(&mut session);
    assert!(
        session
            .apply(&plan)
            .is_err()
    );
    assert!(
        session
            .recover(&plan)
            .is_err()
    );
    assert_eq!(
        session
            .port
            .writes
            .len(),
        writes
    );

    // Even after verification, cancellation must prevent sending reset and
    // handing the port to an application monitor as if reset had succeeded.
    let mut session = connected(vec![]);
    session.phase = SessionPhase::FullVerified;
    session.cancel = CancelContext::new(|| true);
    let writes = session
        .port
        .writes
        .len();
    assert!(matches!(
        session.reset_after_verify(),
        Err(SessionFailure::Protocol(Error::Io(error))) if error.kind() == ErrorKind::Interrupted
    ));
    assert_eq!(session.phase(), SessionPhase::Failed);
    assert_eq!(
        session
            .port
            .writes
            .len(),
        writes
    );
    assert!(
        session
            .into_monitor_port()
            .is_err()
    );
}
