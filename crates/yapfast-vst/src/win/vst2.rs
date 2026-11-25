//! VST 2.4 plugins, through their binary interface as its SDK laid it out:
//! the `AEffect` a plugin's `VSTPluginMain` hands back, its dispatcher's
//! opcodes, and the host's callback. Only what an effect on a microphone
//! needs: opening, 32-bit processing at Yapfast's rate, the editor, and
//! the plugin's state as a chunk or its parameters.

use std::{
    ffi::{CStr, c_void},
    path::Path,
    ptr::null_mut,
    sync::atomic::{AtomicIsize, Ordering},
};

use anyhow::{Context, Result, bail};
use windows_sys::Win32::Foundation::HWND;

use super::{Library, MAX_BLOCK, MAX_CHANNELS, Scratch};
use crate::RATE;

const MAGIC: i32 = i32::from_be_bytes(*b"VstP");

// The dispatcher's opcodes.
const EFF_OPEN: i32 = 0;
const EFF_CLOSE: i32 = 1;
const EFF_SET_SAMPLE_RATE: i32 = 10;
const EFF_SET_BLOCK_SIZE: i32 = 11;
const EFF_MAINS_CHANGED: i32 = 12;
const EFF_EDIT_GET_RECT: i32 = 13;
const EFF_EDIT_OPEN: i32 = 14;
const EFF_EDIT_CLOSE: i32 = 15;
const EFF_EDIT_IDLE: i32 = 19;
const EFF_GET_CHUNK: i32 = 23;
const EFF_SET_CHUNK: i32 = 24;
const EFF_GET_PLUG_CATEGORY: i32 = 35;
const EFF_GET_EFFECT_NAME: i32 = 45;
const EFF_GET_VENDOR_STRING: i32 = 47;
const EFF_GET_PRODUCT_STRING: i32 = 48;
const EFF_SET_PROCESS_PRECISION: i32 = 77;
const EFF_START_PROCESS: i32 = 71;
const EFF_STOP_PROCESS: i32 = 72;

// The host's opcodes a plugin asks.
const MASTER_AUTOMATE: i32 = 0;
const MASTER_VERSION: i32 = 1;
const MASTER_CURRENT_ID: i32 = 2;
const MASTER_IDLE: i32 = 3;
const MASTER_GET_TIME: i32 = 7;
const MASTER_SIZE_WINDOW: i32 = 15;
const MASTER_GET_SAMPLE_RATE: i32 = 16;
const MASTER_GET_BLOCK_SIZE: i32 = 17;
const MASTER_GET_CURRENT_PROCESS_LEVEL: i32 = 23;
const MASTER_GET_VENDOR_STRING: i32 = 32;
const MASTER_GET_PRODUCT_STRING: i32 = 33;
const MASTER_GET_VENDOR_VERSION: i32 = 34;
const MASTER_CAN_DO: i32 = 37;
const MASTER_GET_LANGUAGE: i32 = 38;
const MASTER_UPDATE_DISPLAY: i32 = 42;
const MASTER_BEGIN_EDIT: i32 = 43;
const MASTER_END_EDIT: i32 = 44;

const FLAG_HAS_EDITOR: i32 = 1;
const FLAG_CAN_REPLACING: i32 = 1 << 4;
const FLAG_PROGRAM_CHUNKS: i32 = 1 << 5;
const FLAG_IS_SYNTH: i32 = 1 << 8;
/// `effGetPlugCategory`'s answer for a shell of several plugins.
const CATEGORY_SHELL: isize = 10;

type HostCallback = unsafe extern "C" fn(*mut AEffect, i32, i32, isize, *mut c_void, f32) -> isize;
type Dispatcher = unsafe extern "C" fn(*mut AEffect, i32, i32, isize, *mut c_void, f32) -> isize;
type ProcessProc = unsafe extern "C" fn(*mut AEffect, *mut *mut f32, *mut *mut f32, i32);
type SetParameter = unsafe extern "C" fn(*mut AEffect, i32, f32);
type GetParameter = unsafe extern "C" fn(*mut AEffect, i32) -> f32;
type Main = unsafe extern "C" fn(HostCallback) -> *mut AEffect;

/// A plugin's side of the interface, as VST 2.4 lays it out.
#[repr(C)]
struct AEffect {
    magic: i32,
    dispatcher: Option<Dispatcher>,
    process: Option<ProcessProc>,
    set_parameter: Option<SetParameter>,
    get_parameter: Option<GetParameter>,
    num_programs: i32,
    num_params: i32,
    num_inputs: i32,
    num_outputs: i32,
    flags: i32,
    reserved1: isize,
    reserved2: isize,
    initial_delay: i32,
    real_qualities: i32,
    off_qualities: i32,
    io_ratio: f32,
    object: *mut c_void,
    user: *mut c_void,
    unique_id: i32,
    version: i32,
    process_replacing: Option<ProcessProc>,
    process_double_replacing: Option<ProcessProc>,
    future: [u8; 56],
}

/// The editor's size as a plugin gives it.
#[repr(C)]
struct ERect {
    top: i16,
    left: i16,
    bottom: i16,
    right: i16,
}

/// The time and transport a plugin may ask for: Yapfast plays at 120 BPM in
/// 4/4 from when the plugin loaded.
#[repr(C)]
struct TimeInfo {
    sample_pos: f64,
    sample_rate: f64,
    nano_seconds: f64,
    ppq_pos: f64,
    tempo: f64,
    bar_start_pos: f64,
    cycle_start_pos: f64,
    cycle_end_pos: f64,
    time_sig_numerator: i32,
    time_sig_denominator: i32,
    smpte_offset: i32,
    smpte_frame_rate: i32,
    samples_to_next_clock: i32,
    flags: i32,
}

const TRANSPORT_PLAYING: i32 = 1 << 1;
const PPQ_POS_VALID: i32 = 1 << 9;
const TEMPO_VALID: i32 = 1 << 10;
const TIME_SIG_VALID: i32 = 1 << 13;
const TEMPO: f64 = 120.0;

/// What the host keeps for each plugin, which its `user` points to: the
/// window its editor is in, which a plugin resizing it asks for, and the
/// time it's told.
struct Hosted {
    window: AtomicIsize,
    time: std::cell::UnsafeCell<TimeInfo>,
}

/// A loaded VST2 plugin.
pub struct Plugin {
    effect: *mut AEffect,
    hosted: Box<Hosted>,
    pub name: String,
    pub vendor: String,
    pub editor: bool,
    inputs: usize,
    outputs: usize,
    /// Frames processed, for the time it's told.
    frames: std::sync::atomic::AtomicU64,
    /// Freed last, once the plugin closed.
    library: Library,
}

// SAFETY: a plugin's audio calls come from the microphone's thread one at a
// time (the chain's lock), and the rest from the plugins' own thread, as
// VST 2.4 hosts call them; the pointers stay valid until it's dropped.
unsafe impl Send for Plugin {}
// SAFETY: as above.
unsafe impl Sync for Plugin {}

impl Plugin {
    /// Loads and opens the plugin at `path`, at Yapfast's rate, resumed.
    pub fn load(path: &Path) -> Result<Self> {
        let mut plugin = Self::open(Library::load(path)?)?;
        plugin.resume();
        Ok(plugin)
    }

    fn open(library: Library) -> Result<Self> {
        let main = library
            .symbol(c"VSTPluginMain")
            .or_else(|| library.symbol(c"main"))
            .context("it isn't a VST2 plugin")?;
        // SAFETY: the plugin's entry point, of the signature VST 2.4 gives
        // it.
        let main: Main = unsafe { std::mem::transmute(main) };
        let hosted = Box::new(Hosted {
            window: AtomicIsize::new(0),
            time: std::cell::UnsafeCell::new(TimeInfo {
                sample_pos: 0.0,
                sample_rate: f64::from(RATE),
                nano_seconds: 0.0,
                ppq_pos: 0.0,
                tempo: TEMPO,
                bar_start_pos: 0.0,
                cycle_start_pos: 0.0,
                cycle_end_pos: 0.0,
                time_sig_numerator: 4,
                time_sig_denominator: 4,
                smpte_offset: 0,
                smpte_frame_rate: 0,
                samples_to_next_clock: 0,
                flags: TRANSPORT_PLAYING | PPQ_POS_VALID | TEMPO_VALID | TIME_SIG_VALID,
            }),
        });
        // SAFETY: the plugin's entry point, given the host's callback.
        let effect = unsafe { main(host_callback) };
        // SAFETY: checked for null before it's read.
        if effect.is_null() || unsafe { (*effect).magic } != MAGIC {
            bail!("it isn't a VST2 plugin");
        }
        // SAFETY: a plugin's AEffect, valid until effClose; `user` is the
        // host's to set.
        unsafe { (*effect).user = (&*hosted as *const Hosted).cast_mut().cast() };
        let mut plugin = Self {
            effect,
            hosted,
            name: String::new(),
            vendor: String::new(),
            editor: false,
            inputs: 0,
            outputs: 0,
            frames: std::sync::atomic::AtomicU64::new(0),
            library,
        };
        plugin.dispatch(EFF_OPEN, 0, 0, null_mut(), 0.0);
        // SAFETY: as above.
        let effect = unsafe { &*plugin.effect };
        if plugin.dispatch(EFF_GET_PLUG_CATEGORY, 0, 0, null_mut(), 0.0) == CATEGORY_SHELL {
            plugin.close();
            bail!("it's a shell of several plugins, which Yapfast doesn't open");
        }
        // An instrument that takes sound in, as FL Studio's own plugin
        // does into its mixer, runs as an effect does.
        if effect.num_inputs < 1 || effect.num_outputs < 1 {
            plugin.close();
            if effect.flags & FLAG_IS_SYNTH != 0 {
                bail!("it's an instrument, which takes no sound in");
            }
            bail!("it takes no sound in, as an effect would");
        }
        if effect.flags & FLAG_CAN_REPLACING == 0 || effect.process_replacing.is_none() {
            plugin.close();
            bail!("it's too old a plugin to process in place");
        }
        if effect.num_inputs as usize > MAX_CHANNELS || effect.num_outputs as usize > MAX_CHANNELS {
            plugin.close();
            bail!("it has more than {MAX_CHANNELS} channels");
        }
        plugin.inputs = effect.num_inputs as usize;
        plugin.outputs = effect.num_outputs as usize;
        plugin.editor = effect.flags & FLAG_HAS_EDITOR != 0;
        plugin.name = plugin
            .string(EFF_GET_EFFECT_NAME)
            .or_else(|| plugin.string(EFF_GET_PRODUCT_STRING))
            .unwrap_or_default();
        plugin.vendor = plugin.string(EFF_GET_VENDOR_STRING).unwrap_or_default();
        plugin.dispatch(EFF_SET_SAMPLE_RATE, 0, 0, null_mut(), RATE as f32);
        plugin.dispatch(EFF_SET_BLOCK_SIZE, 0, MAX_BLOCK as isize, null_mut(), 0.0);
        // 32-bit floats.
        plugin.dispatch(EFF_SET_PROCESS_PRECISION, 0, 0, null_mut(), 0.0);
        Ok(plugin)
    }

    fn dispatch(&self, opcode: i32, index: i32, value: isize, ptr: *mut c_void, opt: f32) -> isize {
        // SAFETY: the plugin's own dispatcher, on its AEffect.
        unsafe {
            match (*self.effect).dispatcher {
                Some(dispatcher) => dispatcher(self.effect, opcode, index, value, ptr, opt),
                None => 0,
            }
        }
    }

    /// A string the plugin writes for `opcode`, up to 255 bytes.
    fn string(&self, opcode: i32) -> Option<String> {
        let mut buffer = [0u8; 256];
        self.dispatch(opcode, 0, 0, buffer.as_mut_ptr().cast(), 0.0);
        let text = CStr::from_bytes_until_nul(&buffer).ok()?.to_string_lossy();
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_owned())
    }

    fn resume(&mut self) {
        self.dispatch(EFF_MAINS_CHANGED, 0, 1, null_mut(), 0.0);
        self.dispatch(EFF_START_PROCESS, 0, 0, null_mut(), 0.0);
    }

    fn suspend(&mut self) {
        self.dispatch(EFF_STOP_PROCESS, 0, 0, null_mut(), 0.0);
        self.dispatch(EFF_MAINS_CHANGED, 0, 0, null_mut(), 0.0);
    }

    fn close(&mut self) {
        if !self.effect.is_null() {
            self.dispatch(EFF_CLOSE, 0, 0, null_mut(), 0.0);
            self.effect = null_mut();
        }
    }

    /// Runs `frames` of stereo `input` into `output`, through `scratch`
    /// where the plugin takes more channels: a mono plugin hears the two
    /// channels' mix, and its one channel goes out on both.
    pub fn process(&self, input: [&[f32]; 2], output: [&mut [f32]; 2], scratch: &mut Scratch) {
        let frames = input[0].len().min(MAX_BLOCK);
        let (ins, outs) = (
            self.inputs.min(scratch.ins.len()),
            self.outputs.min(scratch.outs.len()),
        );
        let mut in_ptrs = [null_mut::<f32>(); MAX_CHANNELS];
        let mut out_ptrs = [null_mut::<f32>(); MAX_CHANNELS];
        for (channel, buffer) in scratch.ins.iter_mut().enumerate().take(ins) {
            match (ins, channel) {
                (1, 0) => {
                    for (at, mixed) in buffer[..frames].iter_mut().enumerate() {
                        *mixed = (input[0][at] + input[1][at]) * 0.5;
                    }
                }
                (_, 0 | 1) => buffer[..frames].copy_from_slice(&input[channel][..frames]),
                _ => buffer[..frames].fill(0.0),
            }
            in_ptrs[channel] = buffer.as_mut_ptr();
        }
        for (channel, buffer) in scratch.outs.iter_mut().enumerate().take(outs) {
            buffer[..frames].fill(0.0);
            out_ptrs[channel] = buffer.as_mut_ptr();
        }
        let position = self.frames.fetch_add(frames as u64, Ordering::Relaxed);
        // SAFETY: only the processing thread writes the time, between the
        // plugin's process calls, which read it.
        unsafe {
            let time = &mut *self.hosted.time.get();
            time.sample_pos = position as f64;
            time.ppq_pos = position as f64 / f64::from(RATE) * TEMPO / 60.0;
        }
        // SAFETY: the plugin's processReplacing, with as many buffers of at
        // least `frames` as it has inputs and outputs.
        unsafe {
            if let Some(process) = (*self.effect).process_replacing {
                process(
                    self.effect,
                    in_ptrs.as_mut_ptr(),
                    out_ptrs.as_mut_ptr(),
                    frames as i32,
                );
            }
        }
        let [left, right] = output;
        left[..frames].copy_from_slice(&scratch.outs[0][..frames]);
        let second = if outs > 1 { 1 } else { 0 };
        right[..frames].copy_from_slice(&scratch.outs[second][..frames]);
    }

    /// The plugin's state: its chunk when it keeps one, else its parameters'
    /// values, 4 bytes each.
    pub fn state(&self) -> (bool, Vec<u8>) {
        // SAFETY: the plugin's AEffect, read on the plugins' thread.
        let effect = unsafe { &*self.effect };
        if effect.flags & FLAG_PROGRAM_CHUNKS != 0 {
            let mut data: *mut c_void = null_mut();
            let size = self.dispatch(EFF_GET_CHUNK, 0, 0, (&raw mut data).cast(), 0.0);
            if size > 0 && !data.is_null() {
                // SAFETY: the plugin's chunk of `size` bytes, valid until
                // its next call.
                let chunk = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size as usize) };
                return (true, chunk.to_vec());
            }
        }
        let mut values = Vec::with_capacity(effect.num_params.max(0) as usize * 4);
        if let Some(get) = effect.get_parameter {
            for index in 0..effect.num_params.max(0) {
                // SAFETY: a parameter's index within the plugin's count.
                values.extend_from_slice(&unsafe { get(self.effect, index) }.to_le_bytes());
            }
        }
        (false, values)
    }

    /// Puts back a state [`Self::state`] gave.
    pub fn restore(&self, chunk: bool, data: &[u8]) {
        // SAFETY: as above.
        let effect = unsafe { &*self.effect };
        if chunk {
            let mut copy = data.to_vec();
            self.dispatch(
                EFF_SET_CHUNK,
                0,
                copy.len() as isize,
                copy.as_mut_ptr().cast(),
                0.0,
            );
            return;
        }
        if let Some(set) = effect.set_parameter {
            for (index, value) in data.as_chunks::<4>().0.iter().enumerate() {
                if index as i32 >= effect.num_params {
                    break;
                }
                // SAFETY: a parameter's index within the plugin's count.
                unsafe { set(self.effect, index as i32, f32::from_le_bytes(*value)) };
            }
        }
    }

    /// The editor's size, once it's open or before.
    pub fn editor_size(&self) -> Option<(i32, i32)> {
        let mut rect: *mut ERect = null_mut();
        self.dispatch(EFF_EDIT_GET_RECT, 0, 0, (&raw mut rect).cast(), 0.0);
        if rect.is_null() {
            return None;
        }
        // SAFETY: the plugin's rectangle, valid until its next call.
        let rect = unsafe { &*rect };
        let (width, height) = (
            i32::from(rect.right) - i32::from(rect.left),
            i32::from(rect.bottom) - i32::from(rect.top),
        );
        (width > 0 && height > 0).then_some((width, height))
    }

    /// Opens the editor in `window`.
    pub fn open_editor(&self, window: HWND) -> bool {
        self.hosted.window.store(window as isize, Ordering::Relaxed);
        self.dispatch(EFF_EDIT_OPEN, 0, 0, window, 0.0) != 0 || self.editor_size().is_some()
    }

    pub fn idle_editor(&self) {
        self.dispatch(EFF_EDIT_IDLE, 0, 0, null_mut(), 0.0);
    }

    pub fn close_editor(&self) {
        self.dispatch(EFF_EDIT_CLOSE, 0, 0, null_mut(), 0.0);
        self.hosted.window.store(0, Ordering::Relaxed);
    }
}

impl Drop for Plugin {
    /// Suspends and closes it; its library goes after, with its field.
    fn drop(&mut self) {
        if !self.effect.is_null() {
            self.suspend();
            self.close();
        }
    }
}

/// What a plugin asks of Yapfast.
unsafe extern "C" fn host_callback(
    effect: *mut AEffect,
    opcode: i32,
    index: i32,
    value: isize,
    ptr: *mut c_void,
    _opt: f32,
) -> isize {
    // SAFETY: `user` is null or the `Hosted` this host set, which outlives
    // the plugin.
    let hosted = unsafe {
        (!effect.is_null())
            .then(|| (*effect).user.cast::<Hosted>())
            .filter(|hosted| !hosted.is_null())
            .map(|hosted| &*hosted)
    };
    match opcode {
        MASTER_VERSION => 2400,
        MASTER_CURRENT_ID => 0,
        MASTER_IDLE
        | MASTER_UPDATE_DISPLAY
        | MASTER_AUTOMATE
        | MASTER_BEGIN_EDIT
        | MASTER_END_EDIT => 1,
        MASTER_GET_TIME => hosted.map_or(0, |hosted| hosted.time.get() as isize),
        MASTER_GET_SAMPLE_RATE => RATE as isize,
        MASTER_GET_BLOCK_SIZE => MAX_BLOCK as isize,
        // Unknown: plugins then take the safe path.
        MASTER_GET_CURRENT_PROCESS_LEVEL => 0,
        MASTER_GET_VENDOR_STRING | MASTER_GET_PRODUCT_STRING => {
            if !ptr.is_null() {
                let name = b"Yapfast\0";
                // SAFETY: the plugin's buffer of at least 64 bytes, as the
                // interface sizes it.
                unsafe {
                    std::ptr::copy_nonoverlapping(name.as_ptr(), ptr.cast::<u8>(), name.len())
                };
            }
            1
        }
        MASTER_GET_VENDOR_VERSION => 1,
        MASTER_GET_LANGUAGE => 1,
        MASTER_CAN_DO => {
            if ptr.is_null() {
                return 0;
            }
            // SAFETY: the plugin's null-terminated question.
            let asked = unsafe { CStr::from_ptr(ptr.cast()) }.to_bytes();
            isize::from(matches!(
                asked,
                b"sizeWindow" | b"sendVstTimeInfo" | b"startStopProcess" | b"supplyIdle"
            ))
        }
        MASTER_SIZE_WINDOW => {
            let Some(window) = hosted
                .map(|hosted| hosted.window.load(Ordering::Relaxed))
                .filter(|&window| window != 0)
            else {
                return 0;
            };
            super::window::resize(window as HWND, index, value as i32);
            1
        }
        _ => 0,
    }
}
