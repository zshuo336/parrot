use parrot_api::types::BoxedMessage;
use remote_poc::CodecRegistry;

// This executable owns a fresh process-global registry; it never resets a
// registry while other tests/requests are using it.
#[test]
fn empty_registry_errors_and_user_callback_panic_does_not_poison_it() {
    let msg: BoxedMessage = Box::new(1u32);
    assert!(CodecRegistry::encode_outgoing(&msg)
        .unwrap_err()
        .contains("not remotable"));
    assert!(CodecRegistry::decode_incoming("missing", &[]).is_err());
    CodecRegistry::install::<u32>(
        "test:u32",
        |_| panic!("user codec"),
        |_| panic!("user codec"),
    );
    assert!(std::panic::catch_unwind(|| CodecRegistry::encode_outgoing(
        &(Box::new(1u32) as BoxedMessage)
    ))
    .is_err());
    assert!(std::panic::catch_unwind(|| CodecRegistry::decode_incoming("test:u32", &[])).is_err());
    CodecRegistry::install::<u32>(
        "test:u32",
        |_| {
            // Reentry must not deadlock under the global lock.
            CodecRegistry::install::<u8>("test:u8", |_| Ok(vec![7]), |_| Ok(Box::new(7u8)));
            Ok(vec![1])
        },
        |_| {
            let nested = CodecRegistry::decode_incoming("test:u8", &[])?;
            assert_eq!(*nested.downcast_ref::<u8>().unwrap(), 7);
            Ok(Box::new(1u32))
        },
    );
    assert_eq!(
        CodecRegistry::encode_outgoing(&msg).unwrap(),
        ("test:u32".into(), vec![1])
    );
    assert_eq!(
        *CodecRegistry::decode_incoming("test:u32", &[])
            .unwrap()
            .downcast_ref::<u32>()
            .unwrap(),
        1
    );
}
