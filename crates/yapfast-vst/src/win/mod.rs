//! The chain on Windows. Its plugins live on a thread of their own, as
//! their UI thread: it loads them, opens their editors in windows of its
//! own and runs those windows' messages, keeps the chain in
//! `voice-effects.json`, and takes requests through a hidden window, so
//! they're heard even inside a plugin's own dialog. The microphone's thread
//! runs the chain's plugins on its sound, holding the chain's lock for the
//! block; while the plugins' thread changes the chain, a block passes
//! through unchanged.
//!
//! A plugin that takes Yapfast down as it loads is remembered: a file names
//! it while it loads, and if Yapfast starts again with that file there, it
//! starts with that plugin off.

use std::{
    cell::RefCell,
    ffi::{CStr, OsStr},
    fmt::Write as _,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::{
        Arc, LazyLock, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use windows_sys::Win32::{
    Foundation::{FreeLibrary, HMODULE, HWND, LPARAM, LRESULT, WPARAM},
    System::{
        LibraryLoader::{GetProcAddress, LoadLibraryW},
        Ole::OleInitialize,
    },
    UI::WindowsAndMessaging::{
        DestroyWindow, DispatchMessageW, GetMessageW, KillTimer, MSG, PM_REMOVE, PeekMessageW,
        PostMessageW, SetTimer, TranslateMessage, WM_APP, WM_CLOSE, WM_TIMER,
    },
};

use crate::{Done, Format, Found, Request, Slot, View};

mod vst2;
mod vst3;
pub mod window;

/// The most frames, and channels, a plugin is handed at once.
pub const MAX_BLOCK: usize = crate::MOST_FRAMES;
pub const MAX_CHANNELS: usize = 32;
/// A request waiting for the plugins' thread.
const WAKE: u32 = WM_APP + 1;
/// How often a VST2 editor is told to idle.
const IDLE_MS: u32 = 30;
const IDLE_TIMER: usize = 1;
/// The files the chain is kept in, and the one naming a plugin loading.
const KEPT: &str = "voice-effects.json";
const LOADING: &str = "voice-effects.loading";

/// A path as Windows takes it: UTF-16, null-terminated.
pub fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(Some(0)).collect()
}

type Symbol = unsafe extern "system" fn() -> isize;

/// A plugin's library, loaded, freed as it's dropped; a VST3 module is told
/// first, as its `ExitDll` asks.
pub struct Library {
    module: HMODULE,
    exit: Option<Symbol>,
}

impl Library {
    pub fn load(path: &Path) -> Result<Self> {
        let path = wide(path.as_os_str());
        // SAFETY: a library the person chose as a plugin; loading it runs
        // its initializers, as any host's does.
        let module = unsafe { LoadLibraryW(path.as_ptr()) };
        if module.is_null() {
            bail!("Windows couldn't load it");
        }
        Ok(Self { module, exit: None })
    }

    pub fn symbol(&self, name: &CStr) -> Option<Symbol> {
        // SAFETY: a null-terminated name in a loaded library.
        unsafe { GetProcAddress(self.module, name.as_ptr().cast()) }
    }

    /// Calls the module's `ExitDll` before it's freed.
    pub fn exit_with(&mut self, exit: Option<Symbol>) {
        self.exit = exit;
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        if let Some(exit) = self.exit {
            // SAFETY: VST 3's `bool ExitDll()`, once everything of the
            // module was released.
            unsafe { exit() };
        }
        // SAFETY: loaded by `load`, freed once.
        unsafe { FreeLibrary(self.module) };
    }
}

/// Buffers a VST2 plugin's channels go through, the chain's.
pub struct Scratch {
    pub ins: Vec<Vec<f32>>,
    pub outs: Vec<Vec<f32>>,
}

impl Scratch {
    pub fn new() -> Self {
        Self {
            ins: (0..MAX_CHANNELS).map(|_| vec![0.0; MAX_BLOCK]).collect(),
            outs: (0..MAX_CHANNELS).map(|_| vec![0.0; MAX_BLOCK]).collect(),
        }
    }
}

/// A plugin of either format.
pub enum Plugin {
    Vst2(vst2::Plugin),
    Vst3(vst3::Plugin),
}

impl Plugin {
    fn process(&self, input: [&[f32]; 2], output: [&mut [f32]; 2], scratch: &mut Scratch) {
        match self {
            Self::Vst2(plugin) => plugin.process(input, output, scratch),
            Self::Vst3(plugin) => plugin.process(input, output),
        }
    }

    fn names(&self) -> (&str, &str) {
        match self {
            Self::Vst2(plugin) => (&plugin.name, &plugin.vendor),
            Self::Vst3(plugin) => (&plugin.name, &plugin.vendor),
        }
    }

    fn editor(&self) -> bool {
        match self {
            Self::Vst2(plugin) => plugin.editor,
            Self::Vst3(plugin) => plugin.editor,
        }
    }
}

/// A plugin in the chain, shared by the microphone's thread and the
/// plugins' thread, which alone lets it go.
pub struct Loaded {
    plugin: Plugin,
    bypassed: AtomicBool,
}

/// The chain the microphone runs through, and the buffers it runs in.
struct Chain {
    slots: Vec<Arc<Loaded>>,
    a: [Vec<f32>; 2],
    b: [Vec<f32>; 2],
    scratch: Scratch,
}

impl Chain {
    fn new() -> Self {
        let buffer = || [vec![0.0; MAX_BLOCK], vec![0.0; MAX_BLOCK]];
        Self {
            slots: Vec::new(),
            a: buffer(),
            b: buffer(),
            scratch: Scratch::new(),
        }
    }

    /// Runs interleaved stereo `samples` through each plugin not bypassed,
    /// in order; true when any ran. What isn't a number comes out silent.
    fn run(&mut self, samples: &mut [f32]) -> bool {
        let Self {
            slots,
            a,
            b,
            scratch,
        } = self;
        let mut any = false;
        for block in samples.chunks_mut(MAX_BLOCK * 2) {
            let frames = block.len() / 2;
            for (at, frame) in block.as_chunks::<2>().0.iter().enumerate() {
                a[0][at] = frame[0];
                a[1][at] = frame[1];
            }
            let mut ran = false;
            for slot in slots
                .iter()
                .filter(|slot| !slot.bypassed.load(Ordering::Relaxed))
            {
                {
                    let [a_left, a_right] = &mut *a;
                    let [b_left, b_right] = &mut *b;
                    slot.plugin.process(
                        [&a_left[..frames], &a_right[..frames]],
                        [&mut b_left[..frames], &mut b_right[..frames]],
                        scratch,
                    );
                }
                std::mem::swap(a, b);
                ran = true;
            }
            if ran {
                for (at, frame) in block.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                    let sound = |value: f32| if value.is_finite() { value } else { 0.0 };
                    frame[0] = sound(a[0][at]);
                    frame[1] = sound(a[1][at]);
                }
            }
            any |= ran;
        }
        any
    }
}

/// What the threads share.
struct Shared {
    folder: Mutex<Option<PathBuf>>,
    chain: Mutex<Chain>,
    view: RwLock<Arc<View>>,
    active: AtomicBool,
    thread: Mutex<Option<Thread>>,
}

/// The plugins' thread: where its requests go, and its hidden window.
struct Thread {
    sender: mpsc::Sender<(Request, Done)>,
    window: isize,
}

static SHARED: LazyLock<Shared> = LazyLock::new(|| Shared {
    folder: Mutex::new(None),
    chain: Mutex::new(Chain::new()),
    view: RwLock::new(Arc::default()),
    active: AtomicBool::new(false),
    thread: Mutex::new(None),
});

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn set_folder(dir: &Path) {
    *lock(&SHARED.folder) = Some(dir.to_path_buf());
}

pub fn view() -> Arc<View> {
    SHARED
        .view
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

pub fn active() -> bool {
    SHARED.active.load(Ordering::Relaxed)
}

pub fn process(samples: &mut [f32]) -> bool {
    if !active() {
        return false;
    }
    match SHARED.chain.try_lock() {
        Ok(mut chain) => chain.run(samples),
        Err(_) => false,
    }
}

pub fn request(request: Request, done: Done) {
    let mut thread = lock(&SHARED.thread);
    if thread.is_none() {
        *thread = start();
    }
    let Some(running) = thread.as_ref() else {
        done(Err("Yapfast couldn't start its voice effects.".into()));
        return;
    };
    match running.sender.send((request, done)) {
        // SAFETY: the plugins' thread's hidden window, which lasts as long
        // as Yapfast.
        Ok(()) => unsafe {
            PostMessageW(running.window as HWND, WAKE, 0, 0);
        },
        Err(mpsc::SendError((_, done))) => {
            *thread = None;
            done(Err("Yapfast's voice effects stopped.".into()));
        }
    }
}

/// Starts the plugins' thread, once it has its window.
fn start() -> Option<Thread> {
    let (sender, requests) = mpsc::channel();
    let (ready, window) = mpsc::channel();
    let started = std::thread::Builder::new()
        .name("yapfast-plugins".into())
        .spawn(move || run(requests, &ready));
    if let Err(error) = started {
        tracing::warn!(%error, "could not start the plugins' thread");
        return None;
    }
    let window = window.recv_timeout(Duration::from_secs(10)).ok()?;
    Some(Thread { sender, window })
}

thread_local! {
    /// The plugins' thread's state.
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
    /// Editors closed while the host was busy, to close once it isn't.
    static CLOSING: RefCell<Vec<isize>> = const { RefCell::new(Vec::new()) };
}

/// The plugins' thread: COM for their editors, its windows, and their
/// messages until Yapfast ends.
fn run(requests: mpsc::Receiver<(Request, Done)>, ready: &mpsc::Sender<isize>) {
    // SAFETY: COM on this thread, single-threaded, as plugins' editors
    // expect of their UI thread.
    unsafe { OleInitialize(null()) };
    window::register(procedure);
    let messages = window::messages();
    HOST.with(|host| *host.borrow_mut() = Some(Host::new(requests)));
    let _ = ready.send(messages as isize);
    let mut message = MSG::default();
    // SAFETY: this thread's own queue.
    while unsafe { GetMessageW(&mut message, null_mut(), 0, 0) } > 0 {
        // SAFETY: a message just taken off the queue.
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

/// Runs `act` on the host, unless it's busy, as when a plugin's dialog runs
/// its messages inside a request: false then.
fn with_host(act: impl FnOnce(&mut Host)) -> bool {
    HOST.with(|host| match host.try_borrow_mut() {
        Ok(mut host) => {
            if let Some(host) = host.as_mut() {
                act(host);
            }
            true
        }
        Err(_) => false,
    })
}

/// The windows' messages on the plugins' thread.
unsafe extern "system" fn procedure(window: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match message {
        // A busy host takes it once it's done with the request it's on.
        WAKE => {
            with_host(Host::drain);
            0
        }
        WM_TIMER => {
            with_host(|host| host.idle(window));
            0
        }
        WM_CLOSE => {
            if !with_host(|host| host.close_window(window)) {
                CLOSING.with(|closing| closing.borrow_mut().push(window as isize));
            }
            0
        }
        // SAFETY: as the system gave it.
        _ => unsafe { window::default(window, message, w, l) },
    }
}

/// A plugin as `voice-effects.json` keeps it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct KeptPlugin {
    path: PathBuf,
    format: Option<Format>,
    /// A VST3 plugin's class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    class: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    vendor: String,
    #[serde(default)]
    bypassed: bool,
    /// A VST2 plugin's state is its chunk, rather than its parameters.
    #[serde(default)]
    chunk: bool,
    /// Its state, and a VST3 controller's, in base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    controller: Option<String>,
}

/// The chain as kept.
#[derive(Debug, Serialize, Deserialize)]
struct Kept {
    #[serde(default = "on")]
    on: bool,
    #[serde(default)]
    plugins: Vec<KeptPlugin>,
}

fn on() -> bool {
    true
}

fn encode(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn decode(text: Option<&String>) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(text?).ok()
}

/// A place in the chain.
struct Entry {
    id: u64,
    kept: KeptPlugin,
    loaded: Option<Arc<Loaded>>,
    failed: Option<String>,
    /// Its editor's window, while it's open.
    window: Option<HWND>,
}

/// The plugins' thread's state: the chain's places, whether it's on, and
/// the requests waiting.
struct Host {
    requests: mpsc::Receiver<(Request, Done)>,
    folder: Option<PathBuf>,
    started: bool,
    on: bool,
    entries: Vec<Entry>,
    next_id: u64,
    version: u64,
}

impl Host {
    fn new(requests: mpsc::Receiver<(Request, Done)>) -> Self {
        Self {
            requests,
            folder: None,
            started: false,
            on: true,
            entries: Vec::new(),
            next_id: 1,
            version: 0,
        }
    }

    /// Carries out each request waiting, then closes the editors closed
    /// meanwhile.
    fn drain(&mut self) {
        while let Ok((request, done)) = self.requests.try_recv() {
            let result = self.handle(request);
            done(result);
            for window in CLOSING.with(|closing| std::mem::take(&mut *closing.borrow_mut())) {
                self.close_window(window as HWND);
            }
        }
    }

    fn handle(&mut self, request: Request) -> Result<(), String> {
        self.start();
        match request {
            Request::Start => {}
            Request::On(on) => {
                self.on = on;
                self.changed();
            }
            Request::Add(found) => return self.add(found),
            Request::Remove(id) => self.remove(id),
            Request::Move(from, to) => {
                if from < self.entries.len() {
                    let entry = self.entries.remove(from);
                    self.entries.insert(to.min(self.entries.len()), entry);
                    self.changed();
                }
            }
            Request::Bypass(id, bypassed) => return self.bypass(id, bypassed),
            Request::Edit(id) => return self.edit(id),
            Request::Save => self.save(),
        }
        Ok(())
    }

    /// Loads what was kept, the first time it's asked for anything.
    fn start(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        self.folder = lock(&SHARED.folder).clone();
        let kept = self.read();
        self.on = kept.on;
        let crashed = self.loading();
        for plugin in kept.plugins {
            let mut entry = Entry {
                id: self.next(),
                kept: plugin,
                loaded: None,
                failed: None,
                window: None,
            };
            if crashed.as_ref() == Some(&entry.kept.path) {
                entry.kept.bypassed = true;
                entry.failed =
                    Some("Yapfast closed while loading it. Turn it on to try again.".into());
                tracing::warn!(path = %entry.kept.path.display(), "a plugin took Yapfast down as it loaded; it starts off");
            } else {
                self.load(&mut entry);
            }
            self.entries.push(entry);
        }
        self.set_loading(None);
        self.rebuild();
        self.publish();
    }

    fn next(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn read(&self) -> Kept {
        let kept = self
            .folder
            .as_ref()
            .and_then(|folder| std::fs::read(folder.join(KEPT)).ok())
            .and_then(|data| serde_json::from_slice::<Kept>(&data).ok());
        kept.unwrap_or(Kept {
            on: true,
            plugins: Vec::new(),
        })
    }

    /// The plugin that was loading when Yapfast last closed, if one was.
    fn loading(&self) -> Option<PathBuf> {
        let folder = self.folder.as_ref()?;
        std::fs::read_to_string(folder.join(LOADING))
            .ok()
            .map(PathBuf::from)
    }

    fn set_loading(&self, path: Option<&Path>) {
        let Some(folder) = &self.folder else {
            return;
        };
        let marker = folder.join(LOADING);
        match path {
            Some(path) => {
                let _ = std::fs::write(marker, path.to_string_lossy().as_bytes());
            }
            None => {
                let _ = std::fs::remove_file(marker);
            }
        }
    }

    /// Loads an entry's plugin with its kept state, or says why it
    /// couldn't.
    fn load(&self, entry: &mut Entry) {
        let kept = &mut entry.kept;
        self.set_loading(Some(&kept.path));
        let state = decode(kept.state.as_ref());
        let loaded = match kept.format.unwrap_or(Format::Vst2) {
            Format::Vst2 => vst2::Plugin::load(&kept.path).map(|plugin| {
                if let Some(state) = &state {
                    plugin.restore(kept.chunk, state);
                }
                Plugin::Vst2(plugin)
            }),
            Format::Vst3 => {
                let controller = decode(kept.controller.as_ref()).unwrap_or_default();
                let class = kept.class.as_deref().and_then(vst3::class_from_hex);
                let state = state.as_deref().map(|state| (state, controller.as_slice()));
                vst3::Plugin::load(&kept.path, class, state).map(|plugin| {
                    kept.class = Some(vst3::class_hex(&plugin.class));
                    Plugin::Vst3(plugin)
                })
            }
        };
        self.set_loading(None);
        match loaded {
            Ok(plugin) => {
                let (name, vendor) = plugin.names();
                if !name.is_empty() {
                    kept.name = name.to_owned();
                }
                if !vendor.is_empty() {
                    kept.vendor = vendor.to_owned();
                }
                entry.loaded = Some(Arc::new(Loaded {
                    plugin,
                    bypassed: AtomicBool::new(kept.bypassed),
                }));
                entry.failed = None;
            }
            Err(error) => {
                tracing::warn!(path = %kept.path.display(), error = format!("{error:#}"), "a plugin didn't load");
                entry.failed = Some(format!("Couldn't load it: {error}."));
            }
        }
    }

    fn add(&mut self, found: Found) -> Result<(), String> {
        if let Some(why) = found.unusable {
            return Err(format!("{} is {why}.", found.name));
        }
        let mut entry = Entry {
            id: self.next(),
            kept: KeptPlugin {
                path: found.path,
                format: Some(found.format),
                name: found.name.clone(),
                ..KeptPlugin::default()
            },
            loaded: None,
            failed: None,
            window: None,
        };
        self.load(&mut entry);
        if let Some(failed) = entry.failed {
            return Err(format!("{} wasn't added. {failed}", found.name));
        }
        self.entries.push(entry);
        self.changed();
        Ok(())
    }

    fn remove(&mut self, id: u64) {
        let Some(at) = self.entries.iter().position(|entry| entry.id == id) else {
            return;
        };
        if let Some(window) = self.entries[at].window {
            self.close_window(window);
        }
        let entry = self.entries.remove(at);
        self.changed();
        // Out of the chain, it goes here, on its own thread.
        drop(entry);
    }

    fn bypass(&mut self, id: u64, bypassed: bool) -> Result<(), String> {
        let Some(at) = self.entries.iter().position(|entry| entry.id == id) else {
            return Ok(());
        };
        let mut entry = std::mem::replace(
            &mut self.entries[at],
            Entry {
                id: 0,
                kept: KeptPlugin::default(),
                loaded: None,
                failed: None,
                window: None,
            },
        );
        entry.kept.bypassed = bypassed;
        // Turned on again after it failed, it's tried again.
        if !bypassed && entry.loaded.is_none() {
            self.load(&mut entry);
        }
        if let Some(loaded) = &entry.loaded {
            loaded.bypassed.store(bypassed, Ordering::Relaxed);
        }
        let failed = (!bypassed).then(|| entry.failed.clone()).flatten();
        if failed.is_some() {
            entry.kept.bypassed = true;
        }
        self.entries[at] = entry;
        self.changed();
        failed.map_or(Ok(()), Err)
    }

    /// Opens a plugin's editor in a window of its own, or brings it to the
    /// front.
    fn edit(&mut self, id: u64) -> Result<(), String> {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
            return Ok(());
        };
        if let Some(window) = entry.window {
            window::show(window);
            return Ok(());
        }
        let Some(loaded) = entry.loaded.clone() else {
            return Err("It isn't loaded.".into());
        };
        let title = format!("{} \u{2014} Yapfast", entry.kept.name);
        let window = open_editor(&loaded.plugin, &title)?;
        entry.window = Some(window);
        self.publish();
        Ok(())
    }

    /// A VST2 editor's idle.
    fn idle(&self, window: HWND) {
        if let Some(Plugin::Vst2(plugin)) = self
            .entries
            .iter()
            .find(|entry| entry.window == Some(window))
            .and_then(|entry| entry.loaded.as_ref())
            .map(|loaded| &loaded.plugin)
        {
            plugin.idle_editor();
        }
    }

    /// Closes the editor in `window`, keeping what it changed.
    fn close_window(&mut self, window: HWND) {
        let entry = self
            .entries
            .iter_mut()
            .find(|entry| entry.window == Some(window));
        match entry {
            Some(entry) => {
                entry.window = None;
                if let Some(loaded) = &entry.loaded {
                    close_editor(&loaded.plugin, window);
                }
                self.save();
                self.publish();
            }
            None => {
                // SAFETY: a window of this thread's that nothing holds.
                unsafe { DestroyWindow(window) };
            }
        }
    }

    /// The chain changed: it runs so, is kept so and shows so.
    fn changed(&mut self) {
        self.rebuild();
        self.save();
        self.publish();
    }

    /// The microphone's thread's chain, in the entries' order.
    fn rebuild(&self) {
        let slots = self
            .entries
            .iter()
            .filter_map(|entry| entry.loaded.clone())
            .collect();
        lock(&SHARED.chain).slots = slots;
    }

    /// Reads each plugin's state, then keeps the chain.
    fn save(&mut self) {
        for entry in &mut self.entries {
            let Some(loaded) = &entry.loaded else {
                continue;
            };
            match &loaded.plugin {
                Plugin::Vst2(plugin) => {
                    let (chunk, data) = plugin.state();
                    entry.kept.chunk = chunk;
                    entry.kept.state = Some(encode(&data));
                }
                Plugin::Vst3(plugin) => {
                    // The editor's last edits reach the processor first,
                    // the microphone's thread held off.
                    {
                        let _chain = lock(&SHARED.chain);
                        plugin.flush();
                    }
                    let (component, controller) = plugin.state();
                    entry.kept.state = Some(encode(&component));
                    entry.kept.controller = Some(encode(&controller));
                }
            }
        }
        let Some(folder) = &self.folder else {
            return;
        };
        let kept = Kept {
            on: self.on,
            plugins: self
                .entries
                .iter()
                .map(|entry| entry.kept.clone())
                .collect(),
        };
        let Ok(data) = serde_json::to_vec_pretty(&kept) else {
            return;
        };
        let written = folder.join(format!("{KEPT}.new"));
        let saved = std::fs::create_dir_all(folder)
            .and_then(|()| std::fs::write(&written, data))
            .and_then(|()| std::fs::rename(&written, folder.join(KEPT)));
        if let Err(error) = saved {
            tracing::warn!(%error, "could not keep the voice effects");
        }
    }

    /// Shows the chain as it stands, and runs it or not.
    fn publish(&mut self) {
        self.version += 1;
        let slots = self
            .entries
            .iter()
            .map(|entry| {
                let file = || {
                    entry
                        .kept
                        .path
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                        .unwrap_or_default()
                };
                Slot {
                    id: entry.id,
                    name: if entry.kept.name.is_empty() {
                        file()
                    } else {
                        entry.kept.name.clone()
                    },
                    vendor: entry.kept.vendor.clone(),
                    format: entry.kept.format.unwrap_or(Format::Vst2),
                    path: entry.kept.path.clone(),
                    bypassed: entry.kept.bypassed,
                    editor: entry
                        .loaded
                        .as_ref()
                        .is_some_and(|loaded| loaded.plugin.editor()),
                    open: entry.window.is_some(),
                    failed: entry.failed.clone(),
                }
            })
            .collect();
        let active = self.on
            && self
                .entries
                .iter()
                .any(|entry| entry.loaded.is_some() && !entry.kept.bypassed);
        *SHARED
            .view
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(View {
            on: self.on,
            slots,
            loading: false,
            version: self.version,
        });
        SHARED.active.store(active, Ordering::Relaxed);
    }
}

/// Opens `plugin`'s editor in a new window titled `title`, shown.
fn open_editor(plugin: &Plugin, title: &str) -> Result<HWND, String> {
    let window = match plugin {
        Plugin::Vst2(plugin) => {
            if !plugin.editor {
                return Err("It has no editor.".into());
            }
            let (width, height) = plugin.editor_size().unwrap_or((480, 320));
            let window = window::editor(title, width, height);
            if window.is_null() {
                return Err("Windows couldn't open its window.".into());
            }
            plugin.open_editor(window);
            // Some say their size only once open.
            if let Some((width, height)) = plugin.editor_size() {
                window::resize(window, width, height);
            }
            // SAFETY: this thread's window, its timer killed as it closes.
            unsafe { SetTimer(window, IDLE_TIMER, IDLE_MS, None) };
            window
        }
        Plugin::Vst3(plugin) => {
            let (width, height) = plugin
                .open_view()
                .ok_or_else(|| "It has no editor Yapfast can show.".to_owned())?;
            let window = window::editor(title, width, height);
            if window.is_null() {
                plugin.close_view();
                return Err("Windows couldn't open its window.".into());
            }
            if !plugin.attach_view(window) {
                plugin.close_view();
                // SAFETY: the window just made, which holds nothing.
                unsafe { DestroyWindow(window) };
                return Err("Its editor wouldn't open.".into());
            }
            window
        }
    };
    window::show(window);
    Ok(window)
}

/// Closes `plugin`'s editor and its window.
fn close_editor(plugin: &Plugin, window: HWND) {
    match plugin {
        Plugin::Vst2(plugin) => {
            // SAFETY: the timer set as the editor opened.
            unsafe { KillTimer(window, IDLE_TIMER) };
            plugin.close_editor();
        }
        Plugin::Vst3(plugin) => plugin.close_view(),
    }
    // SAFETY: the editor's window, its view taken out.
    unsafe { DestroyWindow(window) };
}

/// The probe's windows take no messages of their own.
unsafe extern "system" fn quiet(window: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if message == WM_CLOSE {
        return 0;
    }
    // SAFETY: as the system gave it.
    unsafe { window::default(window, message, w, l) }
}

/// Loads the plugin at `path` on this thread, runs a second of a tone
/// through it, saves and restores its state, and with `editor` opens its
/// editor for that long: what each step did.
pub fn probe(path: &Path, format: Format, editor: Option<Duration>) -> String {
    let mut report = String::new();
    // SAFETY: COM on this thread, as an editor expects.
    unsafe { OleInitialize(null()) };
    window::register(quiet);
    let started = Instant::now();
    let loaded = match format {
        Format::Vst2 => vst2::Plugin::load(path).map(Plugin::Vst2),
        Format::Vst3 => vst3::Plugin::load(path, None, None).map(Plugin::Vst3),
    };
    let plugin = match loaded {
        Ok(plugin) => plugin,
        Err(error) => return format!("{}: didn't load: {error:#}\n", path.display()),
    };
    let (name, vendor) = plugin.names();
    let _ = writeln!(
        report,
        "loaded {name:?} by {vendor:?} ({}) in {} ms; editor: {}",
        format.label(),
        started.elapsed().as_millis(),
        plugin.editor()
    );
    let mut scratch = Scratch::new();
    let frames = 480;
    let (mut left, mut right) = (vec![0.0f32; frames], vec![0.0f32; frames]);
    let (mut out_left, mut out_right) = (vec![0.0f32; frames], vec![0.0f32; frames]);
    let (mut energy_in, mut energy_out, mut bad) = (0.0f64, 0.0f64, 0usize);
    let took = Instant::now();
    for block in 0..100 {
        for at in 0..frames {
            let time = (block * frames + at) as f32 / crate::RATE as f32;
            let tone = (std::f32::consts::TAU * 440.0 * time).sin() * 0.25;
            left[at] = tone;
            right[at] = tone * 0.5;
        }
        plugin.process(
            [&left, &right],
            [&mut out_left, &mut out_right],
            &mut scratch,
        );
        for at in 0..frames {
            energy_in += f64::from(left[at] * left[at] + right[at] * right[at]);
            let (l, r) = (out_left[at], out_right[at]);
            if !l.is_finite() || !r.is_finite() {
                bad += 1;
            }
            energy_out += f64::from(l * l + r * r);
        }
    }
    let db = |energy: f64| 10.0 * (energy / (100.0 * frames as f64 * 2.0)).max(1e-12).log10();
    let _ = writeln!(
        report,
        "a second of a 440 Hz tone in 100 blocks of 10 ms took {} ms: in {:.1} dBFS, out {:.1} dBFS{}",
        took.elapsed().as_millis(),
        db(energy_in),
        db(energy_out),
        if bad > 0 {
            ", with samples that aren't numbers"
        } else {
            ""
        }
    );
    match &plugin {
        Plugin::Vst2(plugin) => {
            let (chunk, data) = plugin.state();
            plugin.restore(chunk, &data);
            let _ = writeln!(
                report,
                "state: {} bytes as {}, put back",
                data.len(),
                if chunk { "a chunk" } else { "parameters" }
            );
        }
        Plugin::Vst3(plugin) => {
            plugin.flush();
            let (component, controller) = plugin.state();
            plugin.restore(&component, &controller);
            let _ = writeln!(
                report,
                "state: {} bytes of its component and {} of its controller, put back",
                component.len(),
                controller.len()
            );
        }
    }
    if let Some(open_for) = editor {
        match open_editor(&plugin, &format!("{name} \u{2014} Yapfast probe")) {
            Ok(window) => {
                let until = Instant::now() + open_for;
                let mut message = MSG::default();
                while Instant::now() < until {
                    // SAFETY: this thread's own queue.
                    while unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) } != 0 {
                        // SAFETY: a message just taken off the queue.
                        unsafe {
                            TranslateMessage(&message);
                            DispatchMessageW(&message);
                        }
                    }
                    if let Plugin::Vst2(plugin) = &plugin {
                        plugin.idle_editor();
                    }
                    std::thread::sleep(Duration::from_millis(IDLE_MS.into()));
                }
                close_editor(&plugin, window);
                let _ = writeln!(
                    report,
                    "editor opened for {} s and closed",
                    open_for.as_secs()
                );
            }
            Err(why) => {
                let _ = writeln!(report, "editor: {why}");
            }
        }
    }
    drop(plugin);
    let _ = writeln!(report, "unloaded");
    report
}
