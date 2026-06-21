//! Audio output: decoded frames resample to f32 stereo PCM and play through
//! a rodio sink on its own thread. The sink position is the master clock.

use std::num::{NonZeroU16, NonZeroU32};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use ffmpeg_next as ffmpeg;

use super::{AUDIO_RATE, STALL};

pub(crate) enum AudioCmd {
    Volume(f32),
    Pause,
    Play,
}

/// Resample one decoded audio frame to interleaved f32 stereo and push
/// each sample (blocking on backpressure).
pub(crate) fn send_audio_frame(
    resampler: &mut Option<ffmpeg::software::resampling::Context>,
    decoder: &ffmpeg::decoder::Audio,
    frame: &ffmpeg::frame::Audio,
    pcm_tx: &mpsc::SyncSender<f32>,
) -> Result<(), ()> {
    if resampler.is_none() {
        *resampler = ffmpeg::software::resampling::Context::get(
            decoder.format(),
            decoder.channel_layout(),
            decoder.rate(),
            ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
            ffmpeg::ChannelLayout::STEREO,
            AUDIO_RATE,
        )
        .ok();
    }
    let Some(resampler) = resampler.as_mut() else {
        return Ok(());
    };

    let mut resampled = ffmpeg::frame::Audio::empty();
    if resampler.run(frame, &mut resampled).is_err() {
        return Ok(());
    }

    // Packed f32 stereo: plane 0 holds interleaved bytes.
    let sample_bytes = resampled.samples() * 2 * size_of::<f32>();
    let data = &resampled.data(0)[..sample_bytes];
    for chunk in data.chunks_exact(4) {
        let sample = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if pcm_tx.send(sample).is_err() {
            return Err(());
        }
    }
    Ok(())
}

/// PCM samples streamed from the decode thread into rodio. `recv()`
/// blocks rodio's mixer until samples arrive, exactly like a slow file
/// read would. `None` on disconnect ends the source.
struct PcmChannel {
    rx: mpsc::Receiver<f32>,
}

impl Iterator for PcmChannel {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        self.rx.recv().ok()
    }
}

impl rodio::Source for PcmChannel {
    fn current_span_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> NonZeroU16 {
        NonZeroU16::new(2).expect("stereo channel count is non-zero")
    }
    fn sample_rate(&self) -> NonZeroU32 {
        NonZeroU32::new(AUDIO_RATE).expect("audio rate is non-zero")
    }
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

/// Play decoded PCM through rodio, publishing the player position as the
/// master clock. The device sink lives on its own thread, so commands
/// arrive over a channel.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_audio_output(
    pcm_rx: mpsc::Receiver<f32>,
    stop: Arc<AtomicBool>,
    clock_us: Arc<AtomicU64>,
    has_audio: Arc<AtomicBool>,
    video_ready: Arc<AtomicBool>,
    ui_tick_ms: Arc<AtomicU64>,
    clock_origin: Instant,
    volume: f32,
) -> Option<mpsc::Sender<AudioCmd>> {
    let (tx, rx) = mpsc::channel::<AudioCmd>();

    std::thread::spawn(move || {
        let Ok(mut device_sink) = rodio::DeviceSinkBuilder::open_default_sink() else {
            // No audio device: drain the channel so the decoder never
            // blocks, and let the wall clock pace playback.
            while !stop.load(Ordering::Relaxed) {
                while pcm_rx.try_recv().is_ok() {}
                std::thread::sleep(Duration::from_millis(50));
            }
            return;
        };
        device_sink.log_on_drop(false);
        let player = rodio::Player::connect_new(device_sink.mixer());
        player.set_volume(volume);
        player.append(PcmChannel { rx: pcm_rx });
        // Hold audio until the first video frame is ready, so they start
        // together.
        player.pause();
        let mut waiting_for_video = true;
        let mut playing = true;
        let mut auto_paused = false;
        let mut ended_clock: Option<(Duration, Option<Instant>)> = None;

        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    AudioCmd::Volume(v) => player.set_volume(v),
                    AudioCmd::Pause => {
                        if let Some((position, Some(started))) = ended_clock {
                            ended_clock = Some((position + started.elapsed(), None));
                        }
                        playing = false;
                        player.pause();
                    }
                    AudioCmd::Play => {
                        if let Some((position, None)) = ended_clock {
                            ended_clock = Some((position, Some(Instant::now())));
                        }
                        playing = true;
                        if !waiting_for_video {
                            player.play();
                        }
                    }
                }
            }
            // Release the held audio once the first video frame has shown.
            if waiting_for_video && video_ready.load(Ordering::Relaxed) {
                waiting_for_video = false;
                if playing {
                    player.play();
                }
            }
            // Watchdog: a window move or resize modal loop suspends the UI
            // thread, so pause to freeze audio and the clock with the
            // picture instead of racing ahead on release.
            if has_audio.load(Ordering::Relaxed) && playing {
                let now_ms = clock_origin.elapsed().as_millis() as u64;
                let last_ms = ui_tick_ms.load(Ordering::Relaxed);
                let stalled =
                    last_ms != 0 && now_ms.saturating_sub(last_ms) > STALL.as_millis() as u64;
                if stalled && !auto_paused {
                    if let Some((position, Some(started))) = ended_clock {
                        ended_clock = Some((position + started.elapsed(), None));
                    }
                    player.pause();
                    auto_paused = true;
                } else if !stalled && auto_paused {
                    if let Some((position, None)) = ended_clock {
                        ended_clock = Some((position, Some(Instant::now())));
                    }
                    player.play();
                    auto_paused = false;
                }
            }
            // Silent files feed no samples: the player stays at zero and
            // the session falls back to its wall clock.
            let pos = player.get_pos();
            if pos > Duration::ZERO {
                has_audio.store(true, Ordering::Relaxed);
                clock_us.store(pos.as_micros() as u64, Ordering::Relaxed);
            }
            if player.empty() && has_audio.load(Ordering::Relaxed) {
                let (position, started) =
                    ended_clock.get_or_insert_with(|| (pos, playing.then(Instant::now)));
                let position =
                    *position + started.map(|started| started.elapsed()).unwrap_or_default();
                clock_us.store(position.as_micros() as u64, Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });

    Some(tx)
}
