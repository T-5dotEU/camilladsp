extern crate alsa;
extern crate nix;
use alsa::ctl::{ElemId, ElemIface};
use alsa::ctl::{ElemType, ElemValue};
use alsa::hctl::HCtl;
use alsa::pcm::{Access, Format, Frames, HwParams, State};
use alsa::{Direction, ValueOr};
use audiodevice::*;
use config;
use config::SampleFormat;
use conversions::{buffer_to_chunk_rawbytes, chunk_to_buffer_rawbytes};
use countertimer;
use nix::errno::Errno;
use rubato::VecResampler;
use std::ffi::CString;
use std::fmt::Debug;
use std::sync::mpsc;
use std::sync::{Arc, Barrier, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::{CaptureStatus, PlaybackStatus};
use CommandMessage;
use PrcFmt;
use ProcessingState;
use Res;
use StatusMessage;

const STANDARD_RATES: [u32; 17] = [
    5512, 8000, 11025, 16000, 22050, 32000, 44100, 48000, 64000, 88200, 96000, 176400, 192000,
    352800, 384000, 705600, 768000,
];

#[derive(Debug)]
enum SupportedValues {
    Range(u32, u32),
    Discrete(Vec<u32>),
}

pub struct AlsaPlaybackDevice {
    pub devname: String,
    pub samplerate: usize,
    pub chunksize: usize,
    pub channels: usize,
    pub sample_format: SampleFormat,
    pub target_level: usize,
    pub adjust_period: f32,
    pub enable_rate_adjust: bool,
}

pub struct AlsaCaptureDevice {
    pub devname: String,
    pub samplerate: usize,
    pub enable_resampling: bool,
    pub capture_samplerate: usize,
    pub resampler_conf: config::Resampler,
    pub chunksize: usize,
    pub channels: usize,
    pub sample_format: SampleFormat,
    pub silence_threshold: PrcFmt,
    pub silence_timeout: PrcFmt,
    pub retry_on_error: bool,
    pub avoid_blocking_read: bool,
    pub stop_on_rate_change: bool,
    pub rate_measure_interval: f32,
}

struct CaptureChannels {
    audio: mpsc::SyncSender<AudioMessage>,
    status: mpsc::Sender<StatusMessage>,
    command: mpsc::Receiver<CommandMessage>,
}

struct PlaybackChannels {
    audio: mpsc::Receiver<AudioMessage>,
    status: mpsc::Sender<StatusMessage>,
}

struct CaptureParams {
    channels: usize,
    sample_format: SampleFormat,
    silence_timeout: PrcFmt,
    silence_threshold: PrcFmt,
    chunksize: usize,
    store_bytes_per_sample: usize,
    samplerate: usize,
    capture_samplerate: usize,
    async_src: bool,
    capture_status: Arc<RwLock<CaptureStatus>>,
    retry_on_error: bool,
    avoid_blocking_read: bool,
    stop_on_rate_change: bool,
    rate_measure_interval: f32,
}

struct PlaybackParams {
    target_level: usize,
    adjust_period: f32,
    adjust_enabled: bool,
    sample_format: SampleFormat,
    playback_status: Arc<RwLock<PlaybackStatus>>,
}

enum CaptureResult {
    Normal,
    RecoverableError,
}

/// Play a buffer.
fn play_buffer(
    buffer: &[u8],
    pcmdevice: &alsa::PCM,
    io: &alsa::pcm::IO<u8>,
    target_delay: u64,
) -> Res<()> {
    let playback_state = pcmdevice.state();
    //trace!("Playback state {:?}", playback_state);
    if playback_state == State::XRun {
        warn!("Prepare playback after buffer underrun");
        pcmdevice.prepare()?;
        thread::sleep(Duration::from_millis(target_delay));
    } else if playback_state == State::Prepared {
        info!("Starting playback from Prepared state");
        thread::sleep(Duration::from_millis(target_delay));
    }
    let _frames = match io.writei(buffer) {
        Ok(frames) => frames,
        Err(err) => {
            warn!("Retrying playback, error: {}", err);
            pcmdevice.prepare()?;
            thread::sleep(Duration::from_millis(target_delay));
            io.writei(buffer)?
        }
    };
    Ok(())
}

/// Capture a buffer.
fn capture_buffer(
    buffer: &mut [u8],
    pcmdevice: &alsa::PCM,
    io: &alsa::pcm::IO<u8>,
    retry: bool,
    avoid_blocking: bool,
    samplerate: usize,
    frames_to_read: usize,
) -> Res<CaptureResult> {
    let capture_state = pcmdevice.state();
    if capture_state == State::XRun {
        warn!("Prepare capture device");
        pcmdevice.prepare()?;
    } else if capture_state != State::Running {
        debug!("Starting capture");
        pcmdevice.start()?;
    }
    if avoid_blocking {
        let available = pcmdevice.avail();
        match available {
            Ok(frames) => {
                if (frames as usize) < frames_to_read {
                    trace!(
                        "Not enough frames available: {}, need: {}, waiting...",
                        frames,
                        frames_to_read
                    );
                    // Let's wait for more frames, with 10% plus 1 ms of margin
                    let millis =
                        (1 + (1100 * (frames_to_read - frames as usize)) / samplerate) as u64;
                    let start = Instant::now();
                    thread::sleep(Duration::from_millis(millis));
                    let slept_millis = start.elapsed().as_millis();
                    trace!(
                        "Requested sleep for {} ms, result was {} ms",
                        millis,
                        slept_millis
                    );
                    let frames_after_wait = pcmdevice.avail().unwrap_or(0) as usize;
                    if frames_after_wait < frames_to_read {
                        // Still not enough,
                        warn!("Still not enough frames available: {}, need: {}. Capture timed out, will try again", frames_after_wait, frames_to_read);
                        return Ok(CaptureResult::RecoverableError);
                    }
                }
            }
            Err(err) => {
                if retry {
                    warn!("Capture failed while querying for available frames, error: {}, will try again.", err);
                    thread::sleep(Duration::from_millis(
                        (1000 * frames_to_read as u64) / samplerate as u64,
                    ));
                    return Ok(CaptureResult::RecoverableError);
                } else {
                    warn!(
                        "Capture failed while querying for available frames, error: {}",
                        err
                    );
                    return Err(Box::new(err));
                }
            }
        }
    }
    let _frames = match io.readi(buffer) {
        Ok(frames) => frames,
        Err(err) => match err.nix_error() {
            nix::Error::Sys(Errno::EIO) => {
                if retry {
                    warn!("Capture failed with error: {}, will try again.", err);
                    return Ok(CaptureResult::RecoverableError);
                } else {
                    warn!("Capture failed with error: {}", err);
                    return Err(Box::new(err));
                }
            }
            // TODO: do we need separate handling of xruns that happen in the tiny
            // window between state() and readi()?
            nix::Error::Sys(Errno::EPIPE) => {
                if retry {
                    warn!("Retrying capture, error: {}", err);
                    pcmdevice.prepare()?;
                    io.readi(buffer)?
                } else {
                    warn!("Capture failed, error: {}", err);
                    return Err(Box::new(err));
                }
            }
            _ => {
                warn!("Capture failed, error: {}", err);
                return Err(Box::new(err));
            }
        },
    };
    Ok(CaptureResult::Normal)
}

fn list_samplerates(hwp: &HwParams) -> Res<SupportedValues> {
    let min_rate = hwp.get_rate_min()?;
    let max_rate = hwp.get_rate_max()?;
    if min_rate == max_rate {
        // Only one rate is supported.
        return Ok(SupportedValues::Discrete(vec![min_rate]));
    } else if hwp.test_rate(min_rate + 1).is_ok() {
        // If min_rate + 1 is sipported, then this must be a range.
        return Ok(SupportedValues::Range(min_rate, max_rate));
    }
    let mut rates = Vec::new();
    // Loop through and test all the standard rates.
    for rate in STANDARD_RATES.iter() {
        if hwp.test_rate(*rate).is_ok() {
            rates.push(*rate);
        }
    }
    Ok(SupportedValues::Discrete(rates))
}

fn list_samplerates_as_text(hwp: &HwParams) -> String {
    let supported_rates_res = list_samplerates(hwp);
    if let Ok(rates) = supported_rates_res {
        format!("supported samplerates: {:?}", rates)
    } else {
        "failed checking supported samplerates".to_string()
    }
}

fn list_nbr_channels(hwp: &HwParams) -> Res<(u32, u32, Vec<u32>)> {
    let min_channels = hwp.get_channels_min()?;
    let max_channels = hwp.get_channels_max()?;
    if min_channels == max_channels {
        return Ok((min_channels, max_channels, vec![min_channels]));
    }
    let mut channels = Vec::new();

    let mut check_max = max_channels;
    if check_max > 32 {
        check_max = 32;
    }
    for chan in min_channels..(check_max + 1) {
        if hwp.test_channels(chan).is_ok() {
            channels.push(chan);
        }
    }
    Ok((min_channels, max_channels, channels))
}

fn list_channels_as_text(hwp: &HwParams) -> String {
    let supported_channels_res = list_nbr_channels(hwp);
    if let Ok((min_ch, max_ch, ch_list)) = supported_channels_res {
        format!(
            "supported channels, min: {}, max: {}, list: {:?}",
            min_ch, max_ch, ch_list
        )
    } else {
        "failed checking supported channels".to_string()
    }
}

fn list_formats(hwp: &HwParams) -> Res<Vec<SampleFormat>> {
    let mut formats = Vec::new();
    // Let's just check the formats supported by CamillaDSP
    if hwp.test_format(Format::s16()).is_ok() {
        formats.push(SampleFormat::S16LE);
    }
    if hwp.test_format(Format::s24()).is_ok() {
        formats.push(SampleFormat::S24LE);
    }
    if hwp.test_format(Format::S243LE).is_ok() {
        formats.push(SampleFormat::S24LE3);
    }
    if hwp.test_format(Format::s32()).is_ok() {
        formats.push(SampleFormat::S32LE);
    }
    if hwp.test_format(Format::float()).is_ok() {
        formats.push(SampleFormat::FLOAT32LE);
    }
    if hwp.test_format(Format::float64()).is_ok() {
        formats.push(SampleFormat::FLOAT64LE);
    }
    Ok(formats)
}

fn list_formats_as_text(hwp: &HwParams) -> String {
    let supported_formats_res = list_formats(hwp);
    if let Ok(formats) = supported_formats_res {
        format!("supported sample formats: {:?}", formats)
    } else {
        "failed checking supported sample formats".to_string()
    }
}

/// Open an Alsa PCM device
fn open_pcm(
    devname: String,
    samplerate: u32,
    bufsize: Frames,
    channels: u32,
    sample_format: &SampleFormat,
    capture: bool,
) -> Res<alsa::PCM> {
    // Open the device
    let pcmdev;
    if capture {
        pcmdev = alsa::PCM::new(&devname, Direction::Capture, false)?;
    } else {
        pcmdev = alsa::PCM::new(&devname, Direction::Playback, false)?;
    }
    // Set hardware parameters
    {
        let direction = if capture { "Capture" } else { "Playback" };
        let hwp = HwParams::any(&pcmdev)?;

        // Set number of channels
        debug!("{}: {}", direction, list_channels_as_text(&hwp));
        debug!("{}: setting channels to {}", direction, channels);
        hwp.set_channels(channels)?;

        // Set samplerate
        debug!("{}: {}", direction, list_samplerates_as_text(&hwp));
        debug!("{}: setting rate to {}", direction, samplerate);
        hwp.set_rate(samplerate, ValueOr::Nearest)?;

        // Set sample format
        debug!("{}: {}", direction, list_formats_as_text(&hwp));
        debug!("{}: setting format to {}", direction, sample_format);
        match sample_format {
            SampleFormat::S16LE => hwp.set_format(Format::s16())?,
            SampleFormat::S24LE => hwp.set_format(Format::s24())?,
            SampleFormat::S24LE3 => hwp.set_format(Format::S243LE)?,
            SampleFormat::S32LE => hwp.set_format(Format::s32())?,
            SampleFormat::FLOAT32LE => hwp.set_format(Format::float())?,
            SampleFormat::FLOAT64LE => hwp.set_format(Format::float64())?,
        }

        // Set access mode, buffersize and periods
        hwp.set_access(Access::RWInterleaved)?;
        let _bufsize = hwp.set_buffer_size_near(2 * bufsize)?;
        let _period = hwp.set_period_size_near(bufsize / 4, alsa::ValueOr::Nearest)?;

        // Apply
        pcmdev.hw_params(&hwp)?;
    }

    // Set software parameters
    let (_rate, _act_bufsize) = {
        let hwp = pcmdev.hw_params_current()?;
        let swp = pcmdev.sw_params_current()?;
        let (act_bufsize, act_periodsize) = (hwp.get_buffer_size()?, hwp.get_period_size()?);
        if capture {
            swp.set_start_threshold(0)?;
        } else {
            swp.set_start_threshold(act_bufsize / 2 - act_periodsize)?;
        }
        //swp.set_avail_min(periodsize)?;
        debug!(
            "Opening audio device \"{}\" with parameters: {:?}, {:?}",
            devname, hwp, swp
        );
        pcmdev.sw_params(&swp)?;
        debug!("Audio device \"{}\" successfully opened", devname);
        (hwp.get_rate()?, act_bufsize)
    };
    Ok(pcmdev)
}

fn playback_loop_bytes(
    channels: PlaybackChannels,
    mut buffer: Vec<u8>,
    pcmdevice: &alsa::PCM,
    io: alsa::pcm::IO<u8>,
    params: PlaybackParams,
) {
    let srate = pcmdevice.hw_params_current().unwrap().get_rate().unwrap();
    let mut timer = countertimer::Stopwatch::new();
    let mut chunk_stats;
    let mut buffer_avg = countertimer::Averager::new();
    let mut conversion_result;
    let adjust = params.adjust_period > 0.0 && params.adjust_enabled;
    let target_delay = 1000 * (params.target_level as u64) / srate as u64;
    loop {
        match channels.audio.recv() {
            Ok(AudioMessage::Audio(chunk)) => {
                conversion_result =
                    chunk_to_buffer_rawbytes(&chunk, &mut buffer, &params.sample_format);
                if conversion_result.1 > 0 {
                    params.playback_status.write().unwrap().clipped_samples += conversion_result.1;
                }
                if let Ok(status) = pcmdevice.status() {
                    buffer_avg.add_value(status.get_delay() as f64)
                }
                if timer.larger_than_millis((1000.0 * params.adjust_period) as u64) {
                    if let Some(av_delay) = buffer_avg.get_average() {
                        timer.restart();
                        buffer_avg.restart();
                        if adjust {
                            let speed = calculate_speed(
                                av_delay,
                                params.target_level,
                                params.adjust_period,
                                srate,
                            );
                            channels
                                .status
                                .send(StatusMessage::SetSpeed(speed))
                                .unwrap_or(());
                        }
                        let mut pb_stat = params.playback_status.write().unwrap();
                        pb_stat.buffer_level = av_delay as usize;
                        debug!(
                            "Playback buffer level: {}, signal rms: {:?}",
                            av_delay, pb_stat.signal_rms
                        );
                    }
                }

                chunk_stats = chunk.get_stats();
                params.playback_status.write().unwrap().signal_rms = chunk_stats.rms_db();
                params.playback_status.write().unwrap().signal_peak = chunk_stats.peak_db();

                let playback_res = play_buffer(&buffer, pcmdevice, &io, target_delay);
                match playback_res {
                    Ok(_) => {}
                    Err(msg) => {
                        channels
                            .status
                            .send(StatusMessage::PlaybackError(msg.to_string()))
                            .unwrap_or(());
                    }
                };
            }
            Ok(AudioMessage::EndOfStream) => {
                channels
                    .status
                    .send(StatusMessage::PlaybackDone)
                    .unwrap_or(());
                break;
            }
            Err(err) => {
                error!("Message channel error: {}", err);
                channels
                    .status
                    .send(StatusMessage::PlaybackError(err.to_string()))
                    .unwrap_or(());
                break;
            }
        }
    }
}

fn capture_loop_bytes(
    channels: CaptureChannels,
    mut buffer: Vec<u8>,
    pcmdevice: &alsa::PCM,
    io: alsa::pcm::IO<u8>,
    params: CaptureParams,
    mut resampler: Option<Box<dyn VecResampler<PrcFmt>>>,
) {
    let pcminfo = pcmdevice.info().unwrap();
    let card = pcminfo.get_card();
    let device = pcminfo.get_device();
    let subdevice = pcminfo.get_subdevice();
    let h = HCtl::new(&format!("hw:{}", card), false).unwrap();
    h.load().unwrap();

    let mut elid_loopback = ElemId::new(ElemIface::PCM);
    elid_loopback.set_device(device);
    elid_loopback.set_subdevice(subdevice);
    elid_loopback.set_name(&CString::new("PCM Rate Shift 100000").unwrap());
    let element_loopback = h.find_elem(&elid_loopback);

    let mut elid_uac2_gadget = ElemId::new(ElemIface::PCM);
    elid_uac2_gadget.set_device(device);
    elid_uac2_gadget.set_subdevice(subdevice);
    elid_uac2_gadget.set_name(&CString::new("Capture Pitch 1000000").unwrap());
    let element_uac2_gadget = h.find_elem(&elid_uac2_gadget);

    if element_loopback.is_some() || element_uac2_gadget.is_some() {
        info!("Capture device supports rate adjust");
        if params.samplerate == params.capture_samplerate && resampler.is_some() {
            warn!("Needless 1:1 sample rate conversion active. Not needed since capture device supports rate adjust");
        } else if params.async_src && resampler.is_some() {
            warn!("Async resampler not needed since capture device supports rate adjust. Switch to Sync type to save CPU time.");
        }
    }

    let mut capture_bytes = params.chunksize * params.channels * params.store_bytes_per_sample;
    let mut averager = countertimer::TimeAverage::new();
    let mut watcher_averager = countertimer::TimeAverage::new();
    let mut valuewatcher = countertimer::ValueWatcher::new(
        params.capture_samplerate as f32,
        RATE_CHANGE_THRESHOLD_VALUE,
        RATE_CHANGE_THRESHOLD_COUNT,
    );
    let rate_measure_interval_ms = (1000.0 * params.rate_measure_interval) as u64;
    let mut rate_adjust = 0.0;
    let mut silence_counter = countertimer::SilenceCounter::new(
        params.silence_threshold,
        params.silence_timeout,
        params.capture_samplerate,
        params.chunksize,
    );
    let mut state = ProcessingState::Running;
    let mut value_range = 0.0;
    let mut chunk_stats;
    let mut card_inactive = false;
    loop {
        match channels.command.try_recv() {
            Ok(CommandMessage::Exit) => {
                debug!("Exit message received, sending EndOfStream");
                let msg = AudioMessage::EndOfStream;
                channels.audio.send(msg).unwrap_or(());
                channels
                    .status
                    .send(StatusMessage::CaptureDone)
                    .unwrap_or(());
                break;
            }
            Ok(CommandMessage::SetSpeed { speed }) => {
                let mut elval = ElemValue::new(ElemType::Integer).unwrap();
                rate_adjust = speed;
                if let Some(elem_loopback) = &element_loopback {
                    elval.set_integer(0, (100_000.0 / speed) as i32).unwrap();
                    elem_loopback.write(&elval).unwrap();
                } else if let Some(elem_uac2_gadget) = &element_uac2_gadget {
                    elval.set_integer(0, (speed * 1_000_000.0) as i32).unwrap();
                    elem_uac2_gadget.write(&elval).unwrap();
                } else if let Some(resampl) = &mut resampler {
                    if params.async_src {
                        if resampl.set_resample_ratio_relative(speed).is_err() {
                            debug!("Failed to set resampling speed to {}", speed);
                        }
                    } else {
                        warn!("Requested rate adjust of synchronous resampler. Ignoring request.");
                    }
                }
            }
            Err(_) => {}
        };
        capture_bytes = get_nbr_capture_bytes(capture_bytes, &resampler, &params, &mut buffer);
        let capture_res = capture_buffer(
            &mut buffer[0..capture_bytes],
            pcmdevice,
            &io,
            params.retry_on_error,
            params.avoid_blocking_read,
            params.capture_samplerate,
            capture_bytes / (params.channels * params.store_bytes_per_sample),
        );
        match capture_res {
            Ok(CaptureResult::Normal) => {
                //trace!("Captured {} bytes", capture_bytes);
                averager.add_value(capture_bytes);
                if averager.larger_than_millis(
                    params.capture_status.read().unwrap().update_interval as u64,
                ) {
                    let bytes_per_sec = averager.get_average();
                    averager.restart();
                    let measured_rate_f =
                        bytes_per_sec / (params.channels * params.store_bytes_per_sample) as f64;
                    trace!("Measured sample rate is {} Hz", measured_rate_f);
                    let mut capt_stat = params.capture_status.write().unwrap();
                    capt_stat.measured_samplerate = measured_rate_f as usize;
                    capt_stat.signal_range = value_range as f32;
                    capt_stat.rate_adjust = rate_adjust as f32;
                    capt_stat.state = state;
                    card_inactive = false;
                }
                watcher_averager.add_value(capture_bytes);
                if watcher_averager.larger_than_millis(rate_measure_interval_ms) {
                    let bytes_per_sec = watcher_averager.get_average();
                    watcher_averager.restart();
                    let measured_rate_f =
                        bytes_per_sec / (params.channels * params.store_bytes_per_sample) as f64;
                    let changed = valuewatcher.check_value(measured_rate_f as f32);
                    if changed {
                        warn!(
                            "sample rate change detected, last rate was {} Hz",
                            measured_rate_f
                        );
                        if params.stop_on_rate_change {
                            let msg = AudioMessage::EndOfStream;
                            channels.audio.send(msg).unwrap_or(());
                            channels
                                .status
                                .send(StatusMessage::CaptureFormatChange(measured_rate_f as usize))
                                .unwrap_or(());
                            break;
                        }
                    }
                    trace!("Measured sample rate is {} Hz", measured_rate_f);
                }
            }
            Ok(CaptureResult::RecoverableError) => {
                card_inactive = true;
                params.capture_status.write().unwrap().state = ProcessingState::Paused;
                debug!("Card inactive, pausing");
            }
            Err(msg) => {
                channels
                    .status
                    .send(StatusMessage::CaptureError(msg.to_string()))
                    .unwrap_or(());
                let msg = AudioMessage::EndOfStream;
                channels.audio.send(msg).unwrap_or(());
                return;
            }
        };
        let mut chunk = buffer_to_chunk_rawbytes(
            &buffer[0..capture_bytes],
            params.channels,
            &params.sample_format,
            capture_bytes,
            &params.capture_status.read().unwrap().used_channels,
        );
        chunk_stats = chunk.get_stats();
        params.capture_status.write().unwrap().signal_rms = chunk_stats.rms_db();
        params.capture_status.write().unwrap().signal_peak = chunk_stats.peak_db();
        value_range = chunk.maxval - chunk.minval;
        if card_inactive {
            state = ProcessingState::Paused;
        } else {
            state = silence_counter.update(value_range);
        }
        if state == ProcessingState::Running {
            if let Some(resampl) = &mut resampler {
                let new_waves = resampl.process(&chunk.waveforms).unwrap();
                let mut chunk_frames = new_waves.iter().map(|w| w.len()).max().unwrap();
                if chunk_frames == 0 {
                    chunk_frames = params.chunksize;
                }
                chunk.frames = chunk_frames;
                chunk.valid_frames = chunk.frames;
                chunk.waveforms = new_waves;
            }
            let msg = AudioMessage::Audio(chunk);
            channels.audio.send(msg).unwrap_or(());
        }
    }
    let mut capt_stat = params.capture_status.write().unwrap();
    capt_stat.state = ProcessingState::Inactive;
}

fn get_nbr_capture_bytes(
    capture_bytes: usize,
    resampler: &Option<Box<dyn VecResampler<PrcFmt>>>,
    params: &CaptureParams,
    buf: &mut Vec<u8>,
) -> usize {
    let capture_bytes_new = if let Some(resampl) = &resampler {
        //trace!("Resampler needs {} frames", resampl.nbr_frames_needed());
        resampl.nbr_frames_needed() * params.channels * params.store_bytes_per_sample
    } else {
        capture_bytes
    };
    if capture_bytes_new > buf.len() {
        debug!("Capture buffer too small, extending");
        buf.append(&mut vec![0u8; capture_bytes_new - buf.len()]);
    }
    capture_bytes_new
}

/// Start a playback thread listening for AudioMessages via a channel.
impl PlaybackDevice for AlsaPlaybackDevice {
    fn start(
        &mut self,
        channel: mpsc::Receiver<AudioMessage>,
        barrier: Arc<Barrier>,
        status_channel: mpsc::Sender<StatusMessage>,
        playback_status: Arc<RwLock<PlaybackStatus>>,
    ) -> Res<Box<thread::JoinHandle<()>>> {
        let devname = self.devname.clone();
        let target_level = if self.target_level > 0 {
            self.target_level
        } else {
            self.chunksize
        };
        let adjust_period = self.adjust_period;
        let adjust_enabled = self.enable_rate_adjust;
        let samplerate = self.samplerate;
        let chunksize = self.chunksize;
        let channels = self.channels;
        let bytes_per_sample = self.sample_format.bytes_per_sample();
        let sample_format = self.sample_format.clone();
        let handle = thread::Builder::new()
            .name("AlsaPlayback".to_string())
            .spawn(move || {
                match open_pcm(
                    devname,
                    samplerate as u32,
                    chunksize as Frames,
                    channels as u32,
                    &sample_format,
                    false,
                ) {
                    Ok(pcmdevice) => {
                        match status_channel.send(StatusMessage::PlaybackReady) {
                            Ok(()) => {}
                            Err(_err) => {}
                        }

                        barrier.wait();
                        debug!("Starting playback loop");
                        let pb_params = PlaybackParams {
                            target_level,
                            adjust_period,
                            adjust_enabled,
                            sample_format,
                            playback_status,
                        };
                        let pb_channels = PlaybackChannels {
                            audio: channel,
                            status: status_channel,
                        };

                        let io = pcmdevice.io_bytes();
                        let buffer = vec![0u8; chunksize * channels * bytes_per_sample];
                        playback_loop_bytes(pb_channels, buffer, &pcmdevice, io, pb_params);
                    }
                    Err(err) => {
                        let send_result =
                            status_channel.send(StatusMessage::PlaybackError(err.to_string()));
                        if send_result.is_err() {
                            error!("Playback error: {}", err);
                        }
                        barrier.wait();
                    }
                }
            })
            .unwrap();
        Ok(Box::new(handle))
    }
}

/// Start a capture thread providing AudioMessages via a channel
impl CaptureDevice for AlsaCaptureDevice {
    fn start(
        &mut self,
        channel: mpsc::SyncSender<AudioMessage>,
        barrier: Arc<Barrier>,
        status_channel: mpsc::Sender<StatusMessage>,
        command_channel: mpsc::Receiver<CommandMessage>,
        capture_status: Arc<RwLock<CaptureStatus>>,
    ) -> Res<Box<thread::JoinHandle<()>>> {
        let devname = self.devname.clone();
        let samplerate = self.samplerate;
        let capture_samplerate = self.capture_samplerate;
        let chunksize = self.chunksize;
        let buffer_frames = 2.0f32.powf(
            (1.2 * capture_samplerate as f32 / samplerate as f32 * chunksize as f32)
                .log2()
                .ceil(),
        ) as usize;
        debug!("Buffer frames {}", buffer_frames);
        let channels = self.channels;
        let store_bytes_per_sample = self.sample_format.bytes_per_sample();
        let silence_timeout = self.silence_timeout;
        let silence_threshold = self.silence_threshold;
        let sample_format = self.sample_format.clone();
        let enable_resampling = self.enable_resampling;
        let resampler_conf = self.resampler_conf.clone();
        let async_src = resampler_is_async(&resampler_conf);
        let retry_on_error = self.retry_on_error;
        let avoid_blocking_read = self.avoid_blocking_read;
        let stop_on_rate_change = self.stop_on_rate_change;
        let rate_measure_interval = self.rate_measure_interval;
        let handle = thread::Builder::new()
            .name("AlsaCapture".to_string())
            .spawn(move || {
                let resampler = if enable_resampling {
                    debug!("Creating resampler");
                    get_resampler(
                        &resampler_conf,
                        channels,
                        samplerate,
                        capture_samplerate,
                        chunksize,
                    )
                } else {
                    None
                };
                match open_pcm(
                    devname,
                    capture_samplerate as u32,
                    buffer_frames as Frames,
                    channels as u32,
                    &sample_format,
                    true,
                ) {
                    Ok(pcmdevice) => {
                        match status_channel.send(StatusMessage::CaptureReady) {
                            Ok(()) => {}
                            Err(_err) => {}
                        }
                        barrier.wait();
                        debug!("Starting captureloop");
                        let cap_params = CaptureParams {
                            channels,
                            sample_format,
                            silence_timeout,
                            silence_threshold,
                            chunksize,
                            store_bytes_per_sample,
                            samplerate,
                            capture_samplerate,
                            async_src,
                            capture_status,
                            retry_on_error,
                            avoid_blocking_read,
                            stop_on_rate_change,
                            rate_measure_interval,
                        };
                        let cap_channels = CaptureChannels {
                            audio: channel,
                            status: status_channel,
                            command: command_channel,
                        };
                        let io = pcmdevice.io_bytes();
                        let buffer = vec![0u8; channels * buffer_frames * store_bytes_per_sample];
                        capture_loop_bytes(
                            cap_channels,
                            buffer,
                            &pcmdevice,
                            io,
                            cap_params,
                            resampler,
                        );
                    }
                    Err(err) => {
                        let send_result =
                            status_channel.send(StatusMessage::CaptureError(err.to_string()));
                        if send_result.is_err() {
                            error!("Capture error: {}", err);
                        }
                        barrier.wait();
                    }
                }
            })
            .unwrap();
        Ok(Box::new(handle))
    }
}
