//! Live, controllable audio instances.
//!
//! Tracks add control and event data around the bytestreams offered by [`Input`],
//! where each represents a live audio source inside of the driver's mixer.
//!
//! To prevent locking and stalling of the driver, tracks are controlled from your bot using a
//! [`TrackHandle`]. These handles remotely send commands from your bot's (a)sync
//! context to control playback, register events, and execute synchronous closures.
//!
//! If you want a new track from an [`Input`], i.e., for direct control before
//! playing your source on the driver, use [`create_player`].
//!
//! [`Input`]: ../input/struct.Input.html
//! [`TrackHandle`]: struct.TrackHandle.html
//! [`create_player`]: fn.create_player.html

mod command;
mod error;
mod handle;
mod looping;
mod mode;
mod queue;
mod state;

pub use self::{command::*, error::*, handle::*, looping::*, mode::*, queue::*, state::*};

use crate::{constants::*, driver::tasks::message::*, events::EventStore, input::Input};
use flume::{Receiver, TryRecvError};
use std::time::Duration;
use uuid::Uuid;

/// Control object for audio playback.
///
/// Accessed by both commands and the playback code -- as such, access from user code is
/// almost always guarded via a [`TrackHandle`]. You should expect to receive
/// access to a raw object of this type via [`create_player`], for use in
/// [`Driver::play`] or [`Driver::play_only`].
///
/// # Example
///
/// ```rust,no_run
/// use songbird::{driver::Driver, ffmpeg, tracks::create_player};
///
/// # async {
/// // A Call is also valid here!
/// let mut handler: Driver = Default::default();
/// let source = ffmpeg("../audio/my-favourite-song.mp3")
///     .await
///     .expect("This might fail: handle this error!");
/// let (mut audio, audio_handle) = create_player(source);
///
/// audio.set_volume(0.5);
///
/// handler.play_only(audio);
///
/// // Future access occurs via audio_handle.
/// # };
/// ```
///
/// [`Driver::play_only`]: crate::driver::Driver::play_only
/// [`Driver::play`]: crate::driver::Driver::play
/// [`TrackHandle`]: TrackHandle
/// [`create_player`]: create_player
#[derive(Debug)]
pub struct Track {
    /// Whether or not this sound is currently playing.
    ///
    /// Can be controlled with [`play`] or [`pause`] if chaining is desired.
    ///
    /// [`play`]: Track::play
    /// [`pause`]: Track::pause
    pub(crate) playing: PlayMode,

    /// The desired volume for playback.
    ///
    /// Sensible values fall between `0.0` and `1.0`.
    ///
    /// Can be controlled with [`volume`] if chaining is desired.
    ///
    /// [`volume`]: Track::volume
    pub(crate) volume: f32,

    /// Underlying data access object.
    ///
    /// *Calling code is not expected to use this.*
    pub(crate) source: Input,

    /// The current playback position in the track.
    pub(crate) position: Duration,

    /// The total length of time this track has been active.
    pub(crate) play_time: Duration,

    /// List of events attached to this audio track.
    ///
    /// This may be used to add additional events to a track
    /// before it is sent to the audio context for playing.
    pub events: Option<EventStore>,

    /// Channel from which commands are received.
    ///
    /// Track commands are sent in this manner to ensure that access
    /// occurs in a thread-safe manner, without allowing any external
    /// code to lock access to audio objects and block packet generation.
    pub(crate) commands: Receiver<TrackCommand>,

    /// Handle for safe control of this audio track from other threads.
    ///
    /// Typically, this is used by internal code to supply context information
    /// to event handlers, though more may be cloned from this handle.
    pub handle: TrackHandle,

    /// Count of remaining loops.
    pub loops: LoopState,

    /// Unique identifier for this track.
    pub(crate) uuid: Uuid,
}

impl Track {
    /// Create a new track directly from an input, command source,
    /// and handle.
    ///
    /// In general, you should probably use [`create_player`].
    ///
    /// [`create_player`]: fn.create_player.html
    pub fn new_raw(source: Input, commands: Receiver<TrackCommand>, handle: TrackHandle) -> Self {
        let uuid = handle.uuid();

        Self {
            playing: Default::default(),
            volume: 1.0,
            source,
            position: Default::default(),
            play_time: Default::default(),
            events: Some(EventStore::new_local()),
            commands,
            handle,
            loops: LoopState::Finite(0),
            uuid,
        }
    }

    /// Sets a track to playing if it is paused.
    pub fn play(&mut self) -> &mut Self {
        self.set_playing(PlayMode::Play)
    }

    /// Pauses a track if it is playing.
    pub fn pause(&mut self) -> &mut Self {
        self.set_playing(PlayMode::Pause)
    }

    /// Manually stops a track.
    ///
    /// This will cause the audio track to be removed, with any relevant events triggered.
    /// Stopped/ended tracks cannot be restarted.
    pub fn stop(&mut self) -> &mut Self {
        self.set_playing(PlayMode::Stop)
    }

    pub(crate) fn end(&mut self) -> &mut Self {
        self.set_playing(PlayMode::End)
    }

    #[inline]
    fn set_playing(&mut self, new_state: PlayMode) -> &mut Self {
        self.playing = self.playing.change_to(new_state);

        self
    }

    /// Returns the current play status of this track.
    pub fn playing(&self) -> PlayMode {
        self.playing
    }

    /// Sets [`volume`] in a manner that allows method chaining.
    ///
    /// [`volume`]: Track::volume
    pub fn set_volume(&mut self, volume: f32) -> &mut Self {
        self.volume = volume;

        self
    }

    /// Returns the current volume.
    pub fn volume(&self) -> f32 {
        self.volume
    }

    /// Returns the current playback position.
    pub fn position(&self) -> Duration {
        self.position
    }

    /// Returns the total length of time this track has been active.
    pub fn play_time(&self) -> Duration {
        self.play_time
    }

    /// Set an audio track to loop a set number of times.
    ///
    /// If the underlying [`Input`] does not support seeking,
    /// then all calls will fail with [`TrackError::SeekUnsupported`].
    ///
    /// [`Input`]: crate::input::Input
    /// [`TrackError::SeekUnsupported`]: TrackError::SeekUnsupported
    pub fn set_loops(&mut self, loops: LoopState) -> TrackResult<()> {
        if self.source.is_seekable() {
            self.loops = loops;
            Ok(())
        } else {
            Err(TrackError::SeekUnsupported)
        }
    }

    pub(crate) fn do_loop(&mut self) -> bool {
        match self.loops {
            LoopState::Infinite => true,
            LoopState::Finite(0) => false,
            LoopState::Finite(ref mut n) => {
                *n -= 1;
                true
            },
        }
    }

    /// Steps playback location forward by one frame.
    pub(crate) fn step_frame(&mut self) {
        self.position += TIMESTEP_LENGTH;
        self.play_time += TIMESTEP_LENGTH;
    }

    /// Receives and acts upon any commands forwarded by TrackHandles.
    ///
    /// *Used internally*, this should not be exposed to users.
    pub(crate) fn process_commands(&mut self, index: usize, ic: &Interconnect) {
        // Note: disconnection and an empty channel are both valid,
        // and should allow the audio object to keep running as intended.

        // Note that interconnect failures are not currently errors.
        // In correct operation, the event thread should never panic,
        // but it receiving status updates is secondary do actually
        // doing the work.
        loop {
            match self.commands.try_recv() {
                Ok(cmd) => {
                    use TrackCommand::*;
                    match cmd {
                        Play => {
                            self.play();
                            let _ = ic.events.send(EventMessage::ChangeState(
                                index,
                                TrackStateChange::Mode(self.playing),
                            ));
                        },
                        Pause => {
                            self.pause();
                            let _ = ic.events.send(EventMessage::ChangeState(
                                index,
                                TrackStateChange::Mode(self.playing),
                            ));
                        },
                        Stop => {
                            self.stop();
                            let _ = ic.events.send(EventMessage::ChangeState(
                                index,
                                TrackStateChange::Mode(self.playing),
                            ));
                        },
                        Volume(vol) => {
                            self.set_volume(vol);
                            let _ = ic.events.send(EventMessage::ChangeState(
                                index,
                                TrackStateChange::Volume(self.volume),
                            ));
                        },
                        Seek(time) =>
                            if let Ok(new_time) = self.seek_time(time) {
                                let _ = ic.events.send(EventMessage::ChangeState(
                                    index,
                                    TrackStateChange::Position(new_time),
                                ));
                            },
                        AddEvent(evt) => {
                            let _ = ic.events.send(EventMessage::AddTrackEvent(index, evt));
                        },
                        Do(action) => {
                            action(self);
                            let _ = ic.events.send(EventMessage::ChangeState(
                                index,
                                TrackStateChange::Total(self.state()),
                            ));
                        },
                        Request(tx) => {
                            let _ = tx.send(self.state());
                        },
                        Loop(loops) =>
                            if self.set_loops(loops).is_ok() {
                                let _ = ic.events.send(EventMessage::ChangeState(
                                    index,
                                    TrackStateChange::Loops(self.loops, true),
                                ));
                            },
                        MakePlayable => self.make_playable(),
                    }
                },
                Err(TryRecvError::Disconnected) => {
                    // this branch will never be visited.
                    break;
                },
                Err(TryRecvError::Empty) => {
                    break;
                },
            }
        }
    }

    /// Ready a track for playing if it is lazily initialised.
    ///
    /// Currently, only [`Restartable`] sources support lazy setup.
    /// This call is a no-op for all others.
    ///
    /// [`Restartable`]: crate::input::restartable::Restartable
    pub fn make_playable(&mut self) {
        self.source.reader.make_playable();
    }

    /// Creates a read-only copy of the audio track's state.
    ///
    /// The primary use-case of this is sending information across
    /// threads in response to a [`TrackHandle`].
    ///
    /// [`TrackHandle`]: TrackHandle
    pub fn state(&self) -> TrackState {
        TrackState {
            playing: self.playing,
            volume: self.volume,
            position: self.position,
            play_time: self.play_time,
            loops: self.loops,
        }
    }

    /// Seek to a specific point in the track.
    ///
    /// If the underlying [`Input`] does not support seeking,
    /// then all calls will fail with [`TrackError::SeekUnsupported`].
    ///
    /// [`Input`]: crate::input::Input
    /// [`TrackError::SeekUnsupported`]: TrackError::SeekUnsupported
    pub fn seek_time(&mut self, pos: Duration) -> TrackResult<Duration> {
        if let Some(t) = self.source.seek_time(pos) {
            self.position = t;
            Ok(t)
        } else {
            Err(TrackError::SeekUnsupported)
        }
    }

    /// Returns this track's unique identifier.
    pub fn uuid(&self) -> Uuid {
        self.uuid
    }
}

/// Creates a [`Track`] object to pass into the audio context, and a [`TrackHandle`]
/// for safe, lock-free access in external code.
///
/// Typically, this would be used if you wished to directly work on or configure
/// the [`Track`] object before it is passed over to the driver.
///
/// [`Track`]: Track
/// [`TrackHandle`]: TrackHandle
#[inline]
pub fn create_player(source: Input) -> (Track, TrackHandle) {
    create_player_with_uuid(source, Uuid::new_v4())
}

/// Creates a [`Track`] and [`TrackHandle`] as in [`create_player`], allowing
/// a custom UUID to be set.
///
/// [`create_player`]: create_player
/// [`Track`]: Track
/// [`TrackHandle`]: TrackHandle
pub fn create_player_with_uuid(source: Input, uuid: Uuid) -> (Track, TrackHandle) {
    let (tx, rx) = flume::unbounded();
    let can_seek = source.is_seekable();
    let metadata = source.metadata.clone();
    let handle = TrackHandle::new(tx, can_seek, uuid, metadata);

    let player = Track::new_raw(source, rx, handle.clone());

    (player, handle)
}
