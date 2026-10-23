pub mod converter;

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};
use ironrdp_rdpsnd::server::{RdpsndServerHandler, RdpsndServerMessage};
use ironrdp_server::{ServerEvent, SoundServerFactory, ServerEventSender};
use macrdp_capture::AudioFrame;
use tokio::sync::mpsc;

/// Audio processing loop: receives AudioFrames from SCK, converts to S16LE,
/// and sends RdpsndServerMessage::Wave events.
async fn audio_loop(
    mut audio_rx: mpsc::Receiver<AudioFrame>,
    event_sender: tokio::sync::mpsc::UnboundedSender<ServerEvent>,
    frame_size_interleaved: usize,
    channels: u16,
    sample_rate: u32,
) {
    let mut ring_buffer: VecDeque<f32> = VecDeque::with_capacity(frame_size_interleaved * 4);
    let mut base_timestamp_ms: Option<u64> = None;
    let mut samples_sent: u64 = 0;

    tracing::info!(
        frame_size_interleaved,
        "Audio loop started ({}Hz, {}ch)",
        sample_rate,
        channels
    );

    while let Some(frame) = audio_rx.recv().await {
        if base_timestamp_ms.is_none() {
            base_timestamp_ms = Some(frame.timestamp_ms);
        }
        ring_buffer.extend(frame.data.iter());

        while ring_buffer.len() >= frame_size_interleaved {
            let chunk: Vec<f32> = ring_buffer.drain(..frame_size_interleaved).collect();
            let wave_data = converter::AudioConverter::float32_to_s16le(&chunk);

            let samples_per_ch = frame_size_interleaved / channels as usize;
            let offset_ms = (samples_sent * 1000) / sample_rate as u64;
            let ts = base_timestamp_ms.unwrap_or(0) + offset_ms;

            let _ = event_sender.send(ServerEvent::Rdpsnd(
                RdpsndServerMessage::Wave(wave_data, ts as u32),
            ));

            samples_sent += samples_per_ch as u64;
        }
    }
    tracing::info!("Audio loop ended");
}

/// Find best matching server format index given client's supported formats.
pub fn find_matching_format(
    server_formats: &[AudioFormat],
    client_formats: &[AudioFormat],
) -> Option<usize> {
    for (idx, server_fmt) in server_formats.iter().enumerate().rev() {
        for client_fmt in client_formats {
            if server_fmt.format == client_fmt.format
                && server_fmt.n_samples_per_sec == client_fmt.n_samples_per_sec
                && server_fmt.n_channels == client_fmt.n_channels
                && server_fmt.bits_per_sample == client_fmt.bits_per_sample
            {
                return Some(idx);
            }
        }
    }
    None
}

pub struct MacAudioHandler {
    formats: Vec<AudioFormat>,
    selected_format: Option<usize>,
    /// Published while the client has RDPSND open, so capture runs only then.
    tap: AudioTap,
    tx: mpsc::Sender<AudioFrame>,
}

impl fmt::Debug for MacAudioHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MacAudioHandler")
            .field("selected_format", &self.selected_format)
            .finish_non_exhaustive()
    }
}

impl MacAudioHandler {
    pub fn new(sample_rate: u32, channels: u16, tap: AudioTap, tx: mpsc::Sender<AudioFrame>) -> Self {
        let formats = vec![
            AudioFormat {
                format: WaveFormat::PCM,
                n_channels: channels,
                n_samples_per_sec: sample_rate,
                n_avg_bytes_per_sec: sample_rate * (channels as u32) * 2,
                n_block_align: channels * 2,
                bits_per_sample: 16,
                data: None,
            },
        ];

        Self {
            formats,
            selected_format: None,
            tap,
            tx,
        }
    }
}

impl RdpsndServerHandler for MacAudioHandler {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn start(&mut self, client_format: &ironrdp_rdpsnd::pdu::ClientAudioFormatPdu) -> Option<u16> {
        let idx = find_matching_format(&self.formats, &client_format.formats)?;
        self.selected_format = Some(idx);
        self.tap.send_replace(Some(self.tx.clone()));
        tracing::info!(format_idx = idx, "Audio started");
        Some(idx as u16)
    }

    fn stop(&mut self) {
        tracing::info!("Audio stopped");
        self.selected_format = None;
        // A replaced session may stop after the new one started: clear only our own sender.
        self.tap.send_if_modified(|cur| {
            let ours = cur.as_ref().is_some_and(|c| c.same_channel(&self.tx));
            if ours {
                *cur = None;
            }
            ours
        });
    }
}

/// Audio sender of the session whose client has RDPSND open; None while no client listens.
/// The display pipeline watches it and captures audio only while it holds a sender.
pub type AudioTap = Arc<tokio::sync::watch::Sender<Option<mpsc::Sender<AudioFrame>>>>;

/// Create an audio tap for use by both MacDisplay and MacAudioFactory.
pub fn new_audio_tap() -> AudioTap {
    Arc::new(tokio::sync::watch::channel(None).0)
}

/// Factory that creates MacAudioHandler instances.
/// Creates a fresh audio channel for each connection so the server survives reconnects.
pub struct MacAudioFactory {
    tap: AudioTap,
    event_sender: Option<tokio::sync::mpsc::UnboundedSender<ServerEvent>>,
    sample_rate: u32,
    channels: u16,
}

impl MacAudioFactory {
    pub fn new(tap: AudioTap, sample_rate: u32, channels: u16) -> Self {
        Self {
            tap,
            event_sender: None,
            sample_rate,
            channels,
        }
    }
}

impl SoundServerFactory for MacAudioFactory {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler> {
        let sender = self.event_sender.clone()
            .expect("event sender not set");

        let channels = self.channels;
        let sample_rate = self.sample_rate;
        // 20ms frame: sample_rate/50 samples per channel * channels
        let frame_size_interleaved = (sample_rate as usize / 50) * channels as usize;

        // A fresh channel per connection; the handler publishes it once the client opens RDPSND.
        let (tx, rx) = mpsc::channel::<AudioFrame>(32);

        // Ends when the handler and the capturer drop their senders.
        tokio::spawn(audio_loop(rx, sender, frame_size_interleaved, channels, sample_rate));

        Box::new(MacAudioHandler::new(sample_rate, channels, Arc::clone(&self.tap), tx))
    }
}

impl ServerEventSender for MacAudioFactory {
    fn set_sender(&mut self, sender: tokio::sync::mpsc::UnboundedSender<ServerEvent>) {
        self.event_sender = Some(sender);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm_48k_stereo() -> AudioFormat {
        AudioFormat {
            format: WaveFormat::PCM,
            n_channels: 2,
            n_samples_per_sec: 48000,
            n_avg_bytes_per_sec: 192000,
            n_block_align: 4,
            bits_per_sample: 16,
            data: None,
        }
    }

    #[test]
    fn match_format_pcm_found() {
        let server_formats = vec![pcm_48k_stereo()];
        let client_formats = vec![pcm_48k_stereo()];
        let result = find_matching_format(&server_formats, &client_formats);
        assert_eq!(result, Some(0));
    }

    fn client_formats(formats: Vec<AudioFormat>) -> ironrdp_rdpsnd::pdu::ClientAudioFormatPdu {
        ironrdp_rdpsnd::pdu::ClientAudioFormatPdu {
            version: ironrdp_rdpsnd::pdu::Version::V8,
            flags: ironrdp_rdpsnd::pdu::AudioFormatFlags::ALIVE,
            formats,
            volume_left: 0xFFFF,
            volume_right: 0xFFFF,
            pitch: 0x10000,
            dgram_port: 0,
        }
    }

    fn handler(tap: &AudioTap) -> (MacAudioHandler, mpsc::Receiver<AudioFrame>) {
        let (tx, rx) = mpsc::channel(1);
        (MacAudioHandler::new(48000, 2, Arc::clone(tap), tx), rx)
    }

    #[test]
    fn capture_stays_off_until_the_client_opens_rdpsnd() {
        let tap = new_audio_tap();
        let (mut h, _rx) = handler(&tap);
        assert!(tap.borrow().is_none());
        assert_eq!(h.start(&client_formats(vec![pcm_48k_stereo()])), Some(0));
        assert!(tap.borrow().is_some());
        h.stop();
        assert!(tap.borrow().is_none());
    }

    #[test]
    fn no_common_format_keeps_capture_off() {
        let tap = new_audio_tap();
        let (mut h, _rx) = handler(&tap);
        let mut mono = pcm_48k_stereo();
        mono.n_channels = 1;
        assert_eq!(h.start(&client_formats(vec![mono])), None);
        assert!(tap.borrow().is_none());
    }

    #[test]
    fn replaced_session_stopping_late_keeps_the_new_session_audio() {
        let tap = new_audio_tap();
        let (mut old, _old_rx) = handler(&tap);
        let (mut new, _new_rx) = handler(&tap);
        old.start(&client_formats(vec![pcm_48k_stereo()]));
        new.start(&client_formats(vec![pcm_48k_stereo()]));
        old.stop();
        assert!(tap.borrow().is_some(), "the old session must not turn off the new one's audio");
        new.stop();
        assert!(tap.borrow().is_none());
    }

    #[test]
    fn match_format_no_match() {
        let server_formats = vec![pcm_48k_stereo()];
        let client_formats = vec![AudioFormat {
            format: WaveFormat::PCM,
            n_channels: 1,
            n_samples_per_sec: 44100,
            n_avg_bytes_per_sec: 88200,
            n_block_align: 2,
            bits_per_sample: 16,
            data: None,
        }];
        let result = find_matching_format(&server_formats, &client_formats);
        assert_eq!(result, None);
    }
}
