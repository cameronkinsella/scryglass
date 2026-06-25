//! The decode pipeline. A demux thread routes compressed packets into
//! per-stream queues consumed by independent video and audio decode
//! threads, so a backed-up video queue can never starve the audio sink.
//! Decoded video frames convert to planar YUV and flow through a small
//! bounded channel to the session.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use ffmpeg_next as ffmpeg;

use super::audio::send_audio_frame;
use super::frame::copy_plane;
use super::hw::{download_frame, is_hw_frame, try_init_hw};
use super::{VideoFrame, YuvFormat, YuvMatrix, YuvRange, init_ffmpeg};

/// Items sent to a decode thread. `Loop` is the in-band boundary marker the
/// demuxer inserts after a seek back to the start, so post-seek packets can
/// never overtake it. `offset` is how far to shift the loop's timestamps so
/// they keep climbing in step with the playback clock.
enum DecodeCmd {
    Packet(ffmpeg::codec::packet::Packet),
    Loop { offset: Duration },
}

/// Spawn the decode pipeline: a demux thread routes compressed packets
/// into per-stream queues consumed by independent video and audio decode
/// threads. The pipelines must not share backpressure. A full video
/// frame queue blocking audio decode would starve the sink, freeze the
/// audio clock, and deadlock the whole player.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_decode_thread(
    path: PathBuf,
    start: Duration,
    stop: Arc<AtomicBool>,
    duration_us: Arc<AtomicU64>,
    frame_us: Arc<AtomicU64>,
    video_done: Arc<AtomicBool>,
    pcm_tx: mpsc::SyncSender<f32>,
    hardware: bool,
    looping: Arc<AtomicBool>,
) -> mpsc::Receiver<VideoFrame> {
    let (tx, rx) = mpsc::sync_channel::<VideoFrame>(4);

    std::thread::spawn(move || {
        // The video decode thread marks `video_done` after its flush. A
        // setup error here must mark it too or the UI waits forever.
        if run_pipeline(
            &path,
            start,
            &stop,
            &duration_us,
            &frame_us,
            &video_done,
            tx,
            pcm_tx,
            hardware,
            &looping,
        )
        .is_err()
        {
            video_done.store(true, Ordering::Relaxed);
        }
    });

    rx
}

/// Compressed packets are small, so generous queues let the demuxer run
/// ahead across normal A/V interleaving without coupling the streams.
const PACKET_QUEUE: usize = 512;

#[allow(clippy::too_many_arguments)]
fn run_pipeline(
    path: &Path,
    start: Duration,
    stop: &Arc<AtomicBool>,
    duration_us: &AtomicU64,
    frame_us: &AtomicU64,
    video_done: &Arc<AtomicBool>,
    tx: mpsc::SyncSender<VideoFrame>,
    pcm_tx: mpsc::SyncSender<f32>,
    hardware: bool,
    looping: &Arc<AtomicBool>,
) -> Result<(), ffmpeg::Error> {
    init_ffmpeg()?;
    let mut input = ffmpeg::format::input(path)?;

    let container_secs = if input.duration() > 0 {
        // `duration()` is in AV_TIME_BASE units (microseconds).
        duration_us.store(input.duration() as u64, Ordering::Relaxed);
        input.duration() as f64 / 1_000_000.0
    } else {
        0.0
    };
    if !start.is_zero() {
        let ts = start.as_micros() as i64;
        input.seek(ts, ..ts)?;
    }
    let base = start.as_secs_f64();

    // --- Video decode thread ---
    let video_stream = input
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or(ffmpeg::Error::StreamNotFound)?;
    let video_index = video_stream.index();
    let video_tb = f64::from(video_stream.time_base());
    let video_period_secs = stream_duration_secs(video_stream.duration(), video_tb);
    // Frame duration for stepping, from the average frame rate.
    let fps = f64::from(video_stream.avg_frame_rate());
    if fps > 0.0 {
        frame_us.store((1_000_000.0 / fps) as u64, Ordering::Relaxed);
    }
    let mut video_context =
        ffmpeg::codec::context::Context::from_parameters(video_stream.parameters())?;
    // Frame threading only helps software decode.
    let hw_active = hardware && try_init_hw(&mut video_context);
    if !hw_active {
        video_context.set_threading(ffmpeg::codec::threading::Config::kind(
            ffmpeg::codec::threading::Type::Frame,
        ));
    }
    let mut video_decoder = video_context.decoder().video()?;

    let (video_pkt_tx, video_pkt_rx) = mpsc::sync_channel::<DecodeCmd>(PACKET_QUEUE);
    let video_stop = stop.clone();
    let video_finished = video_done.clone();
    std::thread::spawn(move || {
        let mut scaler: Option<ffmpeg::software::scaling::Context> = None;
        let mut frame = ffmpeg::frame::Video::empty();
        let mut sw_frame = ffmpeg::frame::Video::empty();
        // The first kept frame is time zero: the seek target, not the keyframe
        // before it. A loop resets `base`/`origin` to re-anchor at the start
        // and bumps `offset` so the new loop's timestamps keep climbing.
        let mut origin: Option<f64> = None;
        let mut base = base;
        let mut offset = 0.0;

        while let Ok(cmd) = video_pkt_rx.recv() {
            if video_stop.load(Ordering::Relaxed) {
                return;
            }
            match cmd {
                DecodeCmd::Packet(packet) => {
                    if video_decoder.send_packet(&packet).is_err() {
                        continue;
                    }
                    if drain_video(
                        &mut video_decoder,
                        &mut scaler,
                        &mut frame,
                        &mut sw_frame,
                        video_tb,
                        base,
                        &mut origin,
                        offset,
                        &video_stop,
                        &tx,
                    )
                    .is_err()
                    {
                        return;
                    }
                }
                DecodeCmd::Loop { offset: next } => {
                    // Emit the finishing loop's buffered tail (old anchor),
                    // then reset the decoder for the post-seek keyframe.
                    let _ = video_decoder.send_eof();
                    let _ = drain_video(
                        &mut video_decoder,
                        &mut scaler,
                        &mut frame,
                        &mut sw_frame,
                        video_tb,
                        base,
                        &mut origin,
                        offset,
                        &video_stop,
                        &tx,
                    );
                    video_decoder.flush();
                    origin = None;
                    base = 0.0;
                    offset = next.as_secs_f64();
                }
            }
        }
        // Demuxer hung up (true end of stream): flush the decoder.
        let _ = video_decoder.send_eof();
        let _ = drain_video(
            &mut video_decoder,
            &mut scaler,
            &mut frame,
            &mut sw_frame,
            video_tb,
            base,
            &mut origin,
            offset,
            &video_stop,
            &tx,
        );
        video_finished.store(true, Ordering::Relaxed);
    });

    // --- Audio decode thread (optional) ---
    let audio_stream = input.streams().best(ffmpeg::media::Type::Audio);
    let audio_index = audio_stream.as_ref().map(|s| s.index());
    let mut audio_period_secs = 0.0;
    let audio_pkt_tx = match &audio_stream {
        Some(stream) => {
            let mut audio_decoder =
                ffmpeg::codec::context::Context::from_parameters(stream.parameters())?
                    .decoder()
                    .audio()?;
            let audio_tb = f64::from(stream.time_base());
            audio_period_secs = stream_duration_secs(stream.duration(), audio_tb);
            let (pkt_tx, pkt_rx) = mpsc::sync_channel::<DecodeCmd>(PACKET_QUEUE);
            let audio_stop = stop.clone();
            std::thread::spawn(move || {
                let mut resampler: Option<ffmpeg::software::resampling::Context> = None;
                let mut frame = ffmpeg::frame::Audio::empty();
                let mut base = base;
                while let Ok(cmd) = pkt_rx.recv() {
                    if audio_stop.load(Ordering::Relaxed) {
                        return;
                    }
                    match cmd {
                        DecodeCmd::Packet(packet) => {
                            if audio_decoder.send_packet(&packet).is_err() {
                                continue;
                            }
                            if drain_audio(
                                &mut audio_decoder,
                                &mut frame,
                                &mut resampler,
                                audio_tb,
                                base,
                                &audio_stop,
                                &pcm_tx,
                            )
                            .is_err()
                            {
                                return;
                            }
                        }
                        DecodeCmd::Loop { .. } => {
                            // Flush the finishing loop's tail samples so the
                            // PCM stream (and the master clock) stays unbroken,
                            // then reset for the seek. Keep the resampler: its
                            // buffer joins the loops without a click.
                            let _ = audio_decoder.send_eof();
                            let _ = drain_audio(
                                &mut audio_decoder,
                                &mut frame,
                                &mut resampler,
                                audio_tb,
                                base,
                                &audio_stop,
                                &pcm_tx,
                            );
                            audio_decoder.flush();
                            base = 0.0;
                        }
                    }
                }
                // pcm_tx drops here, ending the rodio source.
            });
            Some(pkt_tx)
        }
        None => None,
    };

    // The loop period the clock advances by each pass: the audio length when
    // there is audio (the sink position is the clock), else the video length,
    // else the container. Zero means the duration is unknown, so don't loop
    // in-pipeline; the end-of-stream fallback reopens instead.
    let loop_period_secs = [audio_period_secs, video_period_secs, container_secs]
        .into_iter()
        .find(|d| *d > 0.0)
        .unwrap_or(0.0);

    // --- Demux loop: route packets, never decode. At end of stream, if
    // looping with a known period, seek back to the start and keep feeding the
    // same threads, marking the boundary in-band so timestamps stay ordered. ---
    let mut iteration = 0u64;
    loop {
        for (stream, packet) in input.packets() {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            if stream.index() == video_index {
                if video_pkt_tx.send(DecodeCmd::Packet(packet)).is_err() {
                    return Ok(());
                }
            } else if Some(stream.index()) == audio_index
                && let Some(tx) = &audio_pkt_tx
                && tx.send(DecodeCmd::Packet(packet)).is_err()
            {
                return Ok(());
            }
        }
        if !should_loop(looping.load(Ordering::Relaxed), loop_period_secs) {
            break;
        }
        if input.seek(0, ..0).is_err() {
            break;
        }
        iteration += 1;
        let offset = loop_offset(loop_period_secs, iteration, base);
        if video_pkt_tx.send(DecodeCmd::Loop { offset }).is_err() {
            return Ok(());
        }
        if let Some(tx) = &audio_pkt_tx
            && tx.send(DecodeCmd::Loop { offset }).is_err()
        {
            return Ok(());
        }
    }
    // Senders drop here and the decode threads flush and finish (true EOF).
    Ok(())
}

/// A stream's duration in seconds, or 0.0 when it is unknown (0 or NOPTS).
fn stream_duration_secs(ts: i64, time_base: f64) -> f64 {
    if ts > 0 { ts as f64 * time_base } else { 0.0 }
}

/// Whether to loop at end of stream: only with looping on and a known period.
/// An unknown period falls back to the reopen path at the call site.
fn should_loop(looping: bool, period_secs: f64) -> bool {
    looping && period_secs > 0.0
}

/// The timestamp shift for loop pass `iteration` (1-based). Each pass adds one
/// period; a seeked start (`base_secs > 0`) makes the first pass short, so
/// subtract it to keep the looped timestamps in step with the clock.
fn loop_offset(period_secs: f64, iteration: u64, base_secs: f64) -> Duration {
    Duration::from_secs_f64((period_secs * iteration as f64 - base_secs).max(0.0))
}

/// Drain every video frame the decoder has ready to the session, shifting each
/// timestamp by `offset` so looped passes keep climbing with the clock. `Err`
/// means the session is stopping, so the thread should exit.
#[allow(clippy::too_many_arguments)]
fn drain_video(
    decoder: &mut ffmpeg::decoder::Video,
    scaler: &mut Option<ffmpeg::software::scaling::Context>,
    frame: &mut ffmpeg::frame::Video,
    sw_frame: &mut ffmpeg::frame::Video,
    video_tb: f64,
    base: f64,
    origin: &mut Option<f64>,
    offset: f64,
    stop: &AtomicBool,
    tx: &mpsc::SyncSender<VideoFrame>,
) -> Result<(), ()> {
    while decoder.receive_frame(frame).is_ok() {
        if stop.load(Ordering::Relaxed) {
            return Err(());
        }
        let pts = frame.pts().unwrap_or(0) as f64 * video_tb;
        let Some(relative) = rebase_pts(pts, base, origin) else {
            continue;
        };
        // Hardware frames live in GPU memory: copy them down to system memory
        // (NV12) before the planes can be read. A failed copy drops just that
        // frame.
        let source = if is_hw_frame(frame) {
            match download_frame(frame) {
                Some(downloaded) => {
                    *sw_frame = downloaded;
                    &*sw_frame
                }
                None => continue,
            }
        } else {
            &*frame
        };
        send_video_frame(scaler, source, relative + offset, tx)?;
    }
    Ok(())
}

/// Drain every decoded audio frame and stream it to the sink as PCM. `Err`
/// means the session is stopping.
fn drain_audio(
    decoder: &mut ffmpeg::decoder::Audio,
    frame: &mut ffmpeg::frame::Audio,
    resampler: &mut Option<ffmpeg::software::resampling::Context>,
    audio_tb: f64,
    base: f64,
    stop: &AtomicBool,
    pcm_tx: &mpsc::SyncSender<f32>,
) -> Result<(), ()> {
    while decoder.receive_frame(frame).is_ok() {
        if stop.load(Ordering::Relaxed) {
            return Err(());
        }
        let pts = frame.pts().unwrap_or(0) as f64 * audio_tb;
        if !audio_reached_target(pts, base) {
            continue;
        }
        send_audio_frame(resampler, decoder, frame, pcm_tx)?;
    }
    Ok(())
}

/// Convert one decoded frame to planar I420 if needed and push it
/// (blocking on backpressure). The GPU does the color conversion, so the
/// CPU only moves luma and chroma planes (1.5 bytes per pixel).
fn send_video_frame(
    scaler: &mut Option<ffmpeg::software::scaling::Context>,
    frame: &ffmpeg::frame::Video,
    relative_secs: f64,
    tx: &mpsc::SyncSender<VideoFrame>,
) -> Result<(), ()> {
    use ffmpeg::format::Pixel;

    // I420 (software) and NV12 (hardware download) upload straight to the
    // GPU. Anything else (10-bit, 4:2:2, 4:4:4) converts once into I420,
    // far cheaper than the old full RGBA conversion and rare in practice.
    let mut converted = ffmpeg::frame::Video::empty();
    let (src, format) = match frame.format() {
        Pixel::YUV420P => (frame, YuvFormat::I420),
        Pixel::NV12 => (frame, YuvFormat::Nv12),
        _ => {
            if scaler.is_none() {
                *scaler = ffmpeg::software::scaling::Context::get(
                    frame.format(),
                    frame.width(),
                    frame.height(),
                    Pixel::YUV420P,
                    frame.width(),
                    frame.height(),
                    ffmpeg::software::scaling::Flags::BILINEAR,
                )
                .ok();
            }
            let Some(scaler) = scaler.as_mut() else {
                return Ok(());
            };
            if scaler.run(frame, &mut converted).is_err() {
                return Ok(());
            }
            (&converted, YuvFormat::I420)
        }
    };

    let width = src.width();
    let height = src.height();
    let chroma_width = width.div_ceil(2);
    let chroma_height = height.div_ceil(2);

    let y = copy_plane(src.data(0), src.stride(0), width as usize, height as usize);
    let (u, v) = match format {
        YuvFormat::I420 => (
            copy_plane(
                src.data(1),
                src.stride(1),
                chroma_width as usize,
                chroma_height as usize,
            ),
            copy_plane(
                src.data(2),
                src.stride(2),
                chroma_width as usize,
                chroma_height as usize,
            ),
        ),
        // NV12 keeps one interleaved UV plane: two bytes per chroma sample.
        YuvFormat::Nv12 => (
            copy_plane(
                src.data(1),
                src.stride(1),
                chroma_width as usize * 2,
                chroma_height as usize,
            ),
            Vec::new(),
        ),
    };

    let matrix = match src.color_space() {
        ffmpeg::util::color::Space::BT709 => YuvMatrix::Bt709,
        ffmpeg::util::color::Space::BT470BG | ffmpeg::util::color::Space::SMPTE170M => {
            YuvMatrix::Bt601
        }
        // Unspecified: HD and up is almost always BT.709, SD is BT.601.
        _ => {
            if height >= 720 {
                YuvMatrix::Bt709
            } else {
                YuvMatrix::Bt601
            }
        }
    };
    let range = match src.color_range() {
        ffmpeg::util::color::Range::JPEG => YuvRange::Full,
        _ => YuvRange::Limited,
    };

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);

    tx.send(VideoFrame {
        id,
        width,
        height,
        chroma_width,
        chroma_height,
        format,
        y,
        u,
        v,
        matrix,
        range,
        timestamp: Duration::from_secs_f64(relative_secs),
    })
    .map_err(|_| ())
}

/// Map a frame PTS onto the session timeline, or None to drop it. Frames
/// before the seek target drop. The first kept frame rebases to zero so
/// it shows at once.
fn rebase_pts(pts: f64, base: f64, origin: &mut Option<f64>) -> Option<f64> {
    if pts < base {
        return None;
    }
    Some(pts - *origin.get_or_insert(pts))
}

/// True once audio `pts` reaches the seek target `base`. Earlier audio is
/// the keyframe-rewind tail and is dropped to stay in sync.
fn audio_reached_target(pts: f64, base: f64) -> bool {
    pts >= base
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_frames_before_the_seek_target() {
        let mut origin = None;
        assert_eq!(rebase_pts(1.0, 2.0, &mut origin), None);
        // A dropped frame must not set the origin, or the first kept frame
        // would not land at zero.
        assert_eq!(origin, None);
    }

    #[test]
    fn first_kept_frame_is_time_zero() {
        let mut origin = None;
        assert_eq!(rebase_pts(2.5, 2.0, &mut origin), Some(0.0));
        assert_eq!(origin, Some(2.5));
    }

    #[test]
    fn later_frames_are_relative_to_the_first() {
        let mut origin = None;
        rebase_pts(2.5, 2.0, &mut origin);
        assert_eq!(rebase_pts(3.0, 2.0, &mut origin), Some(0.5));
        assert_eq!(rebase_pts(4.0, 2.0, &mut origin), Some(1.5));
    }

    #[test]
    fn without_a_seek_playback_starts_at_zero() {
        let mut origin = None;
        assert_eq!(rebase_pts(0.0, 0.0, &mut origin), Some(0.0));
        assert_eq!(rebase_pts(0.5, 0.0, &mut origin), Some(0.5));
    }

    #[test]
    fn audio_before_the_seek_target_is_dropped() {
        assert!(!audio_reached_target(1.0, 2.0));
    }

    #[test]
    fn audio_at_or_after_the_target_plays() {
        assert!(audio_reached_target(2.0, 2.0));
        assert!(audio_reached_target(2.5, 2.0));
    }

    #[test]
    fn without_a_seek_all_audio_plays() {
        assert!(audio_reached_target(0.0, 0.0));
    }

    #[test]
    fn stream_duration_is_zero_when_unknown() {
        assert_eq!(stream_duration_secs(1000, 0.001), 1.0);
        assert_eq!(stream_duration_secs(0, 0.001), 0.0);
        // AV_NOPTS_VALUE comes through as a large negative.
        assert_eq!(stream_duration_secs(i64::MIN, 0.001), 0.0);
    }

    #[test]
    fn loops_only_with_a_known_period() {
        assert!(should_loop(true, 5.0));
        assert!(!should_loop(false, 5.0));
        assert!(!should_loop(true, 0.0));
    }

    #[test]
    fn loop_offset_climbs_by_a_period_and_accounts_for_a_seek() {
        // From the start, each pass adds one full period.
        assert_eq!(loop_offset(5.0, 1, 0.0), Duration::from_secs(5));
        assert_eq!(loop_offset(5.0, 2, 0.0), Duration::from_secs(10));
        // A 2s seeked start shortens the first pass, so the offset trails by 2s.
        assert_eq!(loop_offset(5.0, 1, 2.0), Duration::from_secs(3));
    }

    #[test]
    fn a_looped_frame_keeps_climbing_by_the_offset() {
        // After a loop re-anchors (origin reset, base 0), the first frame
        // rebases to zero and the offset carries it forward with the clock.
        let mut origin = None;
        let relative = rebase_pts(0.0, 0.0, &mut origin).unwrap();
        let offset = 5.0;
        assert_eq!(relative + offset, 5.0);
    }
}
