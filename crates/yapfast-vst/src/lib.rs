//! VST2 and VST3 effects on Yapfast's microphone: the plugins installed,
//! found without loading them ([`scan`]), and a chain of them that a call's
//! microphone runs through, in order, each one bypassed or not, its editor
//! in a window of its own, kept with what each plugin keeps of itself in
//! `voice-effects.json`.
//!
//! The plugins live on a thread of their own, which loads them, opens their
//! editors and runs those windows' messages, as a plugin's own UI thread;
//! the microphone's thread runs them on its sound ([`process`]), 10 ms of
//! 48 kHz stereo at a time. Changing the chain holds the microphone's
//! thread off it for that block, which passes through unchanged.
//!
//! Plugins load only on Windows; elsewhere the chain stays empty.

use std::path::{Path, PathBuf};

mod scan;
#[cfg(windows)]
mod win;

pub use scan::{Format, Found, folders, look, scan, vst3_binary};

/// Whether plugins load on this platform.
pub const AVAILABLE: bool = cfg!(windows);
/// The rate and the most frames at a time the chain runs at.
pub const RATE: u32 = 48_000;
pub const MOST_FRAMES: usize = 1024;

/// The chain as Settings show it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct View {
    /// Its switch: off, the microphone runs through none of it.
    pub on: bool,
    pub slots: Vec<Slot>,
    /// Loading what was kept, as Yapfast starts using it.
    pub loading: bool,
    /// Counts each change.
    pub version: u64,
}

/// A plugin in the chain.
#[derive(Clone, Debug, PartialEq)]
pub struct Slot {
    /// Its place's id, for as long as Yapfast runs.
    pub id: u64,
    pub name: String,
    pub vendor: String,
    pub format: Format,
    pub path: PathBuf,
    pub bypassed: bool,
    /// It has an editor, and it's open.
    pub editor: bool,
    pub open: bool,
    /// Why it isn't running, such as a plugin that failed to load.
    pub failed: Option<String>,
}

/// What the chain is asked.
#[derive(Clone, Debug)]
pub enum Request {
    /// Loads what was kept, if it hasn't been.
    Start,
    /// Turns the chain on or off.
    On(bool),
    /// Loads a plugin at the chain's end.
    Add(Found),
    Remove(u64),
    /// Moves the plugin at one place to another.
    Move(usize, usize),
    Bypass(u64, bool),
    /// Opens a plugin's editor, or brings it to the front.
    Edit(u64),
    /// Keeps the chain and each plugin's state.
    Save,
}

/// What's told once a request is done: why it couldn't be, if it couldn't.
pub type Done = Box<dyn FnOnce(Result<(), String>) + Send>;

/// Where the chain is kept: `voice-effects.json` in `dir`, Yapfast's data
/// folder. Nothing loads until the chain is used.
pub fn set_folder(dir: &Path) {
    #[cfg(windows)]
    win::set_folder(dir);
    #[cfg(not(windows))]
    let _ = dir;
}

/// Starts loading the chain kept, if one is and it hasn't started: as a
/// call's microphone opens, so its effects run once they've loaded.
/// Without one kept, nothing starts.
pub fn warm() {
    #[cfg(windows)]
    win::warm();
}

/// Asks the chain's thread for `request`, starting it first; `done` is
/// told on that thread once it's done.
pub fn request(request: Request, done: Done) {
    #[cfg(windows)]
    win::request(request, done);
    #[cfg(not(windows))]
    {
        let _ = request;
        done(Err("Voice effects run on Windows.".into()));
    }
}

/// The chain as it stands.
pub fn view() -> std::sync::Arc<View> {
    #[cfg(windows)]
    return win::view();
    #[cfg(not(windows))]
    std::sync::Arc::default()
}

/// Whether the microphone runs through any plugin now: the chain is on and
/// one in it runs.
pub fn active() -> bool {
    #[cfg(windows)]
    return win::active();
    #[cfg(not(windows))]
    false
}

/// Runs interleaved stereo `samples` through the chain, in place, as the
/// microphone's thread does each block: true when any plugin ran on them,
/// false when they passed through unchanged (the chain off, empty, or
/// being changed).
pub fn process(samples: &mut [f32]) -> bool {
    #[cfg(windows)]
    return win::process(samples);
    #[cfg(not(windows))]
    {
        let _ = samples;
        false
    }
}

/// Loads the plugin at `path` on its own, runs a second of a tone through
/// it, saves and restores its state, and with `editor` opens its editor for
/// that long: what each step did, for trying a plugin outside Yapfast.
pub fn probe(path: &Path, format: Format, editor: Option<std::time::Duration>) -> String {
    #[cfg(windows)]
    return win::probe(path, format, editor);
    #[cfg(not(windows))]
    {
        let _ = (path, format, editor);
        "Voice effects run on Windows.".into()
    }
}
