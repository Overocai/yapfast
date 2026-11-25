//! VST 3 plugins through Steinberg's interfaces, as the `vst3` crate binds
//! them: a module's factory, an audio effect's component, processor and
//! controller, the objects of Yapfast's they call back on (its name and the
//! messages it makes, a stream for their state, the controller's edits for
//! the processor), and the editor's view in a window of Yapfast's.

use std::{
    cell::{Cell, RefCell, UnsafeCell},
    collections::HashMap,
    ffi::{CStr, CString, c_void},
    path::Path,
    ptr::null_mut,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, bail};
use vst3::{
    Class, ComPtr, ComRef, ComWrapper, Interface,
    Steinberg::{
        FIDString, FUnknown, IBStream,
        IBStream_::IStreamSeekMode_,
        IBStreamTrait, IPlugFrame, IPlugFrameTrait, IPlugView, IPlugViewTrait, IPluginBaseTrait,
        IPluginFactory, IPluginFactory2, IPluginFactory2Trait, IPluginFactory3,
        IPluginFactory3Trait, IPluginFactoryTrait, PClassInfo, PClassInfo2, PFactoryInfo, TUID,
        ViewRect,
        Vst::{
            AudioBusBuffers, AudioBusBuffers__type0, BusDirections_, IAttributeList,
            IAttributeList_::AttrID, IAttributeListTrait, IAudioProcessor, IAudioProcessorTrait,
            IComponent, IComponentHandler, IComponentHandlerTrait, IComponentTrait,
            IConnectionPoint, IConnectionPointTrait, IEditController, IEditControllerTrait,
            IHostApplication, IHostApplicationTrait, IMessage, IMessageTrait, IParamValueQueue,
            IParamValueQueueTrait, IParameterChanges, IParameterChangesTrait, MediaTypes_, ParamID,
            ParamValue, ProcessContext, ProcessContext_::StatesAndFlags_, ProcessData,
            ProcessModes_, ProcessSetup, SpeakerArr, SpeakerArrangement, String128,
            SymbolicSampleSizes_, TChar,
        },
        int32, int64, kInvalidArgument, kNoInterface, kPlatformTypeHWND, kResultFalse, kResultOk,
        kResultTrue, tresult, uint32,
    },
};
use windows_sys::Win32::Foundation::HWND;

use super::{Library, MAX_BLOCK, MAX_CHANNELS};
use crate::RATE;

/// The category of an audio processor's class.
const AUDIO_CLASS: &str = "Audio Module Class";
/// How many parameters' changes a block takes from the editor.
const MOST_CHANGES: usize = 64;
const TEMPO: f64 = 120.0;

/// A class's id as the factory gives it, as bytes.
pub type ClassId = [u8; 16];

fn tuid(id: &ClassId) -> TUID {
    id.map(|byte| byte as std::ffi::c_char)
}

fn class_id(id: &TUID) -> ClassId {
    id.map(|byte| byte as u8)
}

/// A fixed text a plugin wrote, up to its null.
fn text(bytes: &[std::ffi::c_char]) -> String {
    let bytes: Vec<u8> = bytes
        .iter()
        .map(|&byte| byte as u8)
        .take_while(|&byte| byte != 0)
        .collect();
    String::from_utf8_lossy(&bytes).trim().to_owned()
}

/// A loaded VST 3 plugin, active and processing.
pub struct Plugin {
    pub name: String,
    pub vendor: String,
    pub class: ClassId,
    pub editor: bool,
    component: ComPtr<IComponent>,
    processor: ComPtr<IAudioProcessor>,
    controller: Option<ComPtr<IEditController>>,
    /// The component's and the controller's connection points, when the
    /// controller is an object of its own.
    connected: Option<(ComPtr<IConnectionPoint>, ComPtr<IConnectionPoint>)>,
    separate: bool,
    host: ComWrapper<HostApp>,
    handler: ComWrapper<Handler>,
    edits: Arc<Edits>,
    changes: ComWrapper<Changes>,
    /// The microphone's thread's: the buffers, buses and time it hands the
    /// processor.
    audio: UnsafeCell<Audio>,
    frames: AtomicU64,
    /// The editor's view while it's open, and its frame.
    view: RefCell<Option<(ComPtr<IPlugView>, ComWrapper<Frame>)>>,
    factory: ComPtr<IPluginFactory>,
    /// Released last, once everything of it is.
    #[allow(dead_code, reason = "held so the library is freed after the plugin")]
    library: Library,
}

// SAFETY: the processor is called from the microphone's thread one block at
// a time (the chain's lock), everything else from the plugins' own thread,
// as VST 3 hosts split them; `audio` and `changes` are the processing
// thread's alone, `view` the plugins' thread's alone.
unsafe impl Send for Plugin {}
// SAFETY: as above.
unsafe impl Sync for Plugin {}

/// What processing hands the plugin, made once.
struct Audio {
    ins: Vec<Vec<f32>>,
    outs: Vec<Vec<f32>>,
    in_pointers: Vec<*mut f32>,
    out_pointers: Vec<*mut f32>,
    in_buses: Vec<AudioBusBuffers>,
    out_buses: Vec<AudioBusBuffers>,
    /// The main buses' channels.
    main_in: usize,
    main_out: usize,
    context: ProcessContext,
}

impl Plugin {
    /// Loads the audio effect in the plugin at `path`, the class `class` if
    /// given (as it was kept), else its first effect, with `state` (its
    /// component's and controller's) put back, active at Yapfast's rate.
    pub fn load(
        path: &Path,
        class: Option<ClassId>,
        state: Option<(&[u8], &[u8])>,
    ) -> Result<Self> {
        let binary = crate::vst3_binary(path).context("its library isn't there")?;
        let mut library = Library::load(&binary)?;
        if let Some(init) = library.symbol(c"InitDll") {
            // SAFETY: VST 3's module entry, `bool InitDll()`, called once
            // after loading, as Steinberg's hosts do.
            let init: unsafe extern "system" fn() -> bool = unsafe { std::mem::transmute(init) };
            // SAFETY: as above.
            if !unsafe { init() } {
                bail!("it wouldn't start");
            }
        }
        library.exit_with(library.symbol(c"ExitDll"));
        let get = library
            .symbol(c"GetPluginFactory")
            .context("it isn't a VST3 plugin")?;
        // SAFETY: VST 3's `IPluginFactory* GetPluginFactory()`.
        let get: unsafe extern "system" fn() -> *mut IPluginFactory =
            unsafe { std::mem::transmute(get) };
        // SAFETY: the factory comes with a reference for the caller.
        let factory = unsafe { ComPtr::from_raw(get()) }.context("it gave no factory")?;
        let host = ComWrapper::new(HostApp);
        let context = host
            .as_com_ref::<IHostApplication>()
            .context("the host's interface")?
            .as_ptr()
            .cast::<FUnknown>();
        if let Some(factory3) = factory.cast::<IPluginFactory3>() {
            // SAFETY: Yapfast's host object, alive for as long as the plugin.
            unsafe { factory3.setHostContext(context) };
        }
        let chosen = choose_class(&factory, class)?;
        let component: ComPtr<IComponent> =
            create(&factory, &chosen.cid).context("it couldn't make its effect")?;
        // SAFETY: Yapfast's host object as its context.
        if unsafe { component.initialize(context) } != kResultOk {
            bail!("its effect wouldn't start");
        }
        let processor = component
            .cast::<IAudioProcessor>()
            .context("it doesn't process sound")?;
        let (controller, separate) = match component.cast::<IEditController>() {
            Some(controller) => (Some(controller), false),
            None => {
                let mut cid: TUID = [0; 16];
                // SAFETY: a TUID for the plugin to write.
                let controller = if unsafe { component.getControllerClassId(&mut cid) } == kResultOk
                {
                    create::<IEditController>(&factory, &class_id(&cid))
                } else {
                    None
                };
                let controller = controller.filter(|controller| {
                    // SAFETY: as for the component.
                    unsafe { controller.initialize(context) == kResultOk }
                });
                (controller, true)
            }
        };
        let connected = match (&controller, separate) {
            (Some(controller), true) => component
                .cast::<IConnectionPoint>()
                .zip(controller.cast::<IConnectionPoint>())
                .inspect(|(a, b)| {
                    // SAFETY: each the other's live connection point.
                    unsafe {
                        a.connect(b.as_ptr());
                        b.connect(a.as_ptr());
                    }
                }),
            _ => None,
        };
        let edits = Arc::new(Edits::default());
        let handler = ComWrapper::new(Handler {
            edits: edits.clone(),
        });
        if let (Some(controller), Some(handler)) =
            (&controller, handler.as_com_ref::<IComponentHandler>())
        {
            // SAFETY: Yapfast's handler, alive for as long as the plugin.
            unsafe { controller.setComponentHandler(handler.as_ptr()) };
        }
        let changes = ComWrapper::new(Changes::new());
        let mut plugin = Self {
            name: chosen.name,
            vendor: chosen.vendor,
            class: chosen.cid,
            editor: controller.is_some(),
            component,
            processor,
            controller,
            connected,
            separate,
            host,
            handler,
            edits,
            changes,
            audio: UnsafeCell::new(Audio {
                ins: Vec::new(),
                outs: Vec::new(),
                in_pointers: Vec::new(),
                out_pointers: Vec::new(),
                in_buses: Vec::new(),
                out_buses: Vec::new(),
                main_in: 0,
                main_out: 0,
                // SAFETY: plain numbers, for which zero is valid.
                context: unsafe { std::mem::zeroed() },
            }),
            frames: AtomicU64::new(0),
            view: RefCell::new(None),
            factory,
            library,
        };
        match state {
            Some((component, controller)) => plugin.restore(component, controller),
            None => plugin.sync_controller(),
        }
        plugin.set_up()?;
        Ok(plugin)
    }

    /// The controller takes in the component's state, as it starts.
    fn sync_controller(&self) {
        let Some(controller) = &self.controller else {
            return;
        };
        let stream = ComWrapper::new(Stream::default());
        let Some(pointer) = stream
            .as_com_ref::<IBStream>()
            .map(|stream| stream.as_ptr())
        else {
            return;
        };
        // SAFETY: Yapfast's stream, alive across both calls.
        unsafe {
            if self.component.getState(pointer) == kResultOk {
                stream.rewind();
                controller.setComponentState(pointer);
            }
        }
    }

    /// Arranges its buses in stereo where it takes that, sets it up for
    /// Yapfast's rate and blocks, and starts it processing.
    fn set_up(&mut self) -> Result<()> {
        let (kaudio, kinput, koutput) = (
            MediaTypes_::kAudio,
            BusDirections_::kInput,
            BusDirections_::kOutput,
        );
        // SAFETY: the plugin's own counts.
        let (ins, outs) = unsafe {
            (
                self.component.getBusCount(kaudio, kinput),
                self.component.getBusCount(kaudio, koutput),
            )
        };
        if ins < 1 {
            bail!("it's an instrument, which takes no sound in");
        }
        if outs < 1 {
            bail!("it gives no sound out");
        }
        let current = |direction, index| {
            let mut arrangement: SpeakerArrangement = 0;
            // SAFETY: a bus within the plugin's count.
            unsafe {
                self.processor
                    .getBusArrangement(direction, index, &mut arrangement)
            };
            arrangement
        };
        let mut in_arrangements: Vec<SpeakerArrangement> = (0..ins)
            .map(|index| {
                if index == 0 {
                    SpeakerArr::kStereo
                } else {
                    current(kinput, index)
                }
            })
            .collect();
        let mut out_arrangements: Vec<SpeakerArrangement> = (0..outs)
            .map(|index| {
                if index == 0 {
                    SpeakerArr::kStereo
                } else {
                    current(koutput, index)
                }
            })
            .collect();
        // SAFETY: as many arrangements as the plugin has buses; one it
        // refuses leaves its own, read back below.
        unsafe {
            self.processor.setBusArrangements(
                in_arrangements.as_mut_ptr(),
                ins,
                out_arrangements.as_mut_ptr(),
                outs,
            )
        };
        let in_channels: Vec<usize> = (0..ins)
            .map(|index| (current(kinput, index).count_ones() as usize).max(1))
            .collect();
        let out_channels: Vec<usize> = (0..outs)
            .map(|index| (current(koutput, index).count_ones() as usize).max(1))
            .collect();
        if in_channels.iter().sum::<usize>() > MAX_CHANNELS
            || out_channels.iter().sum::<usize>() > MAX_CHANNELS
        {
            bail!("it has more than {MAX_CHANNELS} channels");
        }
        // SAFETY: the main buses, which every effect has.
        unsafe {
            self.component.activateBus(kaudio, kinput, 0, 1);
            self.component.activateBus(kaudio, koutput, 0, 1);
        }
        // SAFETY: a question of the plugin.
        if unsafe {
            self.processor
                .canProcessSampleSize(SymbolicSampleSizes_::kSample32)
        } != kResultTrue
        {
            bail!("it doesn't process 32-bit sound");
        }
        let mut setup = ProcessSetup {
            processMode: ProcessModes_::kRealtime,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32,
            maxSamplesPerBlock: MAX_BLOCK as i32,
            sampleRate: f64::from(RATE),
        };
        // SAFETY: a setup the plugin reads during the call.
        if unsafe { self.processor.setupProcessing(&mut setup) } != kResultOk {
            bail!("it wouldn't set up at 48 kHz");
        }
        let audio = self.audio.get_mut();
        audio.ins = (0..in_channels.iter().sum())
            .map(|_| vec![0.0; MAX_BLOCK])
            .collect();
        audio.outs = (0..out_channels.iter().sum())
            .map(|_| vec![0.0; MAX_BLOCK])
            .collect();
        audio.in_pointers = audio
            .ins
            .iter_mut()
            .map(|buffer| buffer.as_mut_ptr())
            .collect();
        audio.out_pointers = audio
            .outs
            .iter_mut()
            .map(|buffer| buffer.as_mut_ptr())
            .collect();
        let buses = |channels: &[usize], pointers: &mut Vec<*mut f32>| {
            let mut at = 0;
            channels
                .iter()
                .map(|&count| {
                    let bus = AudioBusBuffers {
                        numChannels: count as i32,
                        silenceFlags: 0,
                        __field0: AudioBusBuffers__type0 {
                            // SAFETY: within `pointers`, which outlives the buses.
                            channelBuffers32: unsafe { pointers.as_mut_ptr().add(at) },
                        },
                    };
                    at += count;
                    bus
                })
                .collect::<Vec<_>>()
        };
        audio.in_buses = buses(&in_channels, &mut audio.in_pointers);
        audio.out_buses = buses(&out_channels, &mut audio.out_pointers);
        audio.main_in = in_channels[0];
        audio.main_out = out_channels[0];
        audio.context.sampleRate = f64::from(RATE);
        audio.context.tempo = TEMPO;
        audio.context.timeSigNumerator = 4;
        audio.context.timeSigDenominator = 4;
        audio.context.state = (StatesAndFlags_::kPlaying
            | StatesAndFlags_::kTempoValid
            | StatesAndFlags_::kTimeSigValid
            | StatesAndFlags_::kProjectTimeMusicValid) as u32;
        // SAFETY: set up above.
        unsafe {
            if self.component.setActive(1) != kResultOk {
                bail!("it wouldn't start processing");
            }
            self.processor.setProcessing(1);
        }
        Ok(())
    }

    /// Runs `frames` of stereo `input` into `output`, with the edits made
    /// in its editor since the last block; a mono plugin hears the
    /// channels' mix, and its one channel goes out on both.
    pub fn process(&self, input: [&[f32]; 2], output: [&mut [f32]; 2]) {
        let frames = input[0].len().min(MAX_BLOCK);
        // SAFETY: only the processing thread touches `audio`, one block at a
        // time.
        let audio = unsafe { &mut *self.audio.get() };
        for (channel, buffer) in audio.ins.iter_mut().enumerate() {
            match (audio.main_in, channel) {
                (1, 0) => {
                    for (at, mixed) in buffer[..frames].iter_mut().enumerate() {
                        *mixed = (input[0][at] + input[1][at]) * 0.5;
                    }
                }
                (main, 0 | 1) if channel < main => {
                    buffer[..frames].copy_from_slice(&input[channel][..frames]);
                }
                _ => buffer[..frames].fill(0.0),
            }
        }
        self.run(audio, frames as i32);
        let [left, right] = output;
        left[..frames].copy_from_slice(&audio.outs[0][..frames]);
        let second = usize::from(audio.main_out > 1);
        right[..frames].copy_from_slice(&audio.outs[second][..frames]);
    }

    /// Hands the processor its buffers for `frames` (none, to give it the
    /// editor's edits alone), with those edits and the time.
    fn run(&self, audio: &mut Audio, frames: i32) {
        let changes = &*self.changes;
        changes.clear();
        if let Ok(mut edits) = self.edits.pending.try_lock() {
            for (id, value) in edits.drain(..) {
                changes.add(id, value);
            }
        }
        let changes = if changes.used.get() > 0 {
            self.changes
                .as_com_ref::<IParameterChanges>()
                .map_or(null_mut(), |changes| changes.as_ptr())
        } else {
            null_mut()
        };
        let position = self
            .frames
            .fetch_add(frames.max(0) as u64, Ordering::Relaxed);
        audio.context.projectTimeSamples = position as i64;
        audio.context.continousTimeSamples = position as i64;
        audio.context.projectTimeMusic = position as f64 / f64::from(RATE) * TEMPO / 60.0;
        let mut data = ProcessData {
            processMode: ProcessModes_::kRealtime,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32,
            numSamples: frames,
            numInputs: audio.in_buses.len() as i32,
            numOutputs: audio.out_buses.len() as i32,
            inputs: audio.in_buses.as_mut_ptr(),
            outputs: audio.out_buses.as_mut_ptr(),
            inputParameterChanges: changes,
            outputParameterChanges: null_mut(),
            inputEvents: null_mut(),
            outputEvents: null_mut(),
            processContext: &mut audio.context,
        };
        // SAFETY: buffers of at least `frames` for every channel of every
        // bus, set up for this plugin, and the rest alive for the call.
        unsafe { self.processor.process(&mut data) };
    }

    /// Hands the processor the editor's edits, with no sound, as VST 3 lets
    /// a host flush them; called with the chain held, before its state is
    /// read.
    pub fn flush(&self) {
        // SAFETY: the chain's lock keeps the processing thread off it.
        let audio = unsafe { &mut *self.audio.get() };
        self.run(audio, 0);
    }

    /// Its component's state and its controller's.
    pub fn state(&self) -> (Vec<u8>, Vec<u8>) {
        let read = |get: &dyn Fn(*mut IBStream) -> tresult| {
            let stream = ComWrapper::new(Stream::default());
            let Some(pointer) = stream
                .as_com_ref::<IBStream>()
                .map(|stream| stream.as_ptr())
            else {
                return Vec::new();
            };
            if get(pointer) == kResultOk {
                stream.data.take()
            } else {
                Vec::new()
            }
        };
        // SAFETY: Yapfast's stream, alive for the call.
        let component = read(&|stream| unsafe { self.component.getState(stream) });
        let controller = match &self.controller {
            // SAFETY: as above.
            Some(controller) => read(&|stream| unsafe { controller.getState(stream) }),
            None => Vec::new(),
        };
        (component, controller)
    }

    /// Puts back a state [`Self::state`] gave: the component's, which the
    /// controller takes in too, then the controller's own.
    pub fn restore(&self, component: &[u8], controller: &[u8]) {
        let stream = |data: &[u8]| {
            let stream = ComWrapper::new(Stream::default());
            *stream.data.borrow_mut() = data.to_vec();
            stream
        };
        let given = stream(component);
        let Some(pointer) = given.as_com_ref::<IBStream>().map(|stream| stream.as_ptr()) else {
            return;
        };
        // SAFETY: Yapfast's streams, alive for the calls.
        unsafe {
            self.component.setState(pointer);
            if let Some(edit) = &self.controller {
                given.rewind();
                edit.setComponentState(pointer);
                if !controller.is_empty() {
                    let own = stream(controller);
                    if let Some(pointer) =
                        own.as_com_ref::<IBStream>().map(|stream| stream.as_ptr())
                    {
                        edit.setState(pointer);
                    }
                }
            }
        }
    }

    /// Makes its editor's view, if it has one for a window: its size.
    pub fn open_view(&self) -> Option<(i32, i32)> {
        let controller = self.controller.as_ref()?;
        // SAFETY: the view comes with a reference for the caller.
        let view = unsafe { ComPtr::from_raw(controller.createView(c"editor".as_ptr())) }?;
        // SAFETY: a question of the view.
        if unsafe { view.isPlatformTypeSupported(kPlatformTypeHWND) } != kResultTrue {
            return None;
        }
        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: a rectangle for the view to fill.
        unsafe { view.getSize(&mut rect) };
        let size = (
            (rect.right - rect.left).max(100),
            (rect.bottom - rect.top).max(60),
        );
        let frame = ComWrapper::new(Frame {
            window: Cell::new(null_mut()),
        });
        *self.view.borrow_mut() = Some((view, frame));
        Some(size)
    }

    /// Puts the view [`Self::open_view`] made in `window`.
    pub fn attach_view(&self, window: HWND) -> bool {
        let view = self.view.borrow();
        let Some((view, frame)) = view.as_ref() else {
            return false;
        };
        frame.window.set(window);
        let frame = frame
            .as_com_ref::<IPlugFrame>()
            .map_or(null_mut(), |frame| frame.as_ptr());
        // SAFETY: Yapfast's frame and window, alive while the view is.
        unsafe {
            view.setFrame(frame);
            view.attached(window, kPlatformTypeHWND) == kResultOk
        }
    }

    /// Takes the editor's view out of its window and lets it go.
    pub fn close_view(&self) {
        if let Some((view, _frame)) = self.view.borrow_mut().take() {
            // SAFETY: the view attached to its window.
            unsafe {
                view.removed();
                view.setFrame(null_mut());
            }
        }
    }
}

impl Drop for Plugin {
    /// Stops it, disconnects and terminates its parts; they're released
    /// after, and its library last, with their fields.
    fn drop(&mut self) {
        self.close_view();
        // SAFETY: the plugin's parts, stopped in the order they started.
        unsafe {
            self.processor.setProcessing(0);
            self.component.setActive(0);
            if let Some((a, b)) = &self.connected {
                a.disconnect(b.as_ptr());
                b.disconnect(a.as_ptr());
            }
            if let Some(controller) = &self.controller {
                controller.setComponentHandler(null_mut());
                if self.separate {
                    controller.terminate();
                }
            }
            self.component.terminate();
        }
        let _ = (&self.host, &self.handler, &self.factory);
    }
}

/// The class chosen, with its names.
struct Chosen {
    cid: ClassId,
    name: String,
    vendor: String,
}

/// The audio effect to load from `factory`: `wanted` if it has it, else
/// its first audio class that isn't an instrument, else its first.
fn choose_class(factory: &ComPtr<IPluginFactory>, wanted: Option<ClassId>) -> Result<Chosen> {
    // SAFETY: questions of the factory, with structures for it to fill.
    let (count, mut vendor) = unsafe {
        let mut info: PFactoryInfo = std::mem::zeroed();
        factory.getFactoryInfo(&mut info);
        (factory.countClasses(), text(&info.vendor))
    };
    let factory2 = factory.cast::<IPluginFactory2>();
    let mut audio = Vec::new();
    for index in 0..count {
        // SAFETY: as above.
        let mut info: PClassInfo = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        if unsafe { factory.getClassInfo(index, &mut info) } != kResultOk
            || text(&info.category) != AUDIO_CLASS
        {
            continue;
        }
        let mut sub = String::new();
        let mut class_vendor = String::new();
        if let Some(factory2) = &factory2 {
            // SAFETY: as above.
            let mut info2: PClassInfo2 = unsafe { std::mem::zeroed() };
            // SAFETY: as above.
            if unsafe { factory2.getClassInfo2(index, &mut info2) } == kResultOk {
                sub = text(&info2.subCategories);
                class_vendor = text(&info2.vendor);
            }
        }
        audio.push((class_id(&info.cid), text(&info.name), sub, class_vendor));
    }
    let chosen = wanted
        .and_then(|wanted| audio.iter().find(|(cid, ..)| *cid == wanted))
        .or_else(|| {
            audio
                .iter()
                .find(|(_, _, sub, _)| !sub.split('|').any(|part| part == "Instrument"))
        })
        .or_else(|| audio.first())
        .context("it holds no audio effect")?;
    if !chosen.3.is_empty() {
        vendor.clone_from(&chosen.3);
    }
    Ok(Chosen {
        cid: chosen.0,
        name: chosen.1.clone(),
        vendor,
    })
}

/// Makes the factory's class `cid` as interface `I`.
fn create<I: Interface>(factory: &ComPtr<IPluginFactory>, cid: &ClassId) -> Option<ComPtr<I>> {
    let cid = tuid(cid);
    let iid = I::IID;
    let mut object = null_mut();
    // SAFETY: ids that live through the call; the object comes with a
    // reference for the caller.
    unsafe {
        if factory.createInstance(cid.as_ptr(), iid.as_ptr().cast(), &mut object) != kResultOk {
            return None;
        }
        ComPtr::from_raw(object.cast::<I>())
    }
}

/// A TUID's bytes, as an interface's id compares.
///
/// # Safety
///
/// `id` points to 16 bytes.
unsafe fn guid(id: *const TUID) -> [u8; 16] {
    // SAFETY: as the caller says.
    class_id(unsafe { &*id })
}

/// Yapfast, as plugins ask their host: its name, and the messages and
/// attribute lists they ask it to make.
struct HostApp;

impl Class for HostApp {
    type Interfaces = (IHostApplication,);
}

impl IHostApplicationTrait for HostApp {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        if name.is_null() {
            return kInvalidArgument;
        }
        let written: Vec<u16> = "Yapfast".encode_utf16().chain(Some(0)).collect();
        // SAFETY: the plugin's 128 characters.
        unsafe { (&mut *name)[..written.len()].copy_from_slice(&written) };
        kResultOk
    }

    unsafe fn createInstance(
        &self,
        cid: *mut TUID,
        _iid: *mut TUID,
        object: *mut *mut c_void,
    ) -> tresult {
        if object.is_null() || cid.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: the plugin's TUID and its pointer to fill.
        let (class, object) = unsafe {
            *object = null_mut();
            (guid(cid), &mut *object)
        };
        if class == IMessage::IID {
            let message = ComWrapper::new(Message::new());
            if let Some(pointer) = message.to_com_ptr::<IMessage>() {
                *object = pointer.into_raw().cast();
                return kResultOk;
            }
        } else if class == IAttributeList::IID {
            let list = ComWrapper::new(Attributes::default());
            if let Some(pointer) = list.to_com_ptr::<IAttributeList>() {
                *object = pointer.into_raw().cast();
                return kResultOk;
            }
        }
        kNoInterface
    }
}

/// What the editor changed, for the processor's next block: each
/// parameter's latest value.
#[derive(Default)]
pub struct Edits {
    pending: Mutex<Vec<(ParamID, ParamValue)>>,
}

/// Takes the controller's edits.
struct Handler {
    edits: Arc<Edits>,
}

impl Class for Handler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for Handler {
    unsafe fn beginEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn performEdit(&self, id: ParamID, value: ParamValue) -> tresult {
        let mut pending = self
            .edits
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match pending.iter_mut().find(|(edited, _)| *edited == id) {
            Some(edit) => edit.1 = value,
            None => pending.push((id, value)),
        }
        kResultOk
    }

    unsafe fn endEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn restartComponent(&self, _flags: int32) -> tresult {
        kResultOk
    }
}

/// One parameter's change in a block: one value, at its start.
struct Queue {
    id: Cell<ParamID>,
    value: Cell<ParamValue>,
    points: Cell<i32>,
}

impl Class for Queue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for Queue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id.get()
    }

    unsafe fn getPointCount(&self) -> int32 {
        self.points.get()
    }

    unsafe fn getPoint(&self, index: int32, offset: *mut int32, value: *mut ParamValue) -> tresult {
        if index != 0 || self.points.get() == 0 || offset.is_null() || value.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: the plugin's pointers to fill.
        unsafe {
            *offset = 0;
            *value = self.value.get();
        }
        kResultOk
    }

    unsafe fn addPoint(&self, _offset: int32, value: ParamValue, index: *mut int32) -> tresult {
        self.value.set(value);
        self.points.set(1);
        if !index.is_null() {
            // SAFETY: the plugin's pointer to fill.
            unsafe { *index = 0 };
        }
        kResultOk
    }
}

/// The editor's edits for a block, in queues made once.
struct Changes {
    queues: Vec<ComWrapper<Queue>>,
    used: Cell<usize>,
}

impl Changes {
    fn new() -> Self {
        Self {
            queues: (0..MOST_CHANGES)
                .map(|_| {
                    ComWrapper::new(Queue {
                        id: Cell::new(0),
                        value: Cell::new(0.0),
                        points: Cell::new(0),
                    })
                })
                .collect(),
            used: Cell::new(0),
        }
    }

    fn clear(&self) {
        self.used.set(0);
    }

    fn add(&self, id: ParamID, value: ParamValue) {
        let used = self.used.get();
        if let Some(queue) = self.queues[..used]
            .iter()
            .find(|queue| queue.id.get() == id)
        {
            queue.value.set(value);
            return;
        }
        if let Some(queue) = self.queues.get(used) {
            queue.id.set(id);
            queue.value.set(value);
            queue.points.set(1);
            self.used.set(used + 1);
        }
    }
}

impl Class for Changes {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for Changes {
    unsafe fn getParameterCount(&self) -> int32 {
        self.used.get() as int32
    }

    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        usize::try_from(index)
            .ok()
            .filter(|&index| index < self.used.get())
            .and_then(|index| self.queues[index].as_com_ref::<IParamValueQueue>())
            .map_or(null_mut(), |queue| queue.as_ptr())
    }

    unsafe fn addParameterData(
        &self,
        id: *const ParamID,
        index: *mut int32,
    ) -> *mut IParamValueQueue {
        if id.is_null() {
            return null_mut();
        }
        // SAFETY: the plugin's id.
        let id = unsafe { *id };
        self.add(id, 0.0);
        let at = self.queues[..self.used.get()]
            .iter()
            .position(|queue| queue.id.get() == id);
        let Some(at) = at else {
            return null_mut();
        };
        if !index.is_null() {
            // SAFETY: the plugin's pointer to fill.
            unsafe { *index = at as int32 };
        }
        self.queues[at]
            .as_com_ref::<IParamValueQueue>()
            .map_or(null_mut(), |queue| queue.as_ptr())
    }
}

/// A stream in memory, for a plugin's state.
#[derive(Default)]
struct Stream {
    data: RefCell<Vec<u8>>,
    at: Cell<usize>,
}

impl Stream {
    fn rewind(&self) {
        self.at.set(0);
    }
}

impl Class for Stream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for Stream {
    unsafe fn read(&self, buffer: *mut c_void, bytes: int32, read: *mut int32) -> tresult {
        let data = self.data.borrow();
        let at = self.at.get();
        let count = usize::try_from(bytes)
            .unwrap_or(0)
            .min(data.len().saturating_sub(at));
        if count > 0 && !buffer.is_null() {
            // SAFETY: the plugin's buffer of `bytes`, and ours of `count`
            // from `at`.
            unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr().add(at), buffer.cast::<u8>(), count);
            }
        }
        self.at.set(at + count);
        if !read.is_null() {
            // SAFETY: the plugin's pointer to fill.
            unsafe { *read = count as int32 };
        }
        if count == 0 && bytes > 0 {
            kResultFalse
        } else {
            kResultOk
        }
    }

    unsafe fn write(&self, buffer: *mut c_void, bytes: int32, written: *mut int32) -> tresult {
        let count = usize::try_from(bytes).unwrap_or(0);
        if count > 0 && buffer.is_null() {
            return kInvalidArgument;
        }
        let at = self.at.get();
        let mut data = self.data.borrow_mut();
        if data.len() < at + count {
            data.resize(at + count, 0);
        }
        if count > 0 {
            // SAFETY: the plugin's buffer of `count` bytes, into ours.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    buffer.cast::<u8>(),
                    data.as_mut_ptr().add(at),
                    count,
                );
            }
        }
        self.at.set(at + count);
        if !written.is_null() {
            // SAFETY: the plugin's pointer to fill.
            unsafe { *written = count as int32 };
        }
        kResultOk
    }

    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        let base = match mode {
            IStreamSeekMode_::kIBSeekSet => 0,
            IStreamSeekMode_::kIBSeekCur => self.at.get() as i64,
            IStreamSeekMode_::kIBSeekEnd => self.data.borrow().len() as i64,
            _ => return kInvalidArgument,
        };
        let to = base + pos;
        if to < 0 {
            return kResultFalse;
        }
        self.at.set(to as usize);
        if !result.is_null() {
            // SAFETY: the plugin's pointer to fill.
            unsafe { *result = to };
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if pos.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: the plugin's pointer to fill.
        unsafe { *pos = self.at.get() as i64 };
        kResultOk
    }
}

/// The window an editor's view is in, which the view asks to resize.
struct Frame {
    window: Cell<HWND>,
}

impl Class for Frame {
    type Interfaces = (IPlugFrame,);
}

impl IPlugFrameTrait for Frame {
    unsafe fn resizeView(&self, view: *mut IPlugView, size: *mut ViewRect) -> tresult {
        if size.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: the view's rectangle.
        let rect = unsafe { *size };
        super::window::resize(
            self.window.get(),
            rect.right - rect.left,
            rect.bottom - rect.top,
        );
        // SAFETY: the view asking, live for the call; as VST 3 asks, it's
        // told its new size once its window has it.
        if let Some(view) = unsafe { ComRef::from_raw(view) } {
            // SAFETY: as above.
            unsafe { view.onSize(size) };
        }
        kResultTrue
    }
}

/// A message a plugin asked Yapfast for, between its component and its
/// controller.
struct Message {
    id: RefCell<Option<CString>>,
    attributes: ComWrapper<Attributes>,
}

impl Message {
    fn new() -> Self {
        Self {
            id: RefCell::new(None),
            attributes: ComWrapper::new(Attributes::default()),
        }
    }
}

impl Class for Message {
    type Interfaces = (IMessage,);
}

impl IMessageTrait for Message {
    unsafe fn getMessageID(&self) -> FIDString {
        self.id
            .borrow()
            .as_ref()
            .map_or(c"".as_ptr(), |id| id.as_ptr())
    }

    unsafe fn setMessageID(&self, id: FIDString) {
        // SAFETY: the plugin's null-terminated id.
        *self.id.borrow_mut() = (!id.is_null()).then(|| unsafe { CStr::from_ptr(id) }.to_owned());
    }

    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        self.attributes
            .as_com_ref::<IAttributeList>()
            .map_or(null_mut(), |list| list.as_ptr())
    }
}

/// An attribute's value.
enum Value {
    Int(i64),
    Float(f64),
    Text(Vec<TChar>),
    Bytes(Vec<u8>),
}

/// A message's attributes, by their ids.
#[derive(Default)]
struct Attributes {
    values: RefCell<HashMap<Vec<u8>, Value>>,
}

/// An attribute's id as a key.
///
/// # Safety
///
/// `id` is null or a null-terminated string.
unsafe fn key(id: AttrID) -> Option<Vec<u8>> {
    // SAFETY: as the caller says.
    (!id.is_null()).then(|| unsafe { CStr::from_ptr(id) }.to_bytes().to_vec())
}

impl Class for Attributes {
    type Interfaces = (IAttributeList,);
}

impl IAttributeListTrait for Attributes {
    unsafe fn setInt(&self, id: AttrID, value: int64) -> tresult {
        // SAFETY: the plugin's id.
        let Some(key) = (unsafe { key(id) }) else {
            return kInvalidArgument;
        };
        self.values.borrow_mut().insert(key, Value::Int(value));
        kResultOk
    }

    unsafe fn getInt(&self, id: AttrID, value: *mut int64) -> tresult {
        // SAFETY: the plugin's id.
        let Some(key) = (unsafe { key(id) }) else {
            return kInvalidArgument;
        };
        match self.values.borrow().get(&key) {
            Some(Value::Int(kept)) if !value.is_null() => {
                // SAFETY: the plugin's pointer to fill.
                unsafe { *value = *kept };
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setFloat(&self, id: AttrID, value: f64) -> tresult {
        // SAFETY: the plugin's id.
        let Some(key) = (unsafe { key(id) }) else {
            return kInvalidArgument;
        };
        self.values.borrow_mut().insert(key, Value::Float(value));
        kResultOk
    }

    unsafe fn getFloat(&self, id: AttrID, value: *mut f64) -> tresult {
        // SAFETY: the plugin's id.
        let Some(key) = (unsafe { key(id) }) else {
            return kInvalidArgument;
        };
        match self.values.borrow().get(&key) {
            Some(Value::Float(kept)) if !value.is_null() => {
                // SAFETY: the plugin's pointer to fill.
                unsafe { *value = *kept };
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setString(&self, id: AttrID, string: *const TChar) -> tresult {
        // SAFETY: the plugin's id.
        let Some(key) = (unsafe { key(id) }) else {
            return kInvalidArgument;
        };
        if string.is_null() {
            return kInvalidArgument;
        }
        let mut text = Vec::new();
        // SAFETY: the plugin's null-terminated text.
        unsafe {
            let mut at = 0;
            while *string.add(at) != 0 && at < 1 << 20 {
                text.push(*string.add(at));
                at += 1;
            }
        }
        self.values.borrow_mut().insert(key, Value::Text(text));
        kResultOk
    }

    unsafe fn getString(&self, id: AttrID, string: *mut TChar, bytes: uint32) -> tresult {
        // SAFETY: the plugin's id.
        let Some(key) = (unsafe { key(id) }) else {
            return kInvalidArgument;
        };
        let room = bytes as usize / 2;
        match self.values.borrow().get(&key) {
            Some(Value::Text(kept)) if !string.is_null() && room > 0 => {
                let count = kept.len().min(room - 1);
                // SAFETY: the plugin's buffer of `room` characters.
                unsafe {
                    std::ptr::copy_nonoverlapping(kept.as_ptr(), string, count);
                    *string.add(count) = 0;
                }
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setBinary(&self, id: AttrID, data: *const c_void, bytes: uint32) -> tresult {
        // SAFETY: the plugin's id.
        let Some(key) = (unsafe { key(id) }) else {
            return kInvalidArgument;
        };
        let kept = if data.is_null() || bytes == 0 {
            Vec::new()
        } else {
            // SAFETY: the plugin's `bytes` of data.
            unsafe { std::slice::from_raw_parts(data.cast::<u8>(), bytes as usize) }.to_vec()
        };
        self.values.borrow_mut().insert(key, Value::Bytes(kept));
        kResultOk
    }

    unsafe fn getBinary(
        &self,
        id: AttrID,
        data: *mut *const c_void,
        bytes: *mut uint32,
    ) -> tresult {
        // SAFETY: the plugin's id.
        let Some(key) = (unsafe { key(id) }) else {
            return kInvalidArgument;
        };
        match self.values.borrow().get(&key) {
            Some(Value::Bytes(kept)) if !data.is_null() && !bytes.is_null() => {
                // SAFETY: the plugin's pointers to fill; the bytes stay
                // until the attribute changes, as VST 3 promises.
                unsafe {
                    *data = kept.as_ptr().cast();
                    *bytes = kept.len() as uint32;
                }
                kResultOk
            }
            _ => kResultFalse,
        }
    }
}

/// Copies a class id to text, as `voice-effects.json` keeps it.
pub fn class_hex(id: &ClassId) -> String {
    use std::fmt::Write as _;
    id.iter().fold(String::with_capacity(32), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

/// A class id from [`class_hex`]'s text.
pub fn class_from_hex(text: &str) -> Option<ClassId> {
    let bytes: Option<Vec<u8>> = (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect();
    bytes?.try_into().ok()
}
