//! Tries a VST plugin outside Yapfast, as Yapfast's chain loads it:
//!
//! ```text
//! vst-probe                         lists the plugins found
//! vst-probe [--editor SECONDS] PATH loads one, runs a tone through it,
//!                                   saves and restores its state, and
//!                                   opens its editor for that long
//! ```

use std::{path::PathBuf, time::Duration};

use yapfast_vst::Format;

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut editor, mut path) = (None, None);
    while let Some(arg) = args.next() {
        if arg == "--editor" {
            editor = args
                .next()
                .and_then(|seconds| seconds.parse::<u64>().ok())
                .map(Duration::from_secs);
        } else {
            path = Some(PathBuf::from(arg));
        }
    }
    let Some(path) = path else {
        for found in yapfast_vst::scan(&[]) {
            let why = found
                .unusable
                .map(|why| format!(" ({why})"))
                .unwrap_or_default();
            println!(
                "{} [{}] {}{why}",
                found.name,
                found.format.label(),
                found.path.display()
            );
        }
        return;
    };
    let format = if path
        .extension()
        .is_some_and(|ending| ending.eq_ignore_ascii_case("vst3"))
    {
        Format::Vst3
    } else {
        Format::Vst2
    };
    print!("{}", yapfast_vst::probe(&path, format, editor));
}
