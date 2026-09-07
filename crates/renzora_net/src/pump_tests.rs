use super::*;

unsafe extern "C" fn reply_status(call: *const sys::NetCall) -> NetStatus {
    // SAFETY: NetLink::call provides a live NetCall for the callback.
    NetStatus(unsafe { (*call).state as usize } as i32)
}

#[test]
fn backend_statuses_preserve_errors_and_poison_only_unusable_backends() {
    let mut absent = NetLink::default();
    assert_eq!(absent.call(NetOp::Poll, &[], &[]), Ok(None));
    for status in [
        NetStatus::Ok,
        NetStatus::UnknownOp,
        NetStatus::Error,
        NetStatus::Panicked,
        NetStatus(999),
    ] {
        let mut link = NetLink {
            backend: Some(Loaded {
                name: "fixture".into(),
                state: status.0 as usize,
                entry: reply_status,
            }),
            ..Default::default()
        };
        let result = link.call(NetOp::Poll, &[], &[]);
        if status == NetStatus::Ok {
            assert_eq!(result, Ok(Some(Vec::new())));
        } else if status == NetStatus::UnknownOp {
            assert_eq!(result, Ok(None));
        } else {
            assert!(!result.unwrap_err().is_empty());
        }
        let poisoned = status == NetStatus::Panicked || !status.is_known();
        assert_eq!(link.poisoned, poisoned);
        assert_eq!(link.is_active(), !poisoned);
        if poisoned {
            assert_eq!(link.call(NetOp::Poll, &[], &[]), Ok(None));
        }
    }
    assert!(decode_error(&[]).contains("could not describe"));
}
