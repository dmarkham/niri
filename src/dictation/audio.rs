//! Microphone capture through PipeWire.
//!
//! PipeWire runs its own loop on its own thread here, rather than having its
//! file descriptor folded into the compositor's event loop the way
//! screencasting does. That pattern works for screencasting because niri
//! iterates the PipeWire loop on every rendered frame, so something is always
//! driving it. Dictation has no such driver: when nothing on screen is
//! changing, the loop only gets iterated when some unrelated event happens to
//! wake calloop, and audio piles up until it does. Measured, that meant audio
//! arriving in five-second lumps — which does not lose a word, but delivers a
//! whole utterance at once, so every preview is stale before it is sent and
//! the live transcript can never run ahead of the final.
//!
//! On its own thread the loop is driven continuously, and finished audio
//! reaches the compositor through a calloop channel, which wakes the event
//! loop properly. It also keeps audio work off the event loop entirely.

use std::sync::mpsc;
use std::thread;

use anyhow::Context as _;
use calloop::{LoopHandle, RegistrationToken};
use pipewire::context::ContextRc;
use pipewire::main_loop::MainLoopRc;
use pipewire::properties::properties;
use pipewire::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{self, Pod};
use pipewire::spa::sys::{SPA_PARAM_EnumFormat, SPA_TYPE_OBJECT_Format};
use pipewire::spa::utils::Direction;
use pipewire::stream::{StreamFlags, StreamRc};

use super::chunker::SAMPLE_RATE;
use crate::niri::State;

/// A live microphone capture. Dropping it releases the microphone.
pub struct Capture {
    /// Tells the PipeWire thread to quit.
    stop: pipewire::channel::Sender<()>,
    thread: Option<thread::JoinHandle<()>>,
    token: RegistrationToken,
    event_loop: LoopHandle<'static, State>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        // Ask the loop to quit, then wait: the stream has to be torn down
        // before the microphone is actually released.
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.event_loop.remove(self.token);
    }
}

impl Capture {
    /// Open the default microphone and hand every buffer to `on_audio`.
    ///
    /// The callback receives raw 16 kHz mono s16le, which is exactly what
    /// whisper.cpp wants, so nothing resamples at either end.
    pub fn start(
        event_loop: LoopHandle<'static, State>,
        on_audio: impl Fn(&mut State, &[u8]) + 'static,
    ) -> anyhow::Result<Self> {
        let (to_niri, from_pipewire) = calloop::channel::channel::<Vec<u8>>();
        let (stop, stop_rx) = pipewire::channel::channel::<()>();
        // Reports whether the stream came up, so a missing microphone is an
        // error at the call site rather than silence forever.
        let (ready_tx, ready) = mpsc::channel::<anyhow::Result<()>>();

        let thread = thread::Builder::new()
            .name("dictation-audio".to_owned())
            .spawn(move || {
                if let Err(err) = run(&to_niri, stop_rx, &ready_tx) {
                    let _ = ready_tx.send(Err(err));
                }
            })
            .context("error spawning dictation audio thread")?;

        match ready.recv() {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                let _ = thread.join();
                return Err(err);
            }
            Err(_) => {
                let _ = thread.join();
                anyhow::bail!("dictation audio thread died during setup");
            }
        }

        let token = event_loop
            .insert_source(from_pipewire, move |event, _, state| {
                if let calloop::channel::Event::Msg(audio) = event {
                    on_audio(state, &audio);
                }
            })
            .map_err(|err| anyhow::anyhow!("error inserting audio source: {err}"))?;

        Ok(Self {
            stop,
            thread: Some(thread),
            token,
            event_loop,
        })
    }
}

/// Build the capture stream and run the loop until told to stop. Runs on the
/// PipeWire thread, since none of these objects can be moved between threads
/// once created. Reports on `ready` once the stream is connected.
fn run(
    to_niri: &calloop::channel::Sender<Vec<u8>>,
    stop: pipewire::channel::Receiver<()>,
    ready: &mpsc::Sender<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    let main_loop = MainLoopRc::new(None).context("error creating MainLoop")?;
    let context = ContextRc::new(&main_loop, None).context("error creating Context")?;
    let core = context.connect_rc(None).context("error connecting Core")?;

    let stream = StreamRc::new(
        core.clone(),
        "niri-dictation",
        properties! {
            *pipewire::keys::MEDIA_TYPE => "Audio",
            *pipewire::keys::MEDIA_CATEGORY => "Capture",
            // Communication rather than Music: this asks the session manager
            // for the microphone a voice call would get.
            *pipewire::keys::MEDIA_ROLE => "Communication",
            *pipewire::keys::NODE_NAME => "niri-dictation",
            // 20 ms buffers, matching what the Go tool asked parecord for.
            *pipewire::keys::NODE_LATENCY => "320/16000",
        },
    )
    .context("error creating Stream")?;

    let listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(|_, (), old, new| {
            debug!("dictation capture state {old:?} -> {new:?}");
        })
        .process({
            let to_niri = to_niri.clone();
            move |stream, ()| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let Some(data) = buffer.datas_mut().first_mut() else {
                    return;
                };
                let size = data.chunk().size() as usize;
                let Some(slice) = data.data() else {
                    return;
                };
                let n = size.min(slice.len());
                if n > 0 {
                    // Straight to the compositor: this wakes its event loop,
                    // which the shared-fd arrangement could not be relied on
                    // to do.
                    let _ = to_niri.send(slice[..n].to_vec());
                }
            }
        })
        .register()
        .context("error registering stream listener")?;

    let mut audio_info = AudioInfoRaw::new();
    audio_info.set_format(AudioFormat::S16LE);
    audio_info.set_rate(SAMPLE_RATE);
    audio_info.set_channels(1);

    let values: Vec<u8> = PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pod::Value::Object(pod::Object {
            type_: SPA_TYPE_OBJECT_Format,
            id: SPA_PARAM_EnumFormat,
            properties: audio_info.into(),
        }),
    )
    .context("error serializing audio format")?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).context("error building format pod")?];

    stream
        .connect(
            Direction::Input,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
            &mut params,
        )
        .context("error connecting capture stream")?;

    let attached = stop.attach(main_loop.loop_(), {
        let main_loop = main_loop.clone();
        move |()| main_loop.quit()
    });

    debug!("dictation capture started at {SAMPLE_RATE} Hz mono s16");
    let _ = ready.send(Ok(()));

    main_loop.run();

    // Tear the stream down explicitly, then let the locals drop in reverse
    // order (listener, stream, core, context, loop). Leaking them instead
    // left a zombie capture node in the graph for every dictation session,
    // each one holding the microphone open.
    let _ = stream.disconnect();
    drop(attached);
    drop(listener);
    drop(stream);
    drop(core);
    drop(context);
    debug!("dictation capture stopped");

    Ok(())
}
