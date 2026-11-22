//! Milestone 1 Part 2 — Stage A Console Audio Verification Tool (No Window, No Decks).
//!
//! What this tool tests before we connect live audio to `libprojectM`:
//! 1. Lists every active Windows audio output device (plus our Synthetic Test Signal
//!    using the exact same `AudioSource` trait) and lets you pick one.
//! 2. Prints the device's native format (sample rate, channels, sample type) and the
//!    normalized stereo `f32` output format we convert it to.
//! 3. Displays a live Peak & RMS level meter (~10x/second) while you play music.
//! 4. Verifies Windows WASAPI loopback **silence behavior** (proves the capture loop
//!    never hangs when nothing is playing and injects clean silence frames).
//! 5. Lets you test **Windows volume slider & Mute**, **default device switching**,
//!    and **unplugging a USB/Bluetooth audio device** without crashing.
//! 6. Press `Ctrl+C` to stop cleanly and save a full report to `logs/audio_stage_a.log`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use milkdrop_vj::audio::test_source::{TestAudioSource, TEST_SOURCE_DEVICE_ID};
use milkdrop_vj::audio::wasapi_loopback::WasapiLoopbackSource;
use milkdrop_vj::audio::AudioSource;

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "windows")]
extern "system" fn console_ctrl_handler(_ctrl_type: u32) -> i32 {
    STOP_REQUESTED.store(true, Ordering::SeqCst);
    1 // Handled cleanly so main() prints and flushes the summary before exit.
}

#[cfg(target_os = "windows")]
fn install_ctrl_c_handler() {
    extern "system" {
        fn SetConsoleCtrlHandler(
            handler: Option<extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
    }
    unsafe {
        SetConsoleCtrlHandler(Some(console_ctrl_handler), 1);
    }
}

#[cfg(not(target_os = "windows"))]
fn install_ctrl_c_handler() {}

fn open_log(path: &Path) -> File {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .expect("Failed to open logs/audio_stage_a.log")
}

fn log_line(file: &mut File, line: &str) {
    let _ = writeln!(file, "{line}");
    let _ = file.flush();
}

fn linear_to_dbfs(amp: f32) -> f32 {
    if amp <= 1e-6 {
        -96.0
    } else {
        20.0 * amp.log10()
    }
}

fn render_bar(amp: f32, width: usize) -> String {
    // Map -60 dB .. 0 dB onto 0..=width blocks for a readable DJ-style meter.
    let db = linear_to_dbfs(amp).clamp(-60.0, 0.0);
    let frac = (db + 60.0) / 60.0;
    let filled = (frac * width as f32).round() as usize;
    let filled = filled.min(width);
    format!("{}{}", "#".repeat(filled), "-".repeat(width - filled))
}

fn main() {
    install_ctrl_c_handler();

    let log_path = Path::new("logs/audio_stage_a.log");
    let mut log = open_log(log_path);
    log_line(&mut log, "=== Milestone 1 Part 2 — Stage A Console Audio Tool ===");

    let mut wasapi_src = WasapiLoopbackSource::new().expect("Failed to initialize WASAPI");
    let devices = wasapi_src
        .list_devices()
        .expect("Failed to list Windows playback devices");

    println!("=== Stage A: Windows WASAPI Loopback & AudioSource Console Tool ===");
    println!("Active Windows output devices (for loopback capture):");
    log_line(&mut log, "Enumerated active Windows output devices:");

    for (idx, dev) in devices.iter().enumerate() {
        let def_tag = if dev.is_default { " [DEFAULT]" } else { "" };
        let line = format!("  [{}] {}{}", idx + 1, dev.name, def_tag);
        println!("{line}");
        log_line(&mut log, &format!("{line} (id={})", dev.id));
    }

    let synth_option = devices.len() + 1;
    println!(
        "  [{synth_option}] Synthetic Test Signal (swappable AudioSource check — no real audio)"
    );

    print!("\nSelect a device number [1-{synth_option}] (press Enter for [1]): ");
    let _ = io::stdout().flush();

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .expect("Failed to read choice from terminal");
    let choice: usize = input.trim().parse().unwrap_or(1);

    // Rust concept: `Box<dyn AudioSource>` is a "trait object" — it holds ANY type
    // that implements `AudioSource` (either `WasapiLoopbackSource` or `TestAudioSource`),
    // proving the audio backend is truly swappable at runtime.
    let is_wasapi = choice != synth_option && !devices.is_empty();
    let mut source: Box<dyn AudioSource> = if !is_wasapi {
        let mut synth = TestAudioSource::new(48_000);
        synth
            .select_device(TEST_SOURCE_DEVICE_ID)
            .expect("Failed to select synthetic test source");
        Box::new(synth)
    } else {
        let picked_idx = choice.saturating_sub(1).min(devices.len() - 1);
        let picked = &devices[picked_idx];
        wasapi_src
            .select_device(&picked.id)
            .expect("Failed to select Windows output device");
        Box::new(wasapi_src)
    };

    let fmt = source.start().expect("Failed to start audio capture");

    let fmt_block = format!(
        "\n--- Active Audio Stream ---\n\
         Device:         {}\n\
         Capture mode:   {}\n\
         Native format:  {} Hz, {} channel(s), {}\n\
         Output format:  {} Hz, {} channels (stereo interleaved f32 in -1.0..=1.0)\n",
        fmt.device_name,
        fmt.capture_mode_label,
        fmt.native_sample_rate,
        fmt.native_channels,
        fmt.native_sample_type,
        fmt.output_sample_rate,
        fmt.output_channels
    );
    println!("{fmt_block}");
    log_line(&mut log, &fmt_block);

    println!("Suggested checks while this meter runs:");
    println!("  1. Play music -> watch Peak/RMS bars and pkts/s.");
    println!("  2. Pause music -> verify status switches to SILENCE (no hang, 0 pkts/s).");
    println!("  3. Move Windows volume slider & Mute -> watch whether Peak/RMS changes.");
    println!("  4. Switch Windows default output device or unplug USB/BT -> verify no crash.");
    println!("Press Ctrl+C to stop and save summary.\n");

    let mut last_default = WasapiLoopbackSource::query_current_default_render_device();
    let mut last_default_check = Instant::now();

    let mut pcm_buf = vec![0.0_f32; 512 * 2]; // 512 stereo frames per read (~10.6 ms @ 48 kHz)
    let start_time = Instant::now();
    let mut last_ui_draw = Instant::now();
    let mut last_log_snap = Instant::now();

    let mut window_peak_l = 0.0_f32;
    let mut window_peak_r = 0.0_f32;
    let mut window_sq_sum_l = 0.0_f64;
    let mut window_sq_sum_r = 0.0_f64;
    let mut window_frames = 0usize;
    let mut window_packets = 0usize;

    let mut total_reads = 0u64;
    let mut total_packets = 0u64;
    let mut silence_reads = 0u64;
    let mut live_reads = 0u64;
    let mut saw_silence_state = false;
    let mut device_lost_logged = false;
    let mut last_status_tag = String::new();

    while !STOP_REQUESTED.load(Ordering::Relaxed) {
        let res = match source.read_pcm_stereo(&mut pcm_buf) {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("\n[ERROR] read_pcm_stereo returned error: {e}");
                eprintln!("{msg}");
                log_line(&mut log, &msg);
                break;
            }
        };

        total_reads += 1;
        total_packets += res.packets_read as u64;
        window_packets += res.packets_read;

        if res.silence_injected {
            silence_reads += 1;
            saw_silence_state = true;
        } else {
            live_reads += 1;
        }

        if let Some(ref lost_err) = res.device_lost_error {
            if !device_lost_logged {
                device_lost_logged = true;
                let msg = format!(
                    "\n[EVENT @{:.1}s] DEVICE LOST (handled cleanly, no crash): {lost_err}",
                    start_time.elapsed().as_secs_f64()
                );
                println!("{msg}");
                log_line(&mut log, &msg);
            }
        }

        let n_frames = res.frames_written;
        for i in 0..n_frames {
            let l = pcm_buf[2 * i].abs();
            let r = pcm_buf[2 * i + 1].abs();
            if l > window_peak_l {
                window_peak_l = l;
            }
            if r > window_peak_r {
                window_peak_r = r;
            }
            window_sq_sum_l += (l as f64) * (l as f64);
            window_sq_sum_r += (r as f64) * (r as f64);
        }
        window_frames += n_frames;

        // If capturing from synthetic source, pace at ~60 Hz so the meter is readable.
        if !is_wasapi {
            std::thread::sleep(Duration::from_millis(16));
        }

        // Check once per second whether the Windows default output device changed.
        if is_wasapi && last_default_check.elapsed() >= Duration::from_secs(1) {
            last_default_check = Instant::now();
            let cur_default = WasapiLoopbackSource::query_current_default_render_device();
            if cur_default != last_default {
                let new_name = cur_default
                    .as_ref()
                    .map(|(_, n)| n.as_str())
                    .unwrap_or("<none>");
                let msg = format!(
                    "\n[EVENT @{:.1}s] Windows default output device changed -> `{new_name}` (still capturing `{}`)",
                    start_time.elapsed().as_secs_f64(),
                    fmt.device_name
                );
                println!("{msg}");
                log_line(&mut log, &msg);
                last_default = cur_default;
            }
        }

        // Refresh the live console meter every 100 ms (~10 FPS).
        let ui_dt = last_ui_draw.elapsed().as_secs_f64();
        if ui_dt >= 0.10 {
            let rms_l = if window_frames > 0 {
                (window_sq_sum_l / window_frames as f64).sqrt() as f32
            } else {
                0.0
            };
            let rms_r = if window_frames > 0 {
                (window_sq_sum_r / window_frames as f64).sqrt() as f32
            } else {
                0.0
            };
            let pkts_per_sec = window_packets as f64 / ui_dt;

            let status = if res.device_lost_error.is_some() {
                "DEVICE LOST   "
            } else if res.silence_injected {
                "SILENCE(0pkt) "
            } else if window_peak_l < 1e-5 && window_peak_r < 1e-5 {
                "LIVE(quiet)   "
            } else {
                "LIVE          "
            };

            if status.trim() != last_status_tag {
                let transition = format!(
                    "[STATE @{:.1}s] -> {} | PeakL={:.1}dB PeakR={:.1}dB | RMS={:.1}/{:.1}dB | pkts/s={:.0}",
                    start_time.elapsed().as_secs_f64(),
                    status.trim(),
                    linear_to_dbfs(window_peak_l),
                    linear_to_dbfs(window_peak_r),
                    linear_to_dbfs(rms_l),
                    linear_to_dbfs(rms_r),
                    pkts_per_sec
                );
                log_line(&mut log, &transition);
                last_status_tag = status.trim().to_string();
            }

            if last_log_snap.elapsed() >= Duration::from_secs(2) {
                last_log_snap = Instant::now();
                log_line(
                    &mut log,
                    &format!(
                        "[METER @{:.1}s] {} | PeakL={:.1}dB PeakR={:.1}dB | RMS_L={:.1}dB RMS_R={:.1}dB | pkts/s={:.0}",
                        start_time.elapsed().as_secs_f64(),
                        status.trim(),
                        linear_to_dbfs(window_peak_l),
                        linear_to_dbfs(window_peak_r),
                        linear_to_dbfs(rms_l),
                        linear_to_dbfs(rms_r),
                        pkts_per_sec
                    ),
                );
            }

            print!(
                "\r[{}] L:[{}] {:>5.1}dB | R:[{}] {:>5.1}dB | RMS:{:>5.1}/{:>5.1}dB | {:>4.0} pkt/s   ",
                status,
                render_bar(window_peak_l, 16),
                linear_to_dbfs(window_peak_l),
                render_bar(window_peak_r, 16),
                linear_to_dbfs(window_peak_r),
                linear_to_dbfs(rms_l),
                linear_to_dbfs(rms_r),
                pkts_per_sec
            );
            let _ = io::stdout().flush();

            window_peak_l = 0.0;
            window_peak_r = 0.0;
            window_sq_sum_l = 0.0;
            window_sq_sum_r = 0.0;
            window_frames = 0;
            window_packets = 0;
            last_ui_draw = Instant::now();
        }
    }

    source.stop();
    let elapsed = start_time.elapsed().as_secs_f64();
    let summary = format!(
        "\n\n=== Stage A Summary ===\n\
         Duration:              {:.2}s\n\
         Total reads:           {} (live={}, silence_injected={})\n\
         Total WASAPI packets:  {}\n\
         Silence observed:      {}\n\
         Device-lost handled:   {}\n\
         Log saved to:          {}",
        elapsed,
        total_reads,
        live_reads,
        silence_reads,
        total_packets,
        saw_silence_state,
        device_lost_logged,
        log_path.display()
    );
    println!("{summary}");
    log_line(&mut log, &summary);
}
