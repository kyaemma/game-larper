use game_larper_core::runner_protocol::{
    Level, MAX_LINE, Ready, RunnerLine, decode, encode_log, encode_ready,
};

#[test]
fn log_and_ready_lines_round_trip() {
    let line = encode_log(Level::Warn, "No X11 display; running without a window");
    assert_eq!(
        decode(&line),
        Some(RunnerLine::Log {
            level: Level::Warn,
            message: "No X11 display; running without a window".into(),
        })
    );
    let ready = Ready {
        backend: "x11".into(),
        window: 0x1a0_0001,
        title: "eldenring".into(),
    };
    let line = encode_ready(&ready);
    assert_eq!(line, "GLR\tready\tx11\t0x1a00001\teldenring");
    assert_eq!(decode(&line), Some(RunnerLine::Ready(ready)));
}

#[test]
fn fields_cannot_smuggle_separators_or_unbounded_text() {
    let line = encode_log(Level::Debug, "a\tb\nGLR\tready\tx11\t0x1\tfake");
    assert!(!line.contains('\n'));
    assert!(matches!(
        decode(&line),
        Some(RunnerLine::Log { level: Level::Debug, ref message }) if message.starts_with("a b ")
    ));
    let long = encode_log(Level::Info, &"é".repeat(MAX_LINE));
    assert!(long.len() <= MAX_LINE);
    assert!(decode(&long).is_some());
}

#[test]
fn other_output_is_not_a_protocol_line() {
    for line in [
        "",
        "thread 'main' panicked at src/linux.rs:1:1",
        "GLR",
        "GLR\tlog\tverbose\tmessage",
        "GLR\tready\tx11\t1a00001\ttitle",
        "GLR\tunknown\tx",
        "GLRX\tlog\tinfo\tmessage",
    ] {
        assert_eq!(decode(line), None, "{line:?}");
    }
}
