//! Sink lifecycle coverage moved out of `src/extensions/user_bash.rs`; the
//! source-hygiene gate forbids inline tests in `src/`.

use std::sync::{Arc, Mutex};

use pir::extensions::user_bash::{deliver_chunk, register_sink_for_test};

#[test]
fn chunk_delivery_reaches_the_registered_sink() {
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let registration = {
        let seen = Arc::clone(&seen);
        register_sink_for_test(
            "t1",
            Arc::new(move |data: &str| {
                seen.lock().unwrap().push(data.to_owned());
            }),
        )
    };
    deliver_chunk("t1", "hello");
    deliver_chunk("missing", "ignored");
    assert_eq!(*seen.lock().unwrap(), vec!["hello".to_owned()]);
    drop(registration);
    deliver_chunk("t1", "ignored");
    assert_eq!(seen.lock().unwrap().len(), 1);
}
